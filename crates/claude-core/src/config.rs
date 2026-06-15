//! Config load/save — a 1:1 port of Go's `internal/config/config.go`.
//!
//! Resolves the same on-disk location per-OS, seeds the same [`defaults`], and
//! reproduces Go's "unmarshal over Defaults" merge so a partial `config.json`
//! keeps unspecified fields at their default (rather than zeroing them). The
//! schema itself lives in [`clawdpanel_types`].

use std::path::{Path, PathBuf};

use clawdpanel_types::{Account, Config, Hotkeys, StationConfig, StationItem, StationItemKind};
use serde_json::Value;

/// Per-OS application data directory (port of Go `AppDataDir`):
/// `%APPDATA%\ClawdPanel` (Windows), `~/Library/Application Support/ClawdPanel`
/// (macOS), `$XDG_CONFIG_HOME/ClawdPanel` or `~/.config/ClawdPanel` (Linux),
/// falling back to the temp dir.
pub fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(v) = std::env::var_os("APPDATA") {
            if !v.is_empty() {
                return PathBuf::from(v).join("ClawdPanel");
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = dirs::home_dir() {
            return h.join("Library").join("Application Support").join("ClawdPanel");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(v) = std::env::var_os("XDG_CONFIG_HOME") {
            if !v.is_empty() {
                return PathBuf::from(v).join("ClawdPanel");
            }
        }
        if let Some(h) = dirs::home_dir() {
            return h.join(".config").join("ClawdPanel");
        }
    }
    std::env::temp_dir().join("ClawdPanel")
}

/// Path to the persisted `config.json`.
pub fn config_path() -> PathBuf {
    app_data_dir().join("config.json")
}

/// Go-faithful defaults (port of `config.Defaults`). The default account points
/// at `~/.claude`; the four seeded stations are all livestreams (so they loop
/// forever), preserving the pre-collections behavior.
pub fn defaults() -> Config {
    let home = dirs::home_dir().unwrap_or_default();
    let claude_dir = home.join(".claude").to_string_lossy().into_owned();
    let live = |name: &str, id: &str| StationConfig {
        name: name.to_string(),
        items: vec![StationItem {
            kind: StationItemKind::Livestream,
            id: id.to_string(),
            raw: String::new(),
        }],
        shuffle: false,
    };
    Config {
        monitor: 0,
        theme: "terminal-green".to_string(),
        opacity: 0.92,
        refresh_seconds: 15,
        bar_height: 28,
        active_account: 0,
        accounts: vec![Account { name: "main".to_string(), path: claude_dir }],
        hotkeys: Hotkeys {
            cycle_monitor: "Ctrl+Alt+M".to_string(),
            toggle_click_through: "Ctrl+Alt+T".to_string(),
        },
        start_with_windows: false,
        click_through: false,
        app_bar_mode: true,
        pinned: true,
        stations: vec![
            live("CLAUDE FM", "YmQ7jRgf4f0"),
            live("LOFI GIRL", "X4VbdwhkE10"),
            live("SYNTHWAVE", "4xDzrJKXOOY"),
            live("JAZZ", "A8jDx9TLMQc"),
        ],
        active_station: 0,
        radio_volume: 1.0,
        features: Default::default(),
    }
}

/// Loads the config from the default on-disk location, falling back to
/// [`defaults`] when the file is missing or unreadable.
pub fn load() -> Config {
    load_at(&config_path())
}

/// Loads from an explicit path. Reproduces Go's `json.Unmarshal(data, &cfg)` over
/// a defaults-seeded value: present top-level keys overwrite, absent keys keep
/// their default (and absent *nested* keys keep their default via the per-field
/// serde defaults on [`clawdpanel_types::Features`]/[`Hotkeys`]). A missing file
/// or unparseable JSON yields the full defaults.
pub fn load_at(path: &Path) -> Config {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return defaults(),
    };
    let patch: Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(_) => return defaults(),
    };
    let mut base = serde_json::to_value(defaults()).expect("defaults serialize");
    merge_top_level(&mut base, patch);
    serde_json::from_value(base).unwrap_or_else(|_| defaults())
}

/// Overlays `patch`'s top-level object keys onto `base`. Slices and scalars are
/// replaced wholesale (matching Go, which replaces a slice when its key is
/// present); nested objects are merged per-field by serde's field defaults when
/// they re-deserialize.
fn merge_top_level(base: &mut Value, patch: Value) {
    if let (Value::Object(base_map), Value::Object(patch_map)) = (base, patch) {
        for (k, v) in patch_map {
            base_map.insert(k, v);
        }
    }
}

