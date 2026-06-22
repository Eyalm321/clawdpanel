//! System tray (tray-icon + muda) — S5 (#52). Linux-only for now; the Windows /
//! macOS tray backends land with the parity slices (S10 / S11).
//!
//! tray-icon's Linux backend is the StatusNotifierItem / AppIndicator host,
//! driven by GTK + GLib over D-Bus. Rather than spin a second GTK loop, we init
//! GTK on the main (Slint) thread, build the icon + menu there, and let the
//! Slint event loop pump GLib via [`Tray::pump`] on a timer so menu activations
//! are delivered and [`Tray::poll_action`] can drain them — every muda item is
//! `!Send`, so keeping the whole tray on the one thread that owns it sidesteps
//! cross-thread state. If GTK or the tray host is unavailable (headless / no
//! D-Bus host, e.g. CI) we log and run with no tray instead of panicking.
//!
//! Ports `internal/tray/tray.go` (`Manager.Build` + the `SetChecked` helpers)
//! and the `runTray` call site in `app.go`.

use muda::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// A menu activation the app should act on, returned by [`Tray::poll_action`].
/// Mirrors the `tray.Controller` callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    SetAccount(usize),
    SetMonitor(usize),
    ToggleStartup,
    CheckForUpdates,
    OpenSettings,
    Quit,
}

/// The live tray: dropping it tears down the icon + menu, so the app keeps it
/// for the whole session. All fields stay on the thread that built them.
pub struct Tray {
    _tray: TrayIcon,
    // Keep the Menu alive (TrayIcon holds it behind a trait object); also the
    // owner of every item below.
    _menu: Menu,
    account_items: Vec<CheckMenuItem>,
    monitor_items: Vec<CheckMenuItem>,
    startup_item: CheckMenuItem,
    check_updates_id: MenuId,
    settings_id: MenuId,
    quit_id: MenuId,
}

impl Tray {
    /// Builds the tray icon + menu — title (disabled) / per-account radio /
    /// per-monitor radio / "Start on login" check / "Settings…" / "Quit" — the
    /// Rust port of `tray.Manager.Build`. Returns `None` (logged) when GTK or the
    /// tray host can't initialize, so the app degrades to no-tray rather than
    /// crashing on a headless/CI box.
    pub fn new(
        icon_png: &[u8],
        version: &str,
        account_names: &[String],
        num_monitors: usize,
        start_on_login: bool,
        active_account: usize,
        active_monitor: usize,
    ) -> Option<Tray> {
        if let Err(e) = gtk::init() {
            eprintln!("[tray] gtk init failed: {e}; tray disabled");
            return None;
        }

        let menu = Menu::new();

        // Title row, disabled (Go: `Add(...).SetEnabled(false)`).
        let _ = menu.append(&MenuItem::new(format!("Clawd Panel {version}"), false, None));
        let _ = menu.append(&PredefinedMenuItem::separator());

        // Accounts — radio emulated with check items (muda has no radio group;
        // mutual exclusion is enforced in `set_account_checked`).
        let mut account_items = Vec::with_capacity(account_names.len());
        for (i, name) in account_names.iter().enumerate() {
            let item =
                CheckMenuItem::new(format!("Account: {name}"), true, i == active_account, None);
            let _ = menu.append(&item);
            account_items.push(item);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());

        // Monitors — radio emulated, 1-based labels (Go: `Monitor %d`, i+1).
        let mut monitor_items = Vec::with_capacity(num_monitors);
        for i in 0..num_monitors {
            let item = CheckMenuItem::new(format!("Monitor {}", i + 1), true, i == active_monitor, None);
            let _ = menu.append(&item);
            monitor_items.push(item);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());

        let startup_item = CheckMenuItem::new("Start on login", true, start_on_login, None);
        let _ = menu.append(&startup_item);

        let check_updates = MenuItem::new("Check for updates", true, None);
        let _ = menu.append(&check_updates);

        let settings = MenuItem::new("Settings...", true, None);
        let _ = menu.append(&settings);
        let _ = menu.append(&PredefinedMenuItem::separator());

        let quit = MenuItem::new("Quit", true, None);
        let _ = menu.append(&quit);

        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip(format!("Clawd Panel {version}"));
        // NOT a template icon — the brand square is solid color and would tint to
        // a blob (same reasoning as `tray.go`'s comment on `SetIcon`).
        if let Some(icon) = decode_icon(icon_png) {
            builder = builder.with_icon(icon);
        }
        let tray = match builder.build() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[tray] build failed: {e}; tray disabled");
                return None;
            }
        };

        Some(Tray {
            _tray: tray,
            _menu: menu,
            account_items,
            monitor_items,
            startup_item,
            check_updates_id: check_updates.id().clone(),
            settings_id: settings.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    /// Iterate any pending GLib events so the AppIndicator host delivers menu
    /// activations into muda's channel. Non-blocking; call it from a Slint timer.
    pub fn pump(&self) {
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
    }

    /// Drain one pending menu activation (if any) and map it to a [`TrayAction`].
    pub fn poll_action(&self) -> Option<TrayAction> {
        let ev = muda::MenuEvent::receiver().try_recv().ok()?;
        let id = ev.id();
        if id == &self.quit_id {
            return Some(TrayAction::Quit);
        }
        if id == &self.check_updates_id {
            return Some(TrayAction::CheckForUpdates);
        }
        if id == &self.settings_id {
            return Some(TrayAction::OpenSettings);
        }
        if id == self.startup_item.id() {
            return Some(TrayAction::ToggleStartup);
        }
        if let Some(idx) = self.account_items.iter().position(|it| it.id() == id) {
            return Some(TrayAction::SetAccount(idx));
        }
        if let Some(idx) = self.monitor_items.iter().position(|it| it.id() == id) {
            return Some(TrayAction::SetMonitor(idx));
        }
        None
    }

    /// Radio-select account `index`: check it, uncheck the rest. Port of
    /// `SetAccountChecked`.
    pub fn set_account_checked(&self, index: usize) {
        for (i, it) in self.account_items.iter().enumerate() {
            it.set_checked(i == index);
        }
    }

    /// Radio-select monitor `index`. Port of `SetMonitorChecked`.
    pub fn set_monitor_checked(&self, index: usize) {
        for (i, it) in self.monitor_items.iter().enumerate() {
            it.set_checked(i == index);
        }
    }

    /// Reflect the start-on-login state on its checkbox. Port of `SetStartup`.
    pub fn set_startup_checked(&self, enabled: bool) {
        self.startup_item.set_checked(enabled);
    }
}

/// Decode the embedded brand PNG to an RGBA tray icon. Returns `None` (logged)
/// on a decode/build failure so the tray still builds (icon-less) instead of
/// failing the whole startup.
fn decode_icon(png: &[u8]) -> Option<Icon> {
    let img = match image::load_from_memory(png) {
        Ok(i) => i.into_rgba8(),
        Err(e) => {
            eprintln!("[tray] icon decode failed: {e}");
            return None;
        }
    };
    let (w, h) = img.dimensions();
    match Icon::from_rgba(img.into_raw(), w, h) {
        Ok(icon) => Some(icon),
        Err(e) => {
            eprintln!("[tray] icon build failed: {e}");
            None
        }
    }
}
