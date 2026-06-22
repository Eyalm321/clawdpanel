//! Settings window wiring (S6, #53).
//!
//! Lazily creates a single, reused frameless `SettingsWindow`, fills its
//! `Settings` global from the live [`Config`], and wires the panel CRUD callbacks
//! to: mutate the in-memory config → persist it (`config::save`) → apply live
//! (active-account change repaints the bar, feature toggles flip segment
//! visibility, station edits re-render the editor list). Station URLs are
//! validated through the ported `parse_item` (the old `ParseStationItem`).
//!
//! Threading: every closure here runs on the Slint event loop. The only state
//! the background poll loop also reads lives in [`Shared`] behind `Mutex`/atomic.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use clawdpanel_claude_core::config;
use clawdpanel_types::{Config, Features};
use clawdpanel_ui::{
    bar, BarWindow, ClaudeBar, Settings, SettingsAccount, SettingsFeatures, SettingsParseResult,
    SettingsStation, SettingsStationItem, SettingsWindow, Theme,
};

/// The active account the poll loop reads each tick (so a settings/bar account
/// switch redirects the next fetch without restarting the engine).
#[derive(Clone)]
pub struct AccountSel {
    pub path: PathBuf,
    pub name: String,
}

/// State shared with the background poll loop (the only cross-thread surface).
pub struct Shared {
    pub active: Mutex<AccountSel>,
    pub features: Mutex<Features>,
    /// Last-known data gate for the 5H segment (`hourly_percent >= 0`), so a
    /// feature toggle on the UI thread can recompute separators without waiting
    /// for the next data tick.
    pub hourly_gate: AtomicBool,
}

/// UI-thread-only app state (held in an `Rc`, captured by every callback).
pub struct UiState {
    pub cfg: RefCell<Config>,
    pub bar: slint::Weak<BarWindow>,
    pub settings: RefCell<Option<SettingsWindow>>,
    /// Backing model for the station-edit form's dynamic URL rows.
    st_urls: Rc<VecModel<SharedString>>,
    pub shared: Arc<Shared>,
    pub theme_idx: Cell<i32>,
}

impl UiState {
    /// Builds the app state from the loaded config, seeding [`Shared`] with the
    /// active account + feature flags.
    pub fn new(cfg: Config, bar: slint::Weak<BarWindow>, theme_idx: i32) -> Rc<Self> {
        let active = active_account_sel(&cfg);
        let features = cfg.features.clone();
        Rc::new(Self {
            cfg: RefCell::new(cfg),
            bar,
            settings: RefCell::new(None),
            st_urls: Rc::new(VecModel::default()),
            shared: Arc::new(Shared {
                active: Mutex::new(active),
                features: Mutex::new(features),
                hourly_gate: AtomicBool::new(false),
            }),
            theme_idx: Cell::new(theme_idx),
        })
    }
}

/// Resolves the [`AccountSel`] for the config's active account, falling back to
/// the default `~/.claude` when the path is empty or the index is out of range.
fn active_account_sel(cfg: &Config) -> AccountSel {
    let fallback = || AccountSel {
        path: clawdpanel_claude_core::default_account_path()
            .unwrap_or_else(|| PathBuf::from(".claude")),
        name: "main".to_string(),
    };
    let idx = cfg.active_account;
    if idx < 0 {
        return fallback();
    }
    match cfg.accounts.get(idx as usize) {
        Some(a) if !a.path.is_empty() => AccountSel {
            path: PathBuf::from(&a.path),
            name: a.name.clone(),
        },
        _ => fallback(),
    }
}

