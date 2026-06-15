//! clawdpanel-media — the radio/VOD playback spine for the Rust/Slint rewrite
//! (S7, #54). A port of Go's `internal/audio` + `internal/radio` +
//! `internal/station`:
//!
//! * [`parse`] — station-source parsing ([`parse_item`], [`has_multiple_tracks`]).
//! * [`event`] — the [`Event`]/[`State`] spine + [`ResolvedTrack`].
//! * [`player`] / [`backend`] — the native [`Player`] trait + per-OS backends.
//! * [`controller`] — the single-track [`Controller`] (retry-once, sticky error,
//!   progress throttle, threading contract).
//! * [`station`] — the [`StationPlayer`] queue engine ([`TrackController`] seam).
//! * [`resolver`] — the [`StreamResolver`]/[`PlaylistExpander`] seams + the
//!   `rusty_ytdl` impl behind them (the XL extraction risk).
//! * [`proxy`] — the local HTTP proxy + VOD byte-cache.
//! * [`staticize`] — the live-DASH staticizer (the "playing but silent" fix).

pub mod backend;
pub mod controller;
pub mod engine;
pub mod error;
pub mod event;
pub mod format;
pub mod parse;
pub mod player;
pub mod proxy;
pub mod resolver;
pub mod staticize;
pub mod station;
pub mod ytdl;

pub use clawdpanel_types::{StationConfig, StationItem, StationItemKind};

pub use controller::Controller;
pub use engine::RadioEngine;
pub use error::{Error, Result};
pub use event::{EmitFn, Event, ResolvedTrack, State};
pub use parse::{has_multiple_tracks, parse_item};
pub use player::Player;
pub use resolver::{PlaylistExpander, StreamResolver};
pub use station::{StationPlayer, TrackController};
pub use ytdl::YtdlResolver;
