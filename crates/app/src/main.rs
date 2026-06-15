//! ClawdPanel (Rust/Slint rewrite) -- app entry point.
//!
//! S1 (#48): open a frameless, always-on-top, opaque #0B0C0E bar window
//! rendering static placeholder HUD segments in embedded Cascadia Mono. Live
//! data, docking and themes land in later slices.

use slint::ComponentHandle;
// `unstable-winit-030` re-exports winit + the accessor used to drop the server
// titlebar and pin the bar above everything.
use slint::winit_030::winit::window::WindowLevel;
use slint::winit_030::WinitWindowAccessor;

fn main() -> Result<(), slint::PlatformError> {
    // Must run before any window is shown so `font-family: "Cascadia Mono"`
    // resolves against the embedded TTF.
    clawdpanel_ui::register_fonts();

    let w = clawdpanel_ui::BarWindow::new()?;
    w.window()
        .set_size(slint::PhysicalSize::new(1920, 28));
    w.show()?;

    // Frameless + always-on-top via the live winit window (Wayland/X11 have no
    // way to express this from the .slint side).
    w.window().with_winit_window(|win| {
        win.set_decorations(false);
        win.set_window_level(WindowLevel::AlwaysOnTop);
    });

    // Smoke mode: flash the bar then quit so CI / `CLAWDPANEL_SMOKE=1` runs
    // exit 0 without a human closing the window. Held in scope so the timer
    // isn't dropped (which would cancel it) before the event loop runs.
    let _smoke_timer = std::env::var("CLAWDPANEL_SMOKE").is_ok().then(|| {
        let t = slint::Timer::default();
        t.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(700),
            || {
                let _ = slint::quit_event_loop();
            },
        );
        t
    });

    w.run()
}