/// Pushes the feature flags onto the bar: per-segment visibility + the recomputed
/// separator array (so a hidden segment never leaves a doubled/dangling "·").
pub fn apply_features_to_bar(bar: &BarWindow, f: &Features, hourly_gate: bool) {
    let g = bar.global::<ClaudeBar>();
    g.set_show_weekly(f.weekly_usage);
    g.set_show_radio(f.radio);
    g.set_show_monitor(f.monitor);
    g.set_show_theme(f.theme);
    g.set_feature_hourly(f.hourly_usage);
    let seps = bar::bar_separators(
        f.weekly_usage,
        hourly_gate && f.hourly_usage,
        f.radio,
        f.monitor,
        f.theme,
    );
    g.set_sep_visible(ModelRc::new(VecModel::from(seps)));
}

/// Switches the active account (from the bar cycler or the settings cycler):
/// wraps the index, persists, redirects the poll loop, gives the bar instant
/// feedback, and reflects the change in an open settings window.
pub fn set_active_account(ui: &Rc<UiState>, idx: i32) {
    let acc = {
        let mut cfg = ui.cfg.borrow_mut();
        if cfg.accounts.is_empty() {
            return;
        }
        let n = cfg.accounts.len() as i32;
        let idx = idx.rem_euclid(n);
        cfg.active_account = idx;
        let acc = cfg.accounts[idx as usize].clone();
        let _ = config::save(&cfg);
        acc
    };

    crate::sync_tray_account(ui.cfg.borrow().active_account.max(0) as usize);

    if let Ok(mut a) = ui.shared.active.lock() {
        a.path = PathBuf::from(&acc.path);
        a.name = acc.name.clone();
    }

    if let Some(bar) = ui.bar.upgrade() {
        let g = bar.global::<ClaudeBar>();
        let mut data = g.get_data();
        data.account_name = acc.name.to_uppercase().into();
        g.set_data(data);
    }

    if let Some(sw) = ui.settings.borrow().as_ref() {
        let s = sw.global::<Settings>();
        let idx = ui.cfg.borrow().active_account;
        s.set_active_account(idx);
        s.set_acc_sel(idx);
    }
}

/// Opens (creating on first use) the settings window, refreshing its data and
/// theme each time. The window is hidden — not destroyed — on close, so reopening
/// is instant (mirrors the Wails "hide not close" behavior).
pub fn open_settings(ui: &Rc<UiState>) {
    if ui.settings.borrow().is_none() {
        match build_window(ui) {
            Ok(sw) => *ui.settings.borrow_mut() = Some(sw),
            Err(e) => {
                eprintln!("[settings] failed to create window: {e}");
                return;
            }
        }
    }
    if let Some(sw) = ui.settings.borrow().as_ref() {
        sw.global::<Theme>().set_index(ui.theme_idx.get());
        populate(ui, sw);
        let _ = sw.show();
    }
}

/// Closes (hides) an open settings window.
pub fn close_settings(ui: &Rc<UiState>) {
    if let Some(sw) = ui.settings.borrow().as_ref() {
        let _ = sw.hide();
    }
}

/// Pushes the current theme index into an open settings window (called when the
/// bar cycles the theme so the popup tracks it, like the old shared-localStorage).
pub fn sync_theme(ui: &Rc<UiState>, index: i32) {
    if let Some(sw) = ui.settings.borrow().as_ref() {
        sw.global::<Theme>().set_index(index);
    }
}

// ── conversions: Config → Settings global shapes ──

