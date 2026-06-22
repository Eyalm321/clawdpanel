//! Shared types for ClawdPanel (Rust/Slint rewrite).
//!
//! The persisted-config structs here are a 1:1 mirror of Go's
//! `internal/config/config.go` JSON schema (camelCase keys, same fields), so a
//! `config.json` written by the Go build round-trips through the Rust build with
//! no field loss. Load/save (and the `Defaults()` port) live in
//! `clawdpanel-claude-core::config`; this crate owns only the shape.

use serde::{Deserialize, Serialize};

/// A configured Claude account (name + path to its config dir). Mirrors Go
/// `config.AccountConfig` (`{name, path}`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub path: String,
}

/// Global hotkeys. Mirrors Go `config.HotkeyConfig`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hotkeys {
    pub cycle_monitor: String,
    pub toggle_click_through: String,
}

/// Classifies one entry in a radio station's collection. Mirrors Go
/// `config.StationItemKind` (the `"video" | "playlist" | "livestream"` string).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StationItemKind {
    #[default]
    Video,
    Playlist,
    Livestream,
}

impl StationItemKind {
    /// The lowercase string used in the JSON / by the resolver.
    pub fn as_str(self) -> &'static str {
        match self {
            StationItemKind::Video => "video",
            StationItemKind::Playlist => "playlist",
            StationItemKind::Livestream => "livestream",
        }
    }
}

/// One YouTube source in a station. Mirrors Go `config.StationItem`
/// (`{kind, id, raw?}`); `raw` is `omitempty`, matching Go.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StationItem {
    pub kind: StationItemKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw: String,
}

/// A named, ordered collection of YouTube items. Mirrors Go
/// `config.StationConfig`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StationConfig {
    pub name: String,
    #[serde(default)]
    pub items: Vec<StationItem>,
    #[serde(default)]
    pub shuffle: bool,
}

/// Toggles which optional bar segments are active. Mirrors Go
/// `config.FeatureConfig`; every flag defaults to `true` (an absent or partial
/// `features` object keeps unspecified flags enabled, matching Go's
/// "unmarshal over Defaults").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    #[serde(default = "default_true")]
    pub radio: bool,
    #[serde(default = "default_true")]
    pub monitor: bool,
    #[serde(default = "default_true")]
    pub theme: bool,
    #[serde(default = "default_true")]
    pub weekly_usage: bool,
    #[serde(default = "default_true")]
    pub hourly_usage: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            radio: true,
            monitor: true,
            theme: true,
            weekly_usage: true,
            hourly_usage: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Persisted app configuration. A 1:1 mirror of Go `config.Config` — same
/// fields, same camelCase JSON keys — so the on-disk `config.json` round-trips
/// between the Go and Rust builds without loss.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub monitor: i32,
    pub theme: String,
    pub opacity: f64,
    pub refresh_seconds: i32,
    pub bar_height: i32,
    pub active_account: i32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub hotkeys: Hotkeys,
    pub start_with_windows: bool,
    pub click_through: bool,
    pub app_bar_mode: bool,
    pub pinned: bool,
    #[serde(default)]
    pub stations: Vec<StationConfig>,
    pub active_station: i32,
    pub radio_volume: f64,
    #[serde(default)]
    pub features: Features,
}

impl Default for Config {
    /// Zero-ish baseline. The real, Go-faithful defaults (home-dir account path,
    /// the four seeded stations, etc.) live in
    /// `clawdpanel-claude-core::config::defaults`.
    fn default() -> Self {
        Self {
            monitor: 0,
            theme: String::new(),
            opacity: 1.0,
            refresh_seconds: 15,
            bar_height: 28,
            active_account: 0,
            accounts: Vec::new(),
            hotkeys: Hotkeys::default(),
            start_with_windows: false,
            click_through: false,
            app_bar_mode: true,
            pinned: true,
            stations: Vec::new(),
            active_station: 0,
            radio_volume: 1.0,
            features: Features::default(),
        }
    }
}

/// A physical monitor and its docking geometry (filled by platform-shell, #50).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub dpi_scale: f64,
    pub dock_edge: DockEdge,
}

/// Which screen edge the bar docks to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DockEdge {
    #[default]
    Top,
    Bottom,
}

/// Bar status as computed from session activity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Busy,
    #[default]
    Idle,
    Offline,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_json_round_trips() {
        let cfg = Config {
            monitor: 1,
            theme: "terminal-green".into(),
            opacity: 0.92,
            refresh_seconds: 15,
            bar_height: 28,
            active_account: 0,
            accounts: vec![Account { name: "main".into(), path: "/home/u/.claude".into() }],
            hotkeys: Hotkeys {
                cycle_monitor: "Ctrl+Alt+M".into(),
                toggle_click_through: "Ctrl+Alt+T".into(),
            },
            start_with_windows: false,
            click_through: false,
            app_bar_mode: true,
            pinned: true,
            stations: vec![StationConfig {
                name: "lofi".into(),
                items: vec![StationItem {
                    kind: StationItemKind::Livestream,
                    id: "X4VbdwhkE10".into(),
                    raw: String::new(),
                }],
                shuffle: true,
            }],
            active_station: 0,
            radio_volume: 1.0,
            features: Features::default(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn camel_case_keys_match_go() {
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        for key in [
            "\"refreshSeconds\"",
            "\"barHeight\"",
            "\"activeAccount\"",
            "\"startWithWindows\"",
            "\"clickThrough\"",
            "\"appBarMode\"",
            "\"activeStation\"",
            "\"radioVolume\"",
        ] {
            assert!(json.contains(key), "missing camelCase key {key} in {json}");
        }
    }

    #[test]
    fn station_item_omits_empty_raw() {
        let it = StationItem { kind: StationItemKind::Video, id: "abc".into(), raw: String::new() };
        let json = serde_json::to_string(&it).unwrap();
        assert!(!json.contains("raw"), "empty raw must be omitted: {json}");

        let it2 = StationItem { kind: StationItemKind::Playlist, id: "x".into(), raw: "url".into() };
        let json2 = serde_json::to_string(&it2).unwrap();
        assert!(json2.contains("\"raw\":\"url\""), "non-empty raw kept: {json2}");
    }

    #[test]
    fn features_default_all_true() {
        let f = Features::default();
        assert!(f.radio && f.monitor && f.theme && f.weekly_usage && f.hourly_usage);
    }

    #[test]
    fn partial_features_keep_missing_true() {
        // A features object with only `radio:false` set → the rest stay true,
        // matching Go's "unmarshal over Defaults".
        let f: Features = serde_json::from_str(r#"{"radio":false}"#).unwrap();
        assert!(!f.radio);
        assert!(f.monitor && f.theme && f.weekly_usage && f.hourly_usage);
    }

    #[test]
    fn defaults_are_sane() {
        assert!(Features::default().theme);
        assert_eq!(Status::default(), Status::Idle);
        assert_eq!(DockEdge::default(), DockEdge::Top);
    }
}
