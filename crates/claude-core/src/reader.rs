//! Per-account file readers, ported from `internal/claude/reader.go`. Each
//! tolerates a missing/malformed file by returning `None`/empty (graceful
//! degrade) rather than propagating an error.

use crate::model::{Credentials, NotificationStates, SessionFile, StatsCache};
use std::fs;
use std::path::Path;

pub fn read_stats_cache(account_path: &Path) -> Option<StatsCache> {
    let data = fs::read(account_path.join("stats-cache.json")).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn read_credentials(account_path: &Path) -> Option<Credentials> {
    let data = fs::read(account_path.join(".credentials.json")).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn read_sessions(account_path: &Path) -> Vec<SessionFile> {
    let dir = account_path.join("sessions");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(data) = fs::read(&path) else {
            continue;
        };
        if let Ok(s) = serde_json::from_slice::<SessionFile>(&data) {
            sessions.push(s);
        }
    }
    sessions
}

pub fn read_notifications(account_path: &Path) -> Option<NotificationStates> {
    let data = fs::read(account_path.join("config").join("notification_states.json")).ok()?;
    serde_json::from_slice(&data).ok()
}
