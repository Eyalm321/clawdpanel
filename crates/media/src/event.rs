//! Audio event spine — a 1:1 port of Go's `internal/audio/audio.go`
//! ([`State`], [`Event`]) plus the resolver-facing [`ResolvedTrack`].

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Outward-event sink shared by the controller and station player: the closure
/// the app supplies to push events at the UI (the `radio:state` equivalent).
pub type EmitFn = Arc<dyn Fn(Event) + Send + Sync>;

/// Playback state. Mirrors Go `audio.State` (the lowercase string values), so a
/// serialized [`Event`] matches the old `radio:state` JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Loading,
    Playing,
    Paused,
    Error,
    /// The current track played to its natural end (EOS). Distinct from
    /// idle/paused so the station player can auto-advance. Livestreams (HLS)
    /// never emit it.
    Ended,
}

impl State {
    /// The lowercase tag used in logs and the JSON event.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Loading => "loading",
            State::Playing => "playing",
            State::Paused => "paused",
            State::Error => "error",
            State::Ended => "ended",
        }
    }
}

/// One playback event. Mirrors Go `audio.Event` field-for-field (camelCase JSON
/// keys), so the Slint bridge / any external consumer sees the same shape the
/// old `radio:state` event carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub state: State,
    #[serde(rename = "videoID", default, skip_serializing_if = "String::is_empty")]
    pub video_id: String,
    #[serde(rename = "error", default, skip_serializing_if = "String::is_empty")]
    pub err: String,
    /// Stamped by the station player on forwarded events so the UI can filter to
    /// the active station. The audio layer itself leaves it at 0.
    #[serde(rename = "stationIdx")]
    pub station_idx: i32,
    /// Current track playhead + length in seconds (the bar's seek timeline).
    /// `duration` is 0 for livestreams (and briefly before a VOD's length known).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub position: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration: f64,
    /// Marks a throttled position tick (same state, advanced playhead) rather
    /// than a transition. The station player forwards these straight through
    /// without running its advance/skip logic.
    #[serde(default, skip_serializing_if = "is_false")]
    pub progress: bool,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}
fn is_false(v: &bool) -> bool {
    !*v
}

impl Event {
    /// A bare state-transition event (no video id / position).
    pub fn state(state: State) -> Self {
        Event {
            state,
            video_id: String::new(),
            err: String::new(),
            station_idx: 0,
            position: 0.0,
            duration: 0.0,
            progress: false,
        }
    }

    /// A state event carrying the video id.
    pub fn with_video(state: State, video_id: impl Into<String>) -> Self {
        Event {
            video_id: video_id.into(),
            ..Event::state(state)
        }
    }

    /// An error event for a given video id.
    pub fn error(video_id: impl Into<String>, err: impl Into<String>) -> Self {
        Event {
            state: State::Error,
            video_id: video_id.into(),
            err: err.into(),
            station_idx: 0,
            position: 0.0,
            duration: 0.0,
            progress: false,
        }
    }
}

/// A playable stream URL plus whether it is a livestream. Mirrors both Go
/// `audio.ResolvedTrack` and `radio.ResolvedTrack` (they are identical shapes).
/// Livestreams (`is_live`) never end on their own — the station player must not
/// expect a [`State::Ended`] for them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedTrack {
    pub url: String,
    pub is_live: bool,
}
