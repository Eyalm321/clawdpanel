//! The single-track native player abstraction — a 1:1 port of Go's
//! `audio.Player` interface, with the optional `SetLiveHint` folded into the
//! trait as a default no-op (the Go code type-asserts for it).
//!
//! All methods take `&self`: the controller holds the player as a `Box<dyn
//! Player>` and drives it from multiple tasks, so backends use interior
//! mutability (a `Mutex` around the native pipeline) exactly as the Go players
//! guard their handles with `sync.Mutex`.

use crate::error::Result;

/// Drives one stream URL (HTTP/HLS/DASH). NOT the system volume — `set_volume`
/// sets the player's own output level. Backends emit [`Event`](crate::Event)s
/// asynchronously through the channel they were constructed with (never on the
/// caller's stack — see the threading contract in `controller.rs`).
pub trait Player: Send + Sync {
    /// Loads `url` and starts from the beginning.
    fn play(&self, url: &str) -> Result<()>;
    /// Continues the currently-loaded track from its paused position (distinct
    /// from [`play`](Player::play), which reloads from 0).
    fn resume(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    /// Output volume, 0.0..=1.0 (clamped by the backend).
    fn set_volume(&self, v: f64) -> Result<()>;
    /// Jumps the playhead to `seconds`. No-op/best-effort for livestreams and on
    /// backends that don't support it.
    fn seek(&self, seconds: f64) -> Result<()>;
    /// Marks the NEXT [`play`](Player::play) as a livestream. Default no-op
    /// (mirrors Go's optional `SetLiveHint`); live backends must not pause for
    /// buffering.
    fn set_live_hint(&self, _live: bool) {}
    fn close(&self) -> Result<()> {
        Ok(())
    }
}
