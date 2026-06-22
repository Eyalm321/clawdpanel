//! [`RadioEngine`] — the top-level bundle the app holds: an owned multi-thread
//! tokio runtime driving the [`YtdlResolver`] (+ its local proxy), the
//! single-track [`Controller`] (real native player), and the [`StationPlayer`]
//! queue engine, wired together. Mirrors Go's `app.go` `initAudio`: build the
//! controller, route its events through the station, forward to the UI.

use std::sync::Arc;

use tokio::runtime::Runtime;

use crate::controller::Controller;
use crate::error::{Error, Result};
use crate::event::EmitFn;
use crate::resolver::{PlaylistExpander, StreamResolver};
use crate::station::StationPlayer;
use crate::ytdl::YtdlResolver;
use clawdpanel_types::StationConfig;

/// Owns the whole radio playback stack for the session. Dropping it shuts the
/// runtime (and the gstreamer bus thread) down.
pub struct RadioEngine {
    // Field order matters for drop: station/ctrl before the runtime so their
    // tasks aren't aborted mid-teardown, then the runtime, then the resolver.
    station: Arc<StationPlayer>,
    ctrl: Controller,
    #[allow(dead_code)]
    resolver: Arc<YtdlResolver>,
    /// Held for the session; dropping it shuts down the runtime + bus thread.
    #[allow(dead_code)]
    rt: Runtime,
}

impl RadioEngine {
    /// Builds the engine: owned runtime → resolver+proxy → controller (native
    /// player) → station, then wires controller events through the station and
    /// applies the persisted volume. `emit` forwards (station-stamped) events to
    /// the UI (the `radio:state` equivalent). (Go `initAudio`.)
    pub fn new(stations: Vec<StationConfig>, volume: f64, emit: EmitFn) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("radio-rt")
            .build()
            .map_err(|e| Error::new(format!("radio: runtime: {e}")))?;
        let handle = rt.handle().clone();

        let resolver = YtdlResolver::new(handle.clone())?;
        let ctrl = Controller::new(resolver.clone() as Arc<dyn StreamResolver>, handle.clone())?;
        let station = StationPlayer::new(
            ctrl.track_controller(),
            resolver.clone() as Arc<dyn PlaylistExpander>,
            emit,
            handle,
        );
        station.set_stations(stations);

        // Route every controller event through the station (auto-advance/loop)
        // and on to the UI. Weak so the controller's emit doesn't pin the station.
        let ws = Arc::downgrade(&station);
        ctrl.set_emit(Arc::new(move |ev| {
            if let Some(st) = ws.upgrade() {
                st.on_audio_event(ev);
            }
        }));

        // Re-apply the persisted volume so a launch matches the bar.
        if volume > 0.0 {
            let _ = station.set_volume(volume);
        }

        Ok(RadioEngine { station, ctrl, resolver, rt })
    }

    /// Replaces the known station list (config save).
    pub fn set_stations(&self, stations: Vec<StationConfig>) {
        self.station.set_stations(stations);
    }

    /// (Re)starts / resumes the station at `idx`.
    pub fn play_station(&self, idx: i32) -> Result<()> {
        self.station.play(idx.max(0) as usize)
    }

    pub fn pause(&self) -> Result<()> {
        self.station.pause()
    }

    pub fn next(&self) -> Result<()> {
        self.station.next()
    }

    pub fn prev(&self) -> Result<()> {
        self.station.prev()
    }

    pub fn set_shuffle(&self, idx: i32, on: bool) -> Result<()> {
        self.station.set_shuffle(idx.max(0) as usize, on)
    }

    pub fn set_volume(&self, v: f64) -> Result<()> {
        self.station.set_volume(v)
    }

    /// Jumps the current track's playhead (the bar's seek timeline → controller).
    pub fn seek(&self, seconds: f64) -> Result<()> {
        self.ctrl.seek(seconds)
    }

    /// Whether the station at `idx` can step track-by-track (config-only).
    pub fn station_has_tracks(&self, idx: i32) -> bool {
        idx >= 0 && self.station.station_has_tracks(idx as usize)
    }
}
