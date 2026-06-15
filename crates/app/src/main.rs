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

mod radio;
mod settings;
mod updater;

use clawdpanel_ui::{Backend, ClaudeBar, ClaudeBarData, Theme};
use slint::ComponentHandle;
// `unstable-winit-030` re-exports winit + the accessor used to drop the server
// titlebar and pin the bar above everything.
use slint::winit_030::winit::window::WindowLevel;
use slint::winit_030::WinitWindowAccessor;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use clawdpanel_platform_shell as shell;
use settings::UiState;

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

    // Load the persisted config (S6). The shared app state owns it; the poll
    // loop reads the active account + feature flags from its `Shared` half.
    let cfg = clawdpanel_claude_core::config::load();
    let ui = UiState::new(cfg, w.as_weak(), 0);

    // Dock + reveal + system tray (Linux/X11 only). Held in scope so the X
    // connection, reveal controller + poll loop, the tray, and its GLib pump
    // timer all live for the whole session.
    #[cfg(target_os = "linux")]
    let _linux = setup_linux(&w, xid, &reveal_slot);
    #[cfg(not(target_os = "linux"))]
    let _ = (&xid, &reveal_slot);

    wire_interactions(&w, &ui);

    // Radio playback engine (S7): only stood up when the Radio feature is on,
    // mirroring Go's `initAudio`. Held for the session (drop tears down the
    // runtime + gstreamer bus thread).
    let _radio = if ui.cfg.borrow().features.radio {
        radio::setup(&w, &ui)
    } else {
        None
    };

    spawn_bar_engine(w.as_weak(), ui.shared.clone());

    // Smoke mode: flash the bar then quit so CI / `CLAWDPANEL_SMOKE=1` runs exit
    // 0 without a human closing the window. Held in scope so the timer isn't
    // dropped (which would cancel it) before the event loop runs.
    // Smoke mode: step through all five themes (exercising every palette + the
    // CRT overlay + outlined-progress + caret-timer paths), then quit so CI /
    // `CLAWDPANEL_SMOKE=1` runs exit 0 without a human closing the window. Held
    // in scope so the timer isn't dropped (which would cancel it).
    let _smoke_timer = std::env::var("CLAWDPANEL_SMOKE").is_ok().then(|| {
        let t = slint::Timer::default();
        let weak = w.as_weak();
        let state = ui.clone();
        let n = std::rc::Rc::new(std::cell::Cell::new(0_i32));
        t.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(120),
            move || {
                let i = n.get();
                n.set(i + 1);
                if let Some(bar) = weak.upgrade() {
                    bar.global::<Theme>().set_index(i % 5);
                }
                // Exercise the S6 settings window open/close path without a human.
                if i == 3 {
                    settings::open_settings(&state);
                }
                if i == 5 {
                    settings::close_settings(&state);
                }
                // Exercise the S9 update window open/close path (synthetic result,
                // no network) so the Update component's compile + show path runs.
                if i == 6 {
                    updater::smoke_open();
                }
                if i == 8 {
                    updater::smoke_close();
                }
                if i >= 10 {
                    let _ = slint::quit_event_loop();
                }
            },
        );
        t
    });

    w.run()
}