fn to_settings_accounts(cfg: &Config) -> ModelRc<SettingsAccount> {
    let rows: Vec<SettingsAccount> = cfg
        .accounts
        .iter()
        .map(|a| SettingsAccount {
            name: a.name.clone().into(),
            path: a.path.clone().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn to_settings_stations(cfg: &Config) -> ModelRc<SettingsStation> {
    let rows: Vec<SettingsStation> = cfg
        .stations
        .iter()
        .map(|st| {
            let items: Vec<SettingsStationItem> = st
                .items
                .iter()
                .map(|it| SettingsStationItem {
                    raw: it.raw.clone().into(),
                    id: it.id.clone().into(),
                    kind: it.kind.as_str().into(),
                })
                .collect();
            SettingsStation {
                name: st.name.clone().into(),
                items: ModelRc::new(VecModel::from(items)),
                shuffle: st.shuffle,
            }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn to_settings_features(cfg: &Config) -> SettingsFeatures {
    let f = &cfg.features;
    SettingsFeatures {
        radio: f.radio,
        monitor: f.monitor,
        theme: f.theme,
        weekly_usage: f.weekly_usage,
        hourly_usage: f.hourly_usage,
    }
}

/// Refills the whole `Settings` global from the current config (after any CRUD).
fn populate(ui: &Rc<UiState>, sw: &SettingsWindow) {
    let cfg = ui.cfg.borrow();
    let s = sw.global::<Settings>();
    s.set_accounts(to_settings_accounts(&cfg));
    s.set_active_account(cfg.active_account);
    s.set_acc_sel(clamp_idx(cfg.active_account, cfg.accounts.len()));
    s.set_stations(to_settings_stations(&cfg));
    s.set_st_sel(clamp_idx(cfg.active_station, cfg.stations.len()));
    s.set_features(to_settings_features(&cfg));
    // Reset transient form state.
    s.set_acc_form_open(false);
    s.set_acc_error("".into());
    s.set_st_form_open(false);
    s.set_st_error("".into());
}

fn clamp_idx(idx: i32, len: usize) -> i32 {
    if len == 0 {
        0
    } else {
        idx.clamp(0, len as i32 - 1)
    }
}

/// Builds the settings window and wires every `Settings` callback. Each closure
/// captures the shared `Rc<UiState>` plus a `Weak<SettingsWindow>` so it can read
/// form state / re-push refreshed data after mutating + saving the config.
fn build_window(ui: &Rc<UiState>) -> Result<SettingsWindow, slint::PlatformError> {
    let sw = SettingsWindow::new()?;
    let s = sw.global::<Settings>();
    s.set_st_form_urls(ModelRc::from(ui.st_urls.clone()));

    let weak = sw.as_weak();

    // ── nav + close ──
    {
        let weak = weak.clone();
        s.on_show_panel(move |p| {
            if let Some(sw) = weak.upgrade() {
                sw.global::<Settings>().set_active_panel(p);
            }
        });
    }
    {
        let ui = ui.clone();
        s.on_close(move || close_settings(&ui));
    }

    // ── Accounts ──
    {
        let ui = ui.clone();
        s.on_account_select(move |idx| set_active_account(&ui, idx));
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_account_add(move || {
            if let Some(sw) = weak.upgrade() {
                let s = sw.global::<Settings>();
                s.set_acc_edit_index(-1);
                s.set_acc_form_name("".into());
                s.set_acc_form_path("".into());
                s.set_acc_error("".into());
                s.set_acc_form_open(true);
            }
            let _ = &ui;
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_account_edit(move || {
            let Some(sw) = weak.upgrade() else { return };
            let s = sw.global::<Settings>();
            let sel = s.get_acc_sel();
            let cfg = ui.cfg.borrow();
            if let Some(a) = cfg.accounts.get(sel.max(0) as usize) {
                s.set_acc_edit_index(sel);
                s.set_acc_form_name(a.name.clone().into());
                s.set_acc_form_path(a.path.clone().into());
                s.set_acc_error("".into());
                s.set_acc_form_open(true);
            }
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_account_save(move || {
            let Some(sw) = weak.upgrade() else { return };
            let s = sw.global::<Settings>();
            let name = s.get_acc_form_name().trim().to_string();
            let path = s.get_acc_form_path().trim().to_string();
            if name.is_empty() || path.is_empty() {
                s.set_acc_error("Name and Path cannot be empty!".into());
                return;
            }
            {
                let mut cfg = ui.cfg.borrow_mut();
                let idx = s.get_acc_edit_index();
                if idx < 0 {
                    cfg.accounts.push(clawdpanel_types::Account { name, path });
                    cfg.active_account = cfg.accounts.len() as i32 - 1;
                } else if let Some(a) = cfg.accounts.get_mut(idx as usize) {
                    a.name = name;
                    a.path = path;
                }
                let _ = config::save(&cfg);
            }
            // Active account may now point at a renamed/new account → re-apply.
            let active = ui.cfg.borrow().active_account;
            set_active_account(&ui, active);
            refresh_account_count(&ui);
            populate(&ui, &sw);
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_account_delete(move || {
            let Some(sw) = weak.upgrade() else { return };
            let s = sw.global::<Settings>();
            let sel = s.get_acc_sel();
            {
                let mut cfg = ui.cfg.borrow_mut();
                if cfg.accounts.len() <= 1 {
                    s.set_acc_error("At least one account is required.".into());
                    return;
                }
                let idx = sel.max(0) as usize;
                if idx >= cfg.accounts.len() {
                    return;
                }
                cfg.accounts.remove(idx);
                if cfg.active_account >= cfg.accounts.len() as i32 {
                    cfg.active_account = 0;
                }
                let _ = config::save(&cfg);
            }
            let active = ui.cfg.borrow().active_account;
            set_active_account(&ui, active);
            refresh_account_count(&ui);
            populate(&ui, &sw);
        });
    }
    {
        let weak = weak.clone();
        s.on_account_cancel(move || {
            if let Some(sw) = weak.upgrade() {
                let s = sw.global::<Settings>();
                s.set_acc_form_open(false);
                s.set_acc_error("".into());
            }
        });
    }

    // ── Stations ──
    {
        let weak = weak.clone();
        s.on_station_select(move |idx| {
            if let Some(sw) = weak.upgrade() {
                let s = sw.global::<Settings>();
                let len = s.get_stations().row_count() as i32;
                if len > 0 {
                    s.set_st_sel(idx.rem_euclid(len));
                }
            }
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_station_add(move || {
            if let Some(sw) = weak.upgrade() {
                let s = sw.global::<Settings>();
                s.set_st_edit_index(-1);
                s.set_st_form_name("".into());
                ui.st_urls.set_vec(vec![SharedString::new()]);
                s.set_st_error("".into());
                s.set_st_form_open(true);
            }
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_station_edit(move || {
            let Some(sw) = weak.upgrade() else { return };
            let s = sw.global::<Settings>();
            let sel = s.get_st_sel();
            let cfg = ui.cfg.borrow();
            let Some(st) = cfg.stations.get(sel.max(0) as usize) else {
                s.set_st_error("No station to edit — click ADD.".into());
                return;
            };
            s.set_st_edit_index(sel);
            s.set_st_form_name(st.name.clone().into());
            let urls: Vec<SharedString> = if st.items.is_empty() {
                vec![SharedString::new()]
            } else {
                st.items
                    .iter()
                    .map(|it| {
                        if it.raw.is_empty() { it.id.clone() } else { it.raw.clone() }.into()
                    })
                    .collect()
            };
            ui.st_urls.set_vec(urls);
            s.set_st_error("".into());
            s.set_st_form_open(true);
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_station_delete(move || {
            let Some(sw) = weak.upgrade() else { return };
            let s = sw.global::<Settings>();
            let sel = s.get_st_sel();
            {
                let mut cfg = ui.cfg.borrow_mut();
                let idx = sel.max(0) as usize;
                if idx >= cfg.stations.len() {
                    return;
                }
                cfg.stations.remove(idx);
                if cfg.active_station >= cfg.stations.len() as i32 {
                    cfg.active_station = 0;
                }
                let _ = config::save(&cfg);
            }
            populate(&ui, &sw);
        });
    }
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_station_save(move || {
            let Some(sw) = weak.upgrade() else { return };
            let s = sw.global::<Settings>();
            let name = s.get_st_form_name().trim().to_string();
            if name.is_empty() {
                s.set_st_error("Station name cannot be empty!".into());
                return;
            }
            // Collect non-empty rows and validate each via the ParseStationItem port.
            let mut items = Vec::new();
            for row in ui.st_urls.iter() {
                let v = row.trim().to_string();
                if v.is_empty() {
                    continue;
                }
                match clawdpanel_media::parse_item(&v) {
                    Ok(it) => items.push(it),
                    Err(e) => {
                        s.set_st_error(format!("Invalid YouTube URL/ID: {v}\n{e}").into());
                        return;
                    }
                }
            }
            if items.is_empty() {
                s.set_st_error("Add at least one video or playlist URL.".into());
                return;
            }
            {
                let mut cfg = ui.cfg.borrow_mut();
                let idx = s.get_st_edit_index();
                if idx < 0 {
                    cfg.stations.push(clawdpanel_types::StationConfig {
                        name,
                        items,
                        shuffle: false,
                    });
                    cfg.active_station = cfg.stations.len() as i32 - 1;
                } else if let Some(st) = cfg.stations.get_mut(idx as usize) {
                    // Preserve the bar-driven shuffle flag across detail edits.
                    let shuffle = st.shuffle;
                    *st = clawdpanel_types::StationConfig { name, items, shuffle };
                }
                let _ = config::save(&cfg);
            }
            populate(&ui, &sw);
        });
    }
    {
        let weak = weak.clone();
        s.on_station_cancel(move || {
            if let Some(sw) = weak.upgrade() {
                let s = sw.global::<Settings>();
                s.set_st_form_open(false);
                s.set_st_error("".into());
            }
        });
    }
    {
        let ui = ui.clone();
        s.on_st_url_add(move || ui.st_urls.push(SharedString::new()));
    }
    {
        let ui = ui.clone();
        s.on_st_url_remove(move |i| {
            let i = i as usize;
            if i < ui.st_urls.row_count() {
                ui.st_urls.remove(i);
            }
        });
    }
    {
        let ui = ui.clone();
        s.on_st_url_edited(move |i, text| {
            let i = i as usize;
            if i < ui.st_urls.row_count() {
                ui.st_urls.set_row_data(i, text);
            }
        });
    }

    // ── parse validation (server-side ParseStationItem) ──
    s.on_parse_station_item(|input| match clawdpanel_media::parse_item(&input) {
        Ok(it) => SettingsParseResult {
            ok: true,
            id: it.id.into(),
            kind: it.kind.as_str().into(),
            error: "".into(),
        },
        Err(e) => SettingsParseResult {
            ok: false,
            id: "".into(),
            kind: "".into(),
            error: e.into(),
        },
    });

    // ── Options ──
    {
        let ui = ui.clone();
        let weak = weak.clone();
        s.on_feature_toggle(move |key, val| {
            {
                let mut cfg = ui.cfg.borrow_mut();
                match key.as_str() {
                    "radio" => cfg.features.radio = val,
                    "monitor" => cfg.features.monitor = val,
                    "theme" => cfg.features.theme = val,
                    "weeklyUsage" => cfg.features.weekly_usage = val,
                    "hourlyUsage" => cfg.features.hourly_usage = val,
                    _ => {}
                }
                let _ = config::save(&cfg);
                if let Ok(mut f) = ui.shared.features.lock() {
                    *f = cfg.features.clone();
                }
            }
            let features = ui.cfg.borrow().features.clone();
            let gate = ui.shared.hourly_gate.load(Ordering::Relaxed);
            if let Some(bar) = ui.bar.upgrade() {
                apply_features_to_bar(&bar, &features, gate);
            }
            if let Some(sw) = weak.upgrade() {
                sw.global::<Settings>()
                    .set_features(to_settings_features(&ui.cfg.borrow()));
            }
        });
    }

    Ok(sw)
}

/// Updates the bar's account-cycler arrow gate (`< 2 → hidden`) after a CRUD.
fn refresh_account_count(ui: &Rc<UiState>) {
    if let Some(bar) = ui.bar.upgrade() {
        let n = ui.cfg.borrow().accounts.len() as i32;
        bar.global::<ClaudeBar>().set_accounts_count(n.max(1));
    }
}
