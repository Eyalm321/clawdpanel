//! Update window wiring (S9, #56).
//!
//! Orchestrates the platform-shell updater against the `Update` Slint window:
//! the tray "Check for updates" action runs the GitHub `releases/latest` check on
//! a background thread (a throwaway current-thread tokio runtime, like the bar
//! engine), then — back on the Slint loop — fills the lazily-created, reused
//! `UpdateWindow` and shows it (mirroring Go's auto-open). "Update now" streams
//! the installer with live progress pushed onto `UpdateBridge` (replacing the
//! `update:progress` event) and the detached install runs from platform-shell.
//!
//! The window is reached only through `Weak`/thread_local handles that stay on
//! the UI thread; the background threads carry just `Send` data (the result, the
//! `Weak<UpdateWindow>`), so no `Rc`/Slint component crosses a thread boundary.

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle;

use clawdpanel_platform_shell::updater as shell_updater;
use clawdpanel_ui::{BarWindow, Theme, UpdateBridge, UpdateWindow};
use shell_updater::UpdateCheckResult;

/// The running app version (Go `Version`, default `"dev"` → always "newer", so a
/// dev build always sees the latest release as an update). Override at build time
/// with `CLAWDPANEL_VERSION` (the analogue of the Go ldflags `-X main.Version`).
const CURRENT_VERSION: &str = match option_env!("CLAWDPANEL_VERSION") {
    Some(v) => v,
    None => "dev",
};

/// UI-thread-only update state: the reused window + the last check result + a
/// `Weak` bar handle (read for the live theme index when opening).
pub struct UpdateState {
    window: RefCell<Option<UpdateWindow>>,
    last: RefCell<UpdateCheckResult>,
    bar: slint::Weak<BarWindow>,
}

thread_local! {
    /// Set once on the Slint thread in [`init`]; the check-completion closure
    /// (posted via `invoke_from_event_loop`) reads it to open the window without
    /// sending any `Rc` across the worker thread.
    static UPDATE_STATE: RefCell<Option<Rc<UpdateState>>> = const { RefCell::new(None) };
}

/// Registers the shared update state on the UI thread. Call once at startup.
pub fn init(bar: slint::Weak<BarWindow>) {
    let state = Rc::new(UpdateState {
        window: RefCell::new(None),
        last: RefCell::new(UpdateCheckResult::default()),
        bar,
    });
    UPDATE_STATE.with(|s| *s.borrow_mut() = Some(state));
}

/// Runs the GitHub release check off-thread; on completion (back on the Slint
/// loop) it caches the result and opens the update window. Wired to the tray /
/// brand "Check for updates" action.
pub fn check_now() {
    std::thread::spawn(|| {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[updater] runtime build failed: {e}");
                return;
            }
        };
        let res = rt.block_on(shell_updater::check_for_updates(CURRENT_VERSION));
        let _ = slint::invoke_from_event_loop(move || {
            UPDATE_STATE.with(|s| {
                if let Some(st) = s.borrow().clone() {
                    *st.last.borrow_mut() = res.clone();
                    open_window(&st, res);
                }
            });
        });
    });
}

/// Opens the update window with a pre-fetched update check result.
pub fn show_update_window(res: UpdateCheckResult) {
    UPDATE_STATE.with(|s| {
        if let Some(st) = s.borrow().clone() {
            *st.last.borrow_mut() = res.clone();
            open_window(&st, res);
        }
    });
}


/// Opens (creating + wiring on first use) the update window, filling it from
/// `res` and resetting the progress panel. Hidden — not destroyed — on close, so
/// reopening is instant (the Wails "hide not close" behavior).
fn open_window(st: &Rc<UpdateState>, res: UpdateCheckResult) {
    if st.window.borrow().is_none() {
        match build_window(st) {
            Ok(w) => *st.window.borrow_mut() = Some(w),
            Err(e) => {
                eprintln!("[updater] failed to create window: {e}");
                return;
            }
        }
    }
    let guard = st.window.borrow();
    let Some(w) = guard.as_ref() else { return };

    // Track the bar's live theme (like the settings window).
    if let Some(bar) = st.bar.upgrade() {
        w.global::<Theme>().set_index(bar.global::<Theme>().get_index());
    }

    let b = w.global::<UpdateBridge>();
    b.set_current(if res.current.is_empty() { "dev".into() } else { res.current.clone().into() });
    b.set_latest(if res.latest.is_empty() { "—".into() } else { res.latest.clone().into() });
    // On error show the message in the notes box; else the changelog (the window
    // renders the empty-notes fallback itself).
    b.set_changelog(if res.error.is_empty() {
        res.changelog.clone().into()
    } else {
        res.error.to_uppercase().into()
    });
    b.set_has_download(!res.download_url.is_empty());
    // Reset the transient progress panel each open.
    b.set_show_progress(false);
    b.set_failed(false);
    b.set_progress_pct(0.0);
    b.set_progress_status("Downloading Update...".into());
    b.set_progress_details("0.00 / 0.00 MB".into());

    let _ = w.show();
}

