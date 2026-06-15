//! Linux audio backend — a 1:1 port of Go's `internal/audio/audio_linux.go`
//! onto `gstreamer-rs`. Classic `playbin` (not playbin3 — we need `GstPlayFlags`)
//! with audio-only flags, the PAUSED-preroll → `ASYNC_DONE` → buffer-fill →
//! PLAYING promotion dance (the DASH "playing but silent" fix), a bus-polling
//! thread, and a 100ms position poll. The live-DASH path is served as static
//! DASH (S8 owns true live MPD); VOD is this slice.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use gstreamer as gst;
use gst::prelude::*;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::event::{Event, State};
use crate::player::Player;

/// The user-intent half of the player's state for the current track; gates which
/// pipeline state changes reach the UI. (Go `playbackPhase`.)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Prerolling,
    Playing,
    Paused,
}
impl Phase {
    /// Whether the intent is "audio should be (or become) audible".
    fn wants_playback(self) -> bool {
        matches!(self, Phase::Prerolling | Phase::Playing)
    }
}

/// Per-track player state, reset by `play`. The initial-fill fields are
/// deliberately orthogonal to `phase` — the buffering hold must survive phase
/// changes. (Go `trackState`.)
struct Track {
    phase: Phase,
    live: bool,
    prerolled: bool,
    fill_done: bool,
    low_buffer: bool,
    confirmed: bool,
    /// Stops the position-poll ticks after EOS until the next `play` (so a stale
    /// tick can't re-flip the UI to playing after the station has advanced).
    ended: bool,
    last_pos: i64, // ns; -1 = none yet
}

impl Track {
    fn reset(live: bool) -> Self {
        Track {
            phase: if live { Phase::Playing } else { Phase::Prerolling },
            live,
            prerolled: false,
            fill_done: live, // live tracks have no managed fill
            low_buffer: false,
            confirmed: false,
            ended: false,
            last_pos: -1,
        }
    }
}

/// Shared backend state (the bus thread + the `Player` methods both touch it).
struct Shared {
    playbin: gst::Element,
    name: String,
    tx: mpsc::Sender<Event>,
    track: Mutex<Track>,
    live_hint: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    /// Queues an event for the controller; drops if saturated (Go `send`).
    fn send(&self, ev: Event) {
        if self.tx.try_send(ev.clone()).is_err() {
            log::warn!("[audio] event queue full, dropping {}", ev.state.as_str());
        }
    }

    fn is_from_playbin(&self, msg: &gst::Message) -> bool {
        msg.src().map(|s| s.name()) == Some(self.name.as_str().into())
    }
}

/// The GStreamer `playbin` player.
pub struct LinuxPlayer {
    shared: Arc<Shared>,
    bus_thread: Mutex<Option<JoinHandle<()>>>,
}

static GST_INIT: std::sync::Once = std::sync::Once::new();

