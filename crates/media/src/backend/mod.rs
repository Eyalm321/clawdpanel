//! Per-OS native player backends. The factory [`new_player`] hands the
//! controller a `Box<dyn Player>` wired to a channel it emits events on.
//!
//! Strategy B (per the design): keep the native-per-OS split. Linux is the
//! GStreamer `playbin` port (the static-DASH preroll invariants map 1:1);
//! every other OS is the unsupported stub until its backend lands.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod stub;

#[cfg(target_os = "linux")]
pub use linux::new_player;
#[cfg(not(target_os = "linux"))]
pub use stub::new_player;