/// Builds the update window and wires the `UpdateBridge` callbacks.
fn build_window(st: &Rc<UpdateState>) -> Result<UpdateWindow, slint::PlatformError> {
    let w = UpdateWindow::new()?;
    let b = w.global::<UpdateBridge>();
    let weak = w.as_weak();

    // Drag window handler
    {
        use slint::winit_030::WinitWindowAccessor;
        let weak = weak.clone();
        w.on_drag_window(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.window().with_winit_window(|win| {
                    let _ = win.drag_window();
                });
            }
        });
    }

    // Later / ✕ → hide (state preserved).
    {
        let weak = weak.clone();
        b.on_later(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }
    {
        let weak = weak.clone();
        b.on_close(move || {
            if let Some(w) = weak.upgrade() {
                let _ = w.hide();
            }
        });
    }

    // Update now → install in place (stream + detached install), or hand off to
    // the releases page when there's no in-place flavor for this install.
    {
        let st = st.clone();
        let weak = weak.clone();
        b.on_update_now(move || {
            let res = st.last.borrow().clone();
            if res.download_url.is_empty() {
                shell_updater::open_url(&res.url);
                if let Some(w) = weak.upgrade() {
                    let _ = w.hide();
                }
                return;
            }
            if let Some(w) = weak.upgrade() {
                let bridge = w.global::<UpdateBridge>();
                bridge.set_failed(false);
                bridge.set_progress_pct(0.0);
                bridge.set_progress_status("Downloading Update...".into());
                bridge.set_show_progress(true);
            }
            start_install(weak.clone(), res.download_url.clone());
        });
    }

    Ok(w)
}

/// Streams + installs `url` on a background thread, pushing progress onto
/// `UpdateBridge`. On success the process is replaced by the detached installer
/// (never returns); on failure the window shows the error.
fn start_install(weak: slint::Weak<UpdateWindow>, url: String) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                push_failure(&weak, format!("runtime build failed: {e}"));
                return;
            }
        };

        let prog_weak = weak.clone();
        let mut last_pct: i64 = -1;
        let result = rt.block_on(shell_updater::install_update(&url, move |pct, dl, total| {
            // Throttle UI posts to whole-percent changes (a stream chunk fires
            // far more often than the bar can usefully repaint).
            let p = pct.round() as i64;
            if p == last_pct {
                return;
            }
            last_pct = p;
            let w = prog_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(win) = w.upgrade() {
                    let b = win.global::<UpdateBridge>();
                    b.set_progress_pct(pct as f32);
                    b.set_progress_details(format!("{dl:.2} / {total:.2} MB").into());
                }
            });
        }));

        // install_update exits the process on success, so reaching here is a
        // failure (the download or the detached-install spawn errored).
        if let Err(e) = result {
            push_failure(&weak, e);
        }
    });
}

/// Posts an install failure onto the window's progress panel.
fn push_failure(weak: &slint::Weak<UpdateWindow>, msg: String) {
    let w = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = w.upgrade() {
            let b = win.global::<UpdateBridge>();
            b.set_failed(true);
            b.set_show_progress(true);
            b.set_progress_status("UPDATE FAILED".into());
            b.set_progress_details(msg.into());
        }
    });
}

/// Smoke-only: open the update window with a synthetic "no in-place install"
/// result (exercises the window's compile + show path without any network), then
/// the matching [`smoke_close`]. Mirrors the settings open/close smoke step.
pub fn smoke_open() {
    UPDATE_STATE.with(|s| {
        if let Some(st) = s.borrow().clone() {
            let res = UpdateCheckResult {
                current: CURRENT_VERSION.into(),
                latest: "2.0.0".into(),
                update_available: true,
                url: "https://github.com/Eyalm321/clawdpanel/releases/latest".into(),
                changelog: "Smoke-test release notes.".into(),
                download_url: String::new(),
                error: String::new(),
            };
            open_window(&st, res);
        }
    });
}

pub fn smoke_close() {
    UPDATE_STATE.with(|s| {
        if let Some(st) = s.borrow().as_ref() {
            if let Some(w) = st.window.borrow().as_ref() {
                let _ = w.hide();
            }
        }
    });
}