/// Wires the bar's UI→backend intents (the `Backend` global callbacks) and the
/// startup-fixed counts. Theme cycling is owned here: Rust holds the index (so a
/// future slice can persist it the way the old localStorage did) and pushes the
/// new `Theme.index` straight back for an instant repaint. Account/monitor
/// cycling are no-ops until config plumbing (S6) gives more than one of each; the
/// pin's visual toggle is handled in Slint, this just receives the intent (the
/// auto-hide wiring lives in the platform slice).
fn wire_interactions(w: &clawdpanel_ui::BarWindow, ui: &Rc<UiState>) {
    let backend = w.global::<Backend>();

    // Theme cycling: Rust owns the index (mirrors the old shared-localStorage) and
    // pushes it into the bar AND any open settings window for an instant repaint.
    {
        let weak = w.as_weak();
        let ui = ui.clone();
        backend.on_cycle_theme(move |dir| {
            let next = (ui.theme_idx.get() + dir).rem_euclid(5);
            ui.theme_idx.set(next);
            if let Some(bar) = weak.upgrade() {
                bar.global::<Theme>().set_index(next);
            }
            settings::sync_theme(&ui, next);
        });
    }

    // Account cycling now steps through the configured accounts and switches the
    // active one live (S6) — the bar repaints and the poll loop redirects.
    {
        let ui = ui.clone();
        backend.on_cycle_account(move |dir| {
            let active = ui.cfg.borrow().active_account;
            settings::set_active_account(&ui, active + dir);
        });
    }

    backend.on_cycle_monitor(|_dir| { /* monitor switch is the tray radio (S5) */ });
    backend.on_toggle_pin(|| { /* visual handled in Slint; auto-hide is platform slice */ });
    backend.on_toggle_brand_menu(|| { /* brand-menu window is a later slice */ });

    // Tray / brand "Settings…" → open the S6 window (cross-platform; the tray
    // pump invokes this same callback on Linux).
    {
        let ui = ui.clone();
        backend.on_open_settings(move || settings::open_settings(&ui));
    }

    // Update window (S9): register the shared state on this (the UI) thread, then
    // wire the tray / brand "Check for updates" action to the off-thread check.
    updater::init(w.as_weak());
    backend.on_check_for_updates(updater::check_now);

    // Startup-fixed counts + initial feature visibility from the loaded config.
    let g = w.global::<ClaudeBar>();
    g.set_monitors_count(monitor_count());
    let cfg = ui.cfg.borrow();
    g.set_accounts_count((cfg.accounts.len() as i32).max(1));
    settings::apply_features_to_bar(w, &cfg.features, false);
}