pub fn new_player(events: mpsc::Sender<Event>) -> Result<Box<dyn Player>> {
    let mut init_err: Option<String> = None;
    GST_INIT.call_once(|| {
        if let Err(e) = gst::init() {
            init_err = Some(e.to_string());
        }
    });
    if let Some(e) = init_err {
        return Err(Error::new(format!("gstreamer init: {e}")));
    }

    // Classic playbin (playbin3 dropped the GstPlayFlags "flags" property we rely
    // on for audio-only playback).
    let playbin = gst::ElementFactory::make("playbin")
        .name("radio-playbin")
        .build()
        .map_err(|e| Error::new(format!("create playbin (gstreamer1.0-plugins-base?): {e}")))?;

    // Audio-only: GST_PLAY_FLAG_AUDIO | GST_PLAY_FLAG_SOFT_VOLUME. The bar never
    // renders video, and decoding the stream's video track would need codecs not
    // always shipped. NOTE: no "buffer-duration" — on live streams a fixed large
    // target is never reached and the pipeline sits in buffering limbo forever.
    playbin.set_property_from_str("flags", "audio+soft-volume");

    let shared = Arc::new(Shared {
        name: "radio-playbin".to_string(),
        playbin,
        tx: events,
        track: Mutex::new(Track::reset(false)),
        live_hint: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    {
        let mut t = shared.track.lock();
        t.phase = Phase::Idle;
    }

    let bus_shared = shared.clone();
    let bus_thread = std::thread::Builder::new()
        .name("radio-gst-bus".into())
        .spawn(move || monitor_bus(bus_shared))
        .map_err(|e| Error::new(format!("spawn bus thread: {e}")))?;

    Ok(Box::new(LinuxPlayer {
        shared,
        bus_thread: Mutex::new(Some(bus_thread)),
    }))
}

impl Player for LinuxPlayer {
    fn play(&self, url: &str) -> Result<()> {
        let s = &self.shared;
        // Stop previous playback.
        let _ = s.playbin.set_state(gst::State::Ready);
        s.playbin.set_property("uri", url);

        // Preroll in PAUSED first (the gst-launch dance): going straight to
        // PLAYING races the demuxer's initial segment event — DASH segments keep
        // their live timestamps and without the segment mapping settled the sink
        // schedules them hours ahead → position advances, pure silence. The bus
        // thread promotes to PLAYING once preroll settles and the buffer fills.
        let ret = s
            .playbin
            .set_state(gst::State::Paused)
            .map_err(|_| Error::new("failed to set GStreamer state to PAUSED for preroll"))?;

        // True live sources preroll with NO_PREROLL and must never be paused for
        // buffering; they go to PLAYING immediately.
        let live = ret == gst::StateChangeSuccess::NoPreroll || s.live_hint.load(Ordering::SeqCst);
        if live {
            s.playbin
                .set_state(gst::State::Playing)
                .map_err(|_| Error::new("failed to set GStreamer state to PLAYING"))?;
        }
        *s.track.lock() = Track::reset(live);
        s.send(Event::state(State::Loading));
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        let s = &self.shared;
        s.playbin
            .set_state(gst::State::Playing)
            .map_err(|_| Error::new("failed to set GStreamer state to PLAYING"))?;
        {
            let mut t = s.track.lock();
            t.phase = Phase::Playing;
            t.ended = false;
        }
        s.send(Event::state(State::Playing));
        Ok(())
    }

    fn pause(&self) -> Result<()> {
        let s = &self.shared;
        {
            let t = s.track.lock();
            if !t.phase.wants_playback() {
                return Ok(());
            }
        }
        s.playbin
            .set_state(gst::State::Paused)
            .map_err(|_| Error::new("failed to set GStreamer state to PAUSED"))?;
        s.track.lock().phase = Phase::Paused;
        s.send(Event::state(State::Paused));
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let s = &self.shared;
        let _ = s.playbin.set_state(gst::State::Ready);
        s.track.lock().phase = Phase::Idle;
        s.send(Event::state(State::Idle));
        Ok(())
    }

    fn set_volume(&self, v: f64) -> Result<()> {
        // playbin volume is 0.0..=1.0 (clamped).
        self.shared.playbin.set_property("volume", v);
        Ok(())
    }

    fn seek(&self, seconds: f64) -> Result<()> {
        let pos = gst::ClockTime::from_nseconds((seconds.max(0.0) * 1_000_000_000.0) as u64);
        // Best-effort: livestreams / non-seekable sources simply ignore it.
        let _ = self.shared.playbin.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            pos,
        );
        Ok(())
    }

    fn set_live_hint(&self, live: bool) {
        self.shared.live_hint.store(live, Ordering::SeqCst);
    }

    fn close(&self) -> Result<()> {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.bus_thread.lock().take() {
            let _ = h.join();
        }
        let _ = self.shared.playbin.set_state(gst::State::Null);
        Ok(())
    }
}

impl Drop for LinuxPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.bus_thread.lock().take() {
            let _ = h.join();
        }
        let _ = self.shared.playbin.set_state(gst::State::Null);
    }
}

/// The single bus-polling loop: bus messages → `handle_message`, quiet ticks →
/// `poll_position`. (Go `monitorBus`.)
fn monitor_bus(shared: Arc<Shared>) {
    let Some(bus) = shared.playbin.bus() else {
        log::warn!("[audio] failed to get GStreamer bus");
        return;
    };
    let filter = [
        gst::MessageType::Error,
        gst::MessageType::Eos,
        gst::MessageType::StateChanged,
        gst::MessageType::Buffering,
        gst::MessageType::AsyncDone,
    ];
    while !shared.stop.load(Ordering::SeqCst) {
        match bus.timed_pop_filtered(gst::ClockTime::from_mseconds(100), &filter) {
            Some(msg) => handle_message(&shared, &msg),
            None => poll_position(&shared),
        }
    }
}

