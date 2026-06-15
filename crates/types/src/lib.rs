//! Shared types for ClawdPanel (Rust/Slint rewrite).
//!
//! These are the initial shared structs for the rewrite scaffold. Field-level
//! fidelity with the Go schema is finalized in S2 (claude-core, #49) and
//! S6 (settings/config, #53). See docs/rework-rust-slint/.

use serde::{Deserialize, Serialize};

/// A configured Claude account (name + path to its config dir).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub path: String,
}

/// Feature toggles surfaced in Settings -> Options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Features {
    pub radio: bool,
    pub monitor: bool,
    pub theme: bool,
    pub weekly_usage: bool,
    pub hourly_usage: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self { radio: false, monitor: false, theme: true, weekly_usage: true, hourly_usage: false }
    }
}

/// A radio station: a label plus its source items.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StationConfig {
    pub name: String,
    pub items: Vec<StationItem>,
    pub shuffle: bool,
}

/// One station source (raw URL/ID + parsed video/playlist id).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StationItem {
    pub raw: String,
    pub id: String,
}

/// Persisted app configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub active_account: usize,
    pub features: Features,
    pub stations: Vec<StationConfig>,
    pub bar_height: u32,
    pub refresh_seconds: u32,
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

/// The rendered HUD payload pushed to the Slint bar (#49 fills the real compute).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BarData {
    pub account_name: String,
    pub subscription_type: String,
    pub weekly_percent: f64,
    pub hourly_percent: f64,
    pub reset_in: String,
    pub hourly_reset_in: String,
    pub primary_model: String,
    pub status: Status,
    pub limit_exceeded: bool,
    pub last_updated: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_json_round_trips() {
        let cfg = Config {
            accounts: vec![Account { name: "main".into(), path: "/home/u/.claude".into() }],
            active_account: 0,
            features: Features::default(),
            stations: vec![StationConfig { name: "lofi".into(), items: vec![], shuffle: true }],
            bar_height: 28,
            refresh_seconds: 15,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn defaults_are_sane() {
        assert!(Features::default().theme);
        assert_eq!(Status::default(), Status::Idle);
        assert_eq!(DockEdge::default(), DockEdge::Top);
    }
}