/// Number of detected monitors (gates the monitor cycler arrows). X11-only;
/// every other platform reports a single screen until its dock slice lands.
fn monitor_count() -> i32 {
    #[cfg(target_os = "linux")]
    {
        let n = shell::get_monitors().len() as i32;
        if n < 1 {
            1
        } else {
            n
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        1
    }
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
) -> Option<(shell::X11Window, Vec<shell::MonitorInfo>, shell::Controller, shell::RunHandle)> {
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
    Some((xwin, monitors, ctrl, run))
}

/// The brand PNG embedded for the tray icon (decoded to RGBA in `tray.rs` — NOT a
/// template icon, so the colored square survives; same asset the Go build used).
#[cfg(target_os = "linux")]
const TRAY_ICON_PNG: &[u8] = include_bytes!("../../../build/linux/icon.png");

/// Live Linux shell handles kept alive for the session: the dock/reveal tuple,
/// the tray, and its GLib pump timer.
#[cfg(target_os = "linux")]
type LinuxShell = (
    Option<(shell::X11Window, Vec<shell::MonitorInfo>, shell::Controller, shell::RunHandle)>,
    Option<std::rc::Rc<shell::tray::Tray>>,
    Option<slint::Timer>,
);

/// Docks the bar, starts the reveal machine, then builds + wires the system tray.
/// Returns everything that must outlive `main`'s setup so the connection, poll
/// loop, tray and pump timer aren't dropped before `run()`.
#[cfg(target_os = "linux")]
fn setup_linux(
    w: &clawdpanel_ui::BarWindow,
    xid: Option<u32>,
    reveal_slot: &Arc<Mutex<Option<shell::Controller>>>,
) -> LinuxShell {
    let dock = xid.and_then(|id| dock_and_reveal(id, reveal_slot));

    // Tray menu inputs: single account until S6 config plumbing; monitor count
    // from the dock enumeration (≥1). Active indices default to 0.
    let monitors_len = dock.as_ref().map(|d| d.1.len()).unwrap_or(0).max(1);
    let accounts = vec!["main".to_string()];
    let tray = shell::tray::Tray::new(
        TRAY_ICON_PNG,
        env!("CARGO_PKG_VERSION"),
        &accounts,
        monitors_len,
        shell::autostart::is_start_on_login(),
        0,
        0,
    )
    .map(std::rc::Rc::new);

    // Clones of the dock handles the monitor-radio action needs to re-dock; the
    // originals stay in `dock` (X11Window/Controller share their state via Arc).
    let dock_for_wire = dock
        .as_ref()
        .map(|(xwin, monitors, ctrl, _run)| (xwin.clone(), monitors.clone(), ctrl.clone()));

    let timer = tray.clone().map(|t| wire_tray(w, t, dock_for_wire));
    (dock, tray, timer)
}

/// Wires the `Backend` tray callbacks (Quit / Settings… / ToggleStartup / account
/// + monitor radio — the `tray.Controller` port) and starts the GLib pump timer
/// that dispatches native menu clicks back through those same callbacks. Returns
/// the timer so the caller keeps it alive.
#[cfg(target_os = "linux")]
fn wire_tray(
    w: &clawdpanel_ui::BarWindow,
    tray: std::rc::Rc<shell::tray::Tray>,
    dock: Option<(shell::X11Window, Vec<shell::MonitorInfo>, shell::Controller)>,
) -> slint::Timer {
    let backend = w.global::<Backend>();

    // Quit: end the Slint event loop (Go `controller.Quit()` → app teardown).
    backend.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Settings: the `Backend.open-settings` callback is wired cross-platform in
    // `wire_interactions` (opens the S6 window); the tray pump just invokes it.

    // Start-on-login: flip the autostart desktop entry, then reflect the
    // resulting on-disk state onto the checkbox (Go `App.ToggleStartup`).
    {
        let tray = tray.clone();
        backend.on_toggle_startup(move || {
            let enabled = !shell::autostart::is_start_on_login();
            let exe = std::env::current_exe().unwrap_or_default();
            if let Err(e) = shell::autostart::set_start_on_login(enabled, &exe) {
                eprintln!("[tray] set start-on-login failed: {e}");
            }
            tray.set_startup_checked(shell::autostart::is_start_on_login());
        });
    }

    // Account radio: single account until S6; just reflect the selection (Go
    // `App.SetActiveAccount` → `SetAccountChecked`).
    {
        let tray = tray.clone();
        backend.on_set_active_account(move |idx| {
            tray.set_account_checked(idx.max(0) as usize);
        });
    }

    // Monitor radio: re-dock to the chosen monitor + reconfigure the reveal
    // machine, update the bar's monitor label, then reflect it on the radio (Go
    // `App.SetMonitor`).
    {
        let tray = tray.clone();
        let weak = w.as_weak();
        backend.on_set_monitor(move |idx| {
            let idx = idx.max(0) as usize;
            if let Some((xwin, monitors, ctrl)) = dock.as_ref() {
                if let Some(mon) = monitors.get(idx) {
                    xwin.dock_to_monitor(mon, BAR_HEIGHT, true, monitors);
                    ctrl.configure(mon.clone(), BAR_HEIGHT, true, false);
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<ClaudeBar>()
                            .set_monitor_label(format!("{}", idx + 1).into());
                    }
                }
            }
            tray.set_monitor_checked(idx);
        });
    }

    // Pump GLib + dispatch tray clicks on the Slint loop. ~120ms is well under a
    // human click cadence and cheap (events_pending short-circuits when idle).
    let timer = slint::Timer::default();
    let weak = w.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(120),
        move || {
            tray.pump();
            while let Some(action) = tray.poll_action() {
                let Some(ui) = weak.upgrade() else { break };
                let b = ui.global::<Backend>();
                use shell::tray::TrayAction::*;
                match action {
                    SetAccount(i) => b.invoke_set_active_account(i as i32),
                    SetMonitor(i) => b.invoke_set_monitor(i as i32),
                    ToggleStartup => b.invoke_toggle_startup(),
                    CheckForUpdates => b.invoke_check_for_updates(),
                    OpenSettings => b.invoke_open_settings(),
                    Quit => b.invoke_quit(),
                }
            }
        },
    );
    timer
}