/// Reports StatePlaying from an advancing position (Go `confirmFromPosition`),
/// AND drives the seek timeline by emitting throttled position ticks while
/// playing — the controller down-samples these to ~2.5 progress events/sec.
fn poll_position(shared: &Arc<Shared>) {
    let (wants, ended) = {
        let t = shared.track.lock();
        (t.phase.wants_playback(), t.ended)
    };
    if !wants || ended {
        return;
    }
    let Some(pos) = shared.playbin.query_position::<gst::ClockTime>() else {
        return;
    };
    let pos_ns = pos.nseconds() as i64;
    if pos_ns <= 0 {
        return;
    }

    let emit = {
        let mut t = shared.track.lock();
        let advanced = t.last_pos >= 0 && pos_ns > t.last_pos;
        t.last_pos = pos_ns;
        if !t.confirmed {
            // Confirm playback the moment the position advances (live pipelines
            // can stream audio while the async PLAYING transition never settles).
            if advanced {
                t.confirmed = true;
                t.phase = Phase::Playing;
                true
            } else {
                false
            }
        } else {
            true
        }
    };
    if emit {
        let dur = shared
            .playbin
            .query_duration::<gst::ClockTime>()
            .map(|d| d.nseconds() as f64 / 1e9)
            .unwrap_or(0.0);
        let mut ev = Event::state(State::Playing);
        ev.position = pos_ns as f64 / 1e9;
        ev.duration = dur;
        shared.send(ev);
    }
}

/// Dispatches one GStreamer bus message (Go `handleBusMessage`).
fn handle_message(shared: &Arc<Shared>, msg: &gst::Message) {
    use gst::MessageView;
    match msg.view() {
        MessageView::Error(err) => {
            shared.send(Event::error("", err.error().to_string()));
        }
        MessageView::Eos(_) => {
            // Natural end → StateEnded (distinct from idle/paused) so the station
            // can auto-advance. Stop position ticks until the next play.
            shared.track.lock().ended = true;
            shared.send(Event::state(State::Ended));
        }
        MessageView::AsyncDone(_) => {
            if shared.is_from_playbin(msg) {
                handle_async_done(shared);
            }
        }
        MessageView::Buffering(b) => {
            handle_buffering(shared, b.percent());
        }
        MessageView::StateChanged(sc) => {
            if shared.is_from_playbin(msg) {
                handle_state_changed(shared, sc.current(), sc.pending());
            }
        }
        _ => {}
    }
}

/// Preroll settled → promote to PLAYING unless held by the initial fill. The
/// set_state runs under the lock so check-and-promote is atomic against a
/// concurrent pause. (Go `handleAsyncDone`.)
fn handle_async_done(shared: &Arc<Shared>) {
    let mut t = shared.track.lock();
    t.prerolled = true;
    let promote = !t.fill_done && !t.low_buffer && t.phase.wants_playback();
    if promote {
        t.fill_done = true;
        let _ = shared.playbin.set_state(gst::State::Playing);
    }
}

/// Manages the PAUSED hold during the initial buffer fill (Go `handleBuffering`).
/// The hold applies ONLY during the initial fill (`!fill_done`): once promoted,
/// re-pausing on every sub-100% dip turns jitter into audible stutter. Keyed on
/// `fill_done`, NOT phase, so a pause/resume mid-fill can't cancel the hold.
fn handle_buffering(shared: &Arc<Shared>, percent: i32) {
    let mut t = shared.track.lock();
    let managed = !t.live && !t.fill_done;
    if managed {
        t.low_buffer = percent < 100;
        if percent < 100 {
            let _ = shared.playbin.set_state(gst::State::Paused);
        } else if t.prerolled && t.phase.wants_playback() {
            t.fill_done = true;
            let _ = shared.playbin.set_state(gst::State::Playing);
        }
    }
}

/// Turns settled playbin state changes into UI events. Phase gates each emit so
/// transitional preroll pauses + the READY bounce in `play` don't flicker the
/// UI; while a mid-fill dip holds the pipeline, every report is suppressed.
/// (Go `handleStateChanged`.)
fn handle_state_changed(shared: &Arc<Shared>, new_state: gst::State, pending: gst::State) {
    let settled = pending == gst::State::VoidPending;
    let ev = {
        let mut t = shared.track.lock();
        if t.low_buffer {
            None
        } else {
            match new_state {
                gst::State::Playing => {
                    // Reported even mid-transition: live pipelines stream audio
                    // while the async change never completes.
                    if t.phase.wants_playback() {
                        t.phase = Phase::Playing;
                        t.confirmed = true;
                        Some(Event::state(State::Playing))
                    } else {
                        None
                    }
                }
                gst::State::Paused => {
                    if settled && t.phase == Phase::Paused {
                        Some(Event::state(State::Paused))
                    } else {
                        None
                    }
                }
                gst::State::Ready | gst::State::Null => {
                    if settled && t.phase == Phase::Idle {
                        Some(Event::state(State::Idle))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    };
    if let Some(ev) = ev {
        shared.send(ev);
    }
}
