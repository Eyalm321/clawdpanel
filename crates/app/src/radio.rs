//! Radio wiring (S7, #54). Builds the [`RadioEngine`] (only when `Features.radio`
//! is on), wires the `RadioBridge` Slint callbacks to it, and pushes engine
//! events onto the bridge via `upgrade_in_event_loop` — the Rust port of the
//! Wails `radio:state` consumer + the `main.js:380-739` radio segment.
//!
//! Threading: the callbacks run on the Slint event loop (so they touch `cfg` /
//! the bridge directly); the engine's `emit` runs on the radio runtime threads
//! and marshals each event back onto the UI thread, filtered to the active
//! station (the engine stamps each event with the station index it's playing).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use slint::ComponentHandle;

use clawdpanel_claude_core::config;
use clawdpanel_media::{has_multiple_tracks, EmitFn, Event, RadioEngine, State};
use clawdpanel_ui::{BarWindow, RadioBridge};

use crate::settings::UiState;

/// Builds the radio engine and wires the bridge. Returns the engine to keep it
/// alive for the session, or `None` if radio is disabled / init fails.
pub fn setup(w: &BarWindow, ui: &Rc<UiState>) -> Option<Rc<RadioEngine>> {
    let (stations, volume, active) = {
        let c = ui.cfg.borrow();
        (c.stations.clone(), c.radio_volume, c.active_station)
    };

    // The active-station index the emit filter reads (events for a station the
    // user has cycled away from are dropped, matching the JS stationIdx filter).
    let active_atomic = Arc::new(AtomicI32::new(active));

    let emit: EmitFn = {
        let weak = w.as_weak();
        let filter = active_atomic.clone();
        Arc::new(move |ev: Event| {
            if filter.load(Ordering::Relaxed) != ev.station_idx {
                return;
            }
            let _ = weak.upgrade_in_event_loop(move |bar| apply_event(&bar, &ev));
        })
    };

    let engine = match RadioEngine::new(stations, volume, emit) {
        Ok(e) => Rc::new(e),
        Err(e) => {
            eprintln!("[radio] engine init failed: {e}");
            return None;
        }
    };

    let vol_pct = Rc::new(Cell::new(((volume * 100.0).round() as i32).clamp(0, 200)));
    init_bridge(w, ui, vol_pct.get());
    wire_callbacks(w, ui, engine.clone(), active_atomic, vol_pct);
    Some(engine)
}

/// Maps one engine event onto the bridge (the `radio:state` handler). Progress
/// ticks move the seek timeline; transitions drive the play/pause status.
fn apply_event(bar: &BarWindow, ev: &Event) {
    let rb = bar.global::<RadioBridge>();
    if ev.progress {
        rb.set_pos(ev.position as f32);
        rb.set_dur(ev.duration as f32);
        return;
    }
    match ev.state {
        State::Loading => {
            rb.set_status("load".into());
            // New track: zero the scrubber until the first progress tick.
            rb.set_pos(0.0);
            rb.set_dur(0.0);
        }
        State::Playing => rb.set_status("on".into()),
        State::Paused | State::Idle => rb.set_status("off".into()),
        State::Error => rb.set_status("err".into()),
        // Transient: a track finished and the engine is advancing — keep "on".
        State::Ended => {}
    }
}

/// Seeds the bridge from config (station name, cycler/track-nav gates, shuffle,
/// volume); status starts off.
fn init_bridge(w: &BarWindow, ui: &Rc<UiState>, vol_pct: i32) {
    let rb = w.global::<RadioBridge>();
    let cfg = ui.cfg.borrow();
    refresh_station_bridge(&rb, &cfg, cfg.active_station);
    rb.set_volume_pct(vol_pct);
    rb.set_status("off".into());
    rb.set_pos(0.0);
    rb.set_dur(0.0);
    rb.set_timeline_open(false);
}

/// Pushes the per-station bridge props for `idx` (name, shuffle, cycler count,
/// track-nav gate — all config-derived).
fn refresh_station_bridge(rb: &RadioBridge, cfg: &clawdpanel_types::Config, idx: i32) {
    rb.set_stations_count(cfg.stations.len() as i32);
    let st = (idx >= 0).then(|| cfg.stations.get(idx as usize)).flatten();
    match st {
        Some(s) => {
            rb.set_station_name(if s.name.is_empty() { "---".into() } else { s.name.clone().into() });
            rb.set_shuffle_on(s.shuffle);
            rb.set_track_nav_active(has_multiple_tracks(s));
        }
        None => {
            rb.set_station_name("---".into());
            rb.set_shuffle_on(false);
            rb.set_track_nav_active(false);
        }
    }
}