/// Spawns the background poll loop: a tokio runtime on its own thread runs two
/// tickers and posts updates back onto the Slint event loop. Returning from
/// `main` ends the process, tearing this thread down with it.
fn spawn_bar_engine(
    weak: slint::Weak<clawdpanel_ui::BarWindow>,
    shared: Arc<settings::Shared>,
) {
    use std::sync::atomic::Ordering;

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
            // immediately). The active account + feature flags are read from the
            // shared state each tick, so a settings/bar account switch or feature
            // toggle is picked up without restarting the loop.
            let data_weak = weak.clone();
            let shared_data = shared.clone();
            let data = tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(std::time::Duration::from_secs(REFRESH_SECONDS));
                loop {
                    tick.tick().await;
                    let (path, name) = {
                        let a = shared_data.active.lock().unwrap();
                        (a.path.clone(), a.name.clone())
                    };
                    let bar = clawdpanel_claude_core::load_bar_data(&path, &name).await;
                    let features = shared_data.features.lock().unwrap().clone();
                    let hourly_gate = bar.hourly_percent >= 0.0;
                    shared_data.hourly_gate.store(hourly_gate, Ordering::Relaxed);
                    let w = data_weak.clone();
                    // Move the plain (Send) BarData onto the UI thread and build
                    // the Slint struct there, so no Slint types cross threads.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            let g = ui.global::<ClaudeBar>();
                            g.set_status(bar.status.clone().into());
                            // Recompute segment visibility + separators from the
                            // live feature flags (so a hidden segment never leaves
                            // a doubled/dangling "·").
                            settings::apply_features_to_bar(&ui, &features, hourly_gate);
                            g.set_data(to_claude_bar_data(&bar));
                        }
                    });
                }
            });

            // Status path: cheap 500ms session-freshness check; push only on
            // change (mirrors Go's watchClaudeStatus change-gate).
            let status_weak = weak.clone();
            let shared_status = shared;
            let status = tokio::spawn(async move {
                let mut last = String::new();
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
                loop {
                    tick.tick().await;
                    let path = { shared_status.active.lock().unwrap().path.clone() };
                    let status = clawdpanel_claude_core::get_status(&path);
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
/// Slint `ClaudeBarData`. Builds the display strings (value, split `░▒▓█` meter
/// parts, formatted counts) and the warn flags via the unit-tested
/// `clawdpanel_ui::bar` helpers; keeps the raw floats for the outlined bar width.
fn to_claude_bar_data(b: &clawdpanel_claude_core::BarData) -> ClaudeBarData {
    use clawdpanel_ui::bar;

    let has_limit = b.period_msg_limit > 0;
    let show_hourly = b.hourly_percent >= 0.0;

    let period_value = if has_limit {
        format!("{}%", (b.period_percent * 100.0).round() as i64)
    } else {
        bar::fmt_msgs(b.period_messages)
    };
    let (weekly_fill, weekly_empty) = if has_limit {
        bar::meter_parts(b.period_percent)
    } else {
        (String::new(), String::new())
    };
    let wwarn = bar::weekly_warn(has_limit, b.period_percent, b.limit_exceeded);

    let (hourly_value, hourly_fill, hourly_empty, hwarn) = if show_hourly {
        let (f, e) = bar::meter_parts(b.hourly_percent);
        (
            format!("{}%", (b.hourly_percent * 100.0).round() as i64),
            f,
            e,
            bar::hourly_warn(b.hourly_percent),
        )
    } else {
        (String::new(), String::new(), String::new(), bar::Warn::None)
    };

    ClaudeBarData {
        account_name: b.account_name.to_uppercase().into(),
        subscription_type: b.subscription_type.to_uppercase().into(),
        period_label: if has_limit { "WEEKLY" } else { "MSGS" }.into(),
        period_value: period_value.into(),
        period_has_limit: has_limit,
        period_percent: b.period_percent as f32,
        period_warn_medium: wwarn == bar::Warn::Medium,
        period_warn_high: wwarn == bar::Warn::High,
        weekly_fill: weekly_fill.into(),
        weekly_empty: weekly_empty.into(),
        show_hourly,
        hourly_value: hourly_value.into(),
        hourly_percent: b.hourly_percent as f32,
        hourly_warn_medium: hwarn == bar::Warn::Medium,
        hourly_warn_high: hwarn == bar::Warn::High,
        hourly_fill: hourly_fill.into(),
        hourly_empty: hourly_empty.into(),
        hourly_reset_in: b.hourly_reset_in.clone().into(),
        reset_in: b.reset_in.clone().into(),
        primary_model: b.primary_model.clone().into(),
    }
}