/// Persists the config to the default location.
pub fn save(cfg: &Config) -> std::io::Result<()> {
    save_at(&config_path(), cfg)
}

/// Persists to an explicit path (port of Go `config.Save`): pretty JSON written
/// to a `.tmp` sibling then atomically renamed into place.
pub fn save_at(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample `config.json` exactly as Go's `MarshalIndent` would emit it
    /// (camelCase keys, every field present, `raw` omitted from station items).
    const GO_SAMPLE: &str = r#"{
  "monitor": 1,
  "theme": "terminal-green",
  "opacity": 0.92,
  "refreshSeconds": 15,
  "barHeight": 28,
  "activeAccount": 0,
  "accounts": [
    { "name": "main", "path": "/home/u/.claude" },
    { "name": "work", "path": "/home/u/work/.claude" }
  ],
  "hotkeys": {
    "cycleMonitor": "Ctrl+Alt+M",
    "toggleClickThrough": "Ctrl+Alt+T"
  },
  "startWithWindows": false,
  "clickThrough": false,
  "appBarMode": true,
  "pinned": true,
  "stations": [
    {
      "name": "CLAUDE FM",
      "items": [ { "kind": "livestream", "id": "YmQ7jRgf4f0" } ],
      "shuffle": false
    },
    {
      "name": "GTA RADIO",
      "items": [
        {
          "kind": "video",
          "id": "6TnV43UWoqk",
          "raw": "https://www.youtube.com/watch?v=6TnV43UWoqk&list=PLLvWV__Bn2_PwR92FfrxjsZCAM7zyxzze"
        }
      ],
      "shuffle": true
    }
  ],
  "activeStation": 0,
  "radioVolume": 1.0,
  "features": {
    "radio": true,
    "monitor": true,
    "theme": true,
    "weeklyUsage": true,
    "hourlyUsage": false
  }
}"#;

    #[test]
    fn go_sample_round_trips_field_for_field() {
        let cfg: Config = serde_json::from_str(GO_SAMPLE).expect("parse Go sample");
        // Spot-check a representative field from each section.
        assert_eq!(cfg.monitor, 1);
        assert_eq!(cfg.theme, "terminal-green");
        assert_eq!(cfg.refresh_seconds, 15);
        assert_eq!(cfg.bar_height, 28);
        assert_eq!(cfg.accounts.len(), 2);
        assert_eq!(cfg.accounts[1].name, "work");
        assert_eq!(cfg.hotkeys.cycle_monitor, "Ctrl+Alt+M");
        assert!(cfg.app_bar_mode && cfg.pinned);
        assert_eq!(cfg.stations.len(), 2);
        assert_eq!(cfg.stations[0].items[0].kind, StationItemKind::Livestream);
        assert_eq!(cfg.stations[1].items[0].raw.is_empty(), false);
        assert!(!cfg.features.hourly_usage);
        assert!(cfg.features.weekly_usage);

        // Serialize → re-parse → must equal the first parse (no field loss).
        let again: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(cfg, again);
    }

    #[test]
    fn partial_config_keeps_defaults() {
        // Only a couple of keys present; everything else must fall back to the
        // Go defaults (not zero values).
        let patch = r#"{"monitor": 2, "features": {"radio": false}}"#;
        let mut base = serde_json::to_value(defaults()).unwrap();
        merge_top_level(&mut base, serde_json::from_str(patch).unwrap());
        let cfg: Config = serde_json::from_value(base).unwrap();

        assert_eq!(cfg.monitor, 2); // overridden
        assert_eq!(cfg.bar_height, 28); // default kept
        assert_eq!(cfg.refresh_seconds, 15); // default kept
        assert_eq!(cfg.stations.len(), 4); // default stations kept
        assert!(!cfg.features.radio); // overridden
        assert!(cfg.features.theme); // partial features: default kept
    }

    #[test]
    fn save_then_load_is_stable() {
        let dir = std::env::temp_dir().join(format!("clawdpanel-cfg-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = std::fs::remove_dir_all(&dir);

        let cfg = defaults();
        save_at(&path, &cfg).expect("save");
        let back = load_at(&path);
        assert_eq!(cfg, back);

        // Missing file → defaults.
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load_at(&path), defaults());
    }
}
