//! ClawdPanel (Rust/Slint rewrite) -- app entry point.
//!
//! S1 (#48): open a frameless, always-on-top, opaque #0B0C0E bar window.
//! S2 (#49): drive that bar with live Claude usage. A tokio runtime on a
//! background thread polls `load_bar_data` (~15s) and `get_status` (500ms) for
//! the active account and pushes results onto the `ClaudeBar` Slint global via
//! `invoke_from_event_loop` + a `Weak<BarWindow>`.
//! S3 (#50): Linux top-edge dock + cursor auto-hide reveal. We force the X11
//! backend (run as an XWayland client under Wayland), resolve the bar's X window
//! id from the winit raw handle, dock it to the configured monitor's top edge
//! (reserving a strut), and start the cursor-driven reveal state machine. A
//! single-instance lock makes a second launch ping the running bar to re-reveal
//! and exit.

use clawdpanel_ui::{ClaudeBar, ClaudeBarData};
use slint::ComponentHandle;
// `unstable-winit-030` re-exports winit + the accessor used to drop the server
// titlebar and pin the bar above everything.
use slint::winit_030::winit::window::WindowLevel;
use slint::winit_030::WinitWindowAccessor;
use std::sync::{Arc, Mutex};

use clawdpanel_platform_shell as shell;

/// Default data-refresh cadence (Go config `RefreshSeconds`, default 15). Full
/// config plumbing is S6; until then the bar polls the default account.
const REFRESH_SECONDS: u64 = 15;
/// Bar height in physical px (Go config `BarHeight`, default 28). Config plumbing
/// is S6; the launch defaults here mirror Go's `NewApp` force-overrides
/// (docked + pinned + opaque on every launch).
const BAR_HEIGHT: i32 = 28;
/// Single-instance / reveal-ping id (matches the Go `SingleInstanceOptions`).
const APP_ID: &str = "com.clawdpanel.app";

fn main() -> Result<(), slint::PlatformError> {
    // Linux: run under XWayland even on Wayland sessions. Wayland gives apps no
    // way to self-position or stay always-on-top (GNOME/Mutter has no layer-shell
    // for third parties), so the whole docking path — geometry, _DOCK type,
    // struts — only works as an X11 client. Must be set before the winit backend
    // initializes. `CLAWDPANEL_NO_XWAYLAND` is the escape hatch.
    #[cfg(target_os = "linux")]
    if std::env::var_os("CLAWDPANEL_NO_XWAYLAND").is_none() {
        std::env::set_var("WINIT_UNIX_BACKEND", "x11");
    }

    // Single-instance: a second launch fails to take the lock, pings the running
    // instance (which re-reveals the bar) and exits. The reveal handler holds a
    // slot filled once the reveal controller is built below.
    let reveal_slot: Arc<Mutex<Option<shell::Controller>>> = Arc::new(Mutex::new(None));
    let slot_for_ping = reveal_slot.clone();
    let _single_instance = match shell::single_instance::acquire(APP_ID, move || {
        if let Some(c) = slot_for_ping.lock().unwrap().as_ref() {
            c.reveal();
        }
    }) {
        Ok(None) => return Ok(()), // another instance is running; we pinged it — exit
        Ok(Some(guard)) => Some(guard),
        Err(e) => {
            eprintln!("[shell] single-instance lock failed: {e}; continuing");
            None
        }
    };

    // Must run before any window is shown so `font-family: "Cascadia Mono"`
    // resolves against the embedded TTF.
    clawdpanel_ui::register_fonts();

    let w = clawdpanel_ui::BarWindow::new()?;
    w.window().set_size(slint::PhysicalSize::new(1920, BAR_HEIGHT as u32));
    w.show()?;

    // Frameless + always-on-top via the live winit window (Wayland/X11 have no
    // way to express this from the .slint side). While we hold the winit window,
    // resolve the bar's X11 window id from its raw handle for the dock path.
    let xid = w.window().with_winit_window(|win| {
        win.set_decorations(false);
        win.set_window_level(WindowLevel::AlwaysOnTop);
        x11_window_id(win)
    }).flatten();

    // Dock + reveal (Linux/X11 only). Held in scope so the X connection, reveal
    // controller and its poll loop live for the whole session.
    #[cfg(target_os = "linux")]
    let _dock = xid.and_then(|id| dock_and_reveal(id, &reveal_slot));
    #[cfg(not(target_os = "linux"))]
    let _ = (&xid, &reveal_slot);

    spawn_bar_engine(w.as_weak());

    // Smoke mode: flash the bar then quit so CI / `CLAWDPANEL_SMOKE=1` runs exit
    // 0 without a human closing the window. Held in scope so the timer isn't
    // dropped (which would cancel it) before the event loop runs.
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

/// Pulls the X11 window id out of a winit window's raw handle (Xlib or Xcb).
/// Returns `None` on non-X11 backends (Wayland/Win/mac) so the caller skips the
/// X-only dock path.
fn x11_window_id(win: &slint::winit_030::winit::window::Window) -> Option<u32> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match win.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as u32),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

