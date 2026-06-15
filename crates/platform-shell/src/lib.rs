//! clawdpanel-platform-shell — OS window integration + the auto-hide reveal
//! machine for the Rust/Slint rewrite (epic #47).
//!
//! S3 (#50): Linux (X11 / XWayland) top-edge dock via `x11rb` — `_DOCK` window
//! type, always-on-top, `_NET_WM_STRUT_PARTIAL`, opacity — plus xrandr monitor
//! enumeration with the gnome-panel offset / dock-edge / strut-reservability
//! logic ported from Go, the cursor-driven [`reveal::Controller`] state machine
//! (1:1 port of `internal/reveal`), and single-instance + reveal-ping IPC.
//!
//! The reveal machine talks to the OS only through the [`WindowOps`] seam, so it
//! is fully testable headless with a fake cursor + fake clock.

mod monitor;
mod reveal;
pub mod single_instance;

#[cfg(target_os = "linux")]
mod window;

pub use monitor::get_monitors;
pub use reveal::{Controller, RunHandle};

#[cfg(target_os = "linux")]
pub use window::X11Window;

/// The app-facing monitor descriptor + its docking geometry. Mirrors the Go
/// `platform.MonitorInfo` field-for-field so the ported monitor/dock/reveal math
/// stays 1:1. Win/Linux report physical px; `phys_width` is the value OS-native
/// sizing calls should use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorInfo {
    pub index: i32,
    /// Physical pixels.
    pub left: i32,
    /// Physical pixels.
    pub top: i32,
    /// Logical pixels.
    pub width: i32,
    /// Logical pixels.
    pub height: i32,
    /// Physical pixels (use for OS-native sizing calls).
    pub phys_width: i32,
    /// e.g. 1.25 at 125%.
    pub dpi_scale: f64,
    pub is_primary: bool,
    pub name: String,
    /// Points/pixels between the monitor's true top edge ([`MonitorInfo::top`])
    /// and where the bar's *resting* top lives. macOS menu-bar height; 0 on
    /// Win/Linux.
    pub work_top_offset: i32,
    /// Which edge the bar docks to: `"top"` (default; empty string means top) or
    /// `"bottom"`. Linux sets it per monitor — X11 struts can only reserve space
    /// measured from the ROOT screen edges, so on stacked layouts a monitor with
    /// another above it can only get true space reservation along its bottom edge.
    pub dock_edge: String,
}

/// Bar width to use for OS-native sizing: physical px when known, else logical.
pub fn width_px(m: &MonitorInfo) -> i32 {
    if m.phys_width != 0 {
        m.phys_width
    } else {
        m.width
    }
}

/// The narrow set of OS window operations the reveal machine needs. The
/// production adapter ([`X11Window`] on Linux) binds a single window handle and
/// forwards to the platform layer; tests inject a fake to assert slide positions
/// and exercise generation/cancellation without a real OS window. Methods that
/// don't take a handle (cursor/predicates) are on the seam too so the fake
/// controls every input regardless of the host OS the test runs on.
pub trait WindowOps: Send + Sync {
    /// Root-relative `(left, top, width, height)`.
    fn window_rect(&self) -> (i32, i32, i32, i32);
    fn move_to(&self, x: i32, y: i32);
    fn clip_top(&self, width: i32, height: i32, top_clip: i32);
    fn show(&self);
    fn hide(&self);
    fn set_click_through(&self, enabled: bool);
    fn cursor_pos(&self) -> (i32, i32);
    fn full_screen_active(&self, mon: &MonitorInfo) -> bool;
    fn auto_hide_supported(&self) -> bool;
}