fn wire_callbacks(
    w: &BarWindow,
    ui: &Rc<UiState>,
    engine: Rc<RadioEngine>,
    active_atomic: Arc<AtomicI32>,
    vol_pct: Rc<Cell<i32>>,
) {
    let rb = w.global::<RadioBridge>();

    // play / pause: pause when playing, else (re)start the active station.
    {
        let weak = w.as_weak();
        let ui = ui.clone();
        let engine = engine.clone();
        rb.on_play_pause(move || {
            let Some(bar) = weak.upgrade() else { return };
            let rb = bar.global::<RadioBridge>();
            if rb.get_status().as_str() == "on" {
                let _ = engine.pause();
            } else {
                let active = ui.cfg.borrow().active_station;
                if ui.cfg.borrow().stations.is_empty() {
                    return;
                }
                rb.set_status("load".into());
                let _ = engine.play_station(active);
            }
        });
    }

    // station cycler: persist the new active station, repaint the bridge, and
    // (re)start it if we were already playing.
    {
        let weak = w.as_weak();
        let ui = ui.clone();
        let engine = engine.clone();
        let active_atomic = active_atomic.clone();
        rb.on_next_station(move |dir| {
            let Some(bar) = weak.upgrade() else { return };
            let rb = bar.global::<RadioBridge>();
            let n = ui.cfg.borrow().stations.len() as i32;
            if n < 2 {
                return;
            }
            let was_playing = matches!(rb.get_status().as_str(), "on" | "load");
            let active = {
                let mut c = ui.cfg.borrow_mut();
                let a = (c.active_station + dir).rem_euclid(n);
                c.active_station = a;
                let _ = config::save(&c);
                a
            };
            active_atomic.store(active, Ordering::Relaxed);
            refresh_station_bridge(&rb, &ui.cfg.borrow(), active);
            rb.set_timeline_open(false);
            rb.set_pos(0.0);
            rb.set_dur(0.0);
            if was_playing {
                rb.set_status("load".into());
                let _ = engine.play_station(active);
            } else {
                rb.set_status("off".into());
            }
        });
    }

    // track skip (the chips are grayed when the station can't step).
    {
        let engine = engine.clone();
        rb.on_track_next(move || {
            let _ = engine.next();
        });
    }
    {
        let engine = engine.clone();
        rb.on_track_prev(move || {
            let _ = engine.prev();
        });
    }

    // shuffle: pure mode toggle (persist + apply; never starts/jumps playback).
    {
        let weak = w.as_weak();
        let ui = ui.clone();
        let engine = engine.clone();
        rb.on_toggle_shuffle(move || {
            let active = ui.cfg.borrow().active_station;
            let next = {
                let mut c = ui.cfg.borrow_mut();
                let Some(s) = c.stations.get_mut(active.max(0) as usize) else {
                    return;
                };
                s.shuffle = !s.shuffle;
                let nx = s.shuffle;
                let _ = config::save(&c);
                nx
            };
            engine.set_stations(ui.cfg.borrow().stations.clone());
            let _ = engine.set_shuffle(active, next);
            if let Some(bar) = weak.upgrade() {
                bar.global::<RadioBridge>().set_shuffle_on(next);
            }
        });
    }

    // seek: the bridge sends a 0..1 fraction; multiply by the known duration.
    {
        let weak = w.as_weak();
        let engine = engine.clone();
        rb.on_seek(move |frac| {
            let Some(bar) = weak.upgrade() else { return };
            let dur = bar.global::<RadioBridge>().get_dur();
            if dur > 0.0 {
                let _ = engine.seek((frac as f64) * (dur as f64));
            }
        });
    }

    // volume: cycle −10% wrapping 0 ↔ 200 (the bar shows 0–200%; the player
    // clamps to 1.0). Persist so it survives restarts.
    {
        let weak = w.as_weak();
        let ui = ui.clone();
        let engine = engine.clone();
        rb.on_cycle_volume(move || {
            let cur = vol_pct.get();
            let next = if cur - 10 < 0 {
                if cur == 0 {
                    200
                } else {
                    0
                }
            } else {
                cur - 10
            };
            vol_pct.set(next);
            let v = next as f64 / 100.0;
            let _ = engine.set_volume(v);
            {
                let mut c = ui.cfg.borrow_mut();
                c.radio_volume = v;
                let _ = config::save(&c);
            }
            if let Some(bar) = weak.upgrade() {
                bar.global::<RadioBridge>().set_volume_pct(next);
            }
        });
    }
}