/// Docks the bar to the first monitor's top edge and starts the cursor-driven
/// reveal machine. Launch defaults mirror Go's `NewApp` force-overrides (docked,
/// pinned, opaque). Returns the live handles (X connection, reveal controller,
/// poll loop) to keep them alive for the session, or `None` if X init fails.
#[cfg(target_os = "linux")]
fn dock_and_reveal(
    xid: u32,
    reveal_slot: &Arc<Mutex<Option<shell::Controller>>>,
) -> Option<(shell::X11Window, shell::Controller, shell::RunHandle)> {
    let xwin = match shell::X11Window::new(xid) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[shell] X11 init failed: {e}; docking disabled");
            return None;
        }
    };
    xwin.apply_bar_styles();

    let monitors = shell::get_monitors();
    let mon = monitors.first()?.clone();
    // app_bar_mode=true → reserve the strut; pinned=true → bar stays expanded.
    xwin.dock_to_monitor(&mon, BAR_HEIGHT, true, &monitors);
    xwin.set_opacity(1.0);

    let ctrl = shell::Controller::new(xwin.ops());
    ctrl.configure(mon, BAR_HEIGHT, true, false);
    ctrl.init();
    let run = ctrl.spawn_run();
    *reveal_slot.lock().unwrap() = Some(ctrl.clone());
    Some((xwin, ctrl, run))
}

/// Spawns the background poll loop: a tokio runtime on its own thread runs two
/// tickers and posts updates back onto the Slint event loop. Returning from
/// `main` ends the process, tearing this thread down with it.
fn spawn_bar_engine(weak: slint::Weak<clawdpanel_ui::BarWindow>) {
    let account_path = clawdpanel_claude_core::default_account_path()
        .unwrap_or_else(|| std::path::PathBuf::from(".claude"));
    // Account name/path come from config in S6; until then use the Go default.
    let account_name = "main".to_string();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };

        rt.block_on(async move {
            // Data path: full bar payload on the ~15s cadence (first tick fires
            // immediately). The live fetch + TTL cache live inside load_bar_data.
            let data_weak = weak.clone();
            let data_path = account_path.clone();
            let data = tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(REFRESH_SECONDS));
                loop {
                    tick.tick().await;
                    let bar =
                        clawdpanel_claude_core::load_bar_data(&data_path, &account_name).await;
                    let w = data_weak.clone();
                    // Move the plain (Send) BarData onto the UI thread and build
                    // the Slint struct there, so no Slint types cross threads.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let g = ui.global::<ClaudeBar>();
                            g.set_status(bar.status.clone().into());
                            g.set_data(to_claude_bar_data(&bar));
                        }
                    });
                }
            });

            // Status path: cheap 500ms session-freshness check; push only on
            // change (mirrors Go's watchClaudeStatus change-gate).
            let status_weak = weak.clone();
            let status_path = account_path;
            let status = tokio::spawn(async move {
                let mut last = String::new();
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
                loop {
                    tick.tick().await;
                    let status = clawdpanel_claude_core::get_status(&status_path);
                    if status != last {
                        last = status.clone();
                        let w = status_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = w.upgrade() {
                                ui.global::<ClaudeBar>().set_status(status.into());
                            }
                        });
                    }
                }
            });

            let _ = tokio::join!(data, status);
        });
    });
}

/// Maps the claude-core [`BarData`](clawdpanel_claude_core::BarData) onto the
/// Slint `ClaudeBarData`. Builds the `░▒▓█` meters and rounds the percents for
/// display; keeps the raw floats for the bar's warn-color thresholds.
fn to_claude_bar_data(b: &clawdpanel_claude_core::BarData) -> ClaudeBarData {
    let show_hourly = b.hourly_percent >= 0.0;
    let weekly_meter = if b.period_msg_limit > 0 {
        clawdpanel_claude_core::render_meter(b.period_percent)
    } else {
        String::new()
    };
    let hourly_meter = if show_hourly {
        clawdpanel_claude_core::render_meter(b.hourly_percent)
    } else {
        String::new()
    };

    ClaudeBarData {
        account_name: b.account_name.to_uppercase().into(),
        subscription_type: b.subscription_type.clone().into(),
        weekly_pct: (b.period_percent * 100.0).round() as i32,
        period_percent: b.period_percent as f32,
        period_msg_limit: b.period_msg_limit as i32,
        period_messages: b.period_messages as i32,
        weekly_meter: weekly_meter.into(),
        hourly_pct: if show_hourly {
            (b.hourly_percent * 100.0).round() as i32
        } else {
            0
        },
        hourly_percent: b.hourly_percent as f32,
        hourly_meter: hourly_meter.into(),
        hourly_reset_in: b.hourly_reset_in.clone().into(),
        show_hourly,
        reset_in: b.reset_in.clone().into(),
        primary_model: b.primary_model.clone().into(),
        limit_exceeded: b.limit_exceeded,
    }
}
