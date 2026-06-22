//! Captured rate-limit file (`rate_limits.json`) reader + the normalized
//! [`ApiUsage`] shape, ported from `internal/claude/api.go`.

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// Normalized real-time usage, sourced either from the captured `rate_limits.json`
/// file or the live `/api/oauth/usage` fetch. Percent fields are `0.0..=1.0`, or
/// negative (`-1.0`) when unavailable. Reset times are absolute instants (Utc).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiUsage {
    /// 0.0..=1.0; negative if unavailable.
    pub weekly_percent: f64,
    /// 0.0..=1.0; negative if unavailable.
    pub hourly_percent: f64,
    /// seven_day reset; `None` if unavailable.
    pub reset_at: Option<DateTime<Utc>>,
    /// five_hour reset; `None` if unavailable.
    pub hourly_reset_at: Option<DateTime<Utc>>,
    pub limit_exceeded: bool,
    pub model_id: String,
}

impl ApiUsage {
    /// The "nothing yet" baseline: both percents flagged unavailable.
    pub fn unavailable() -> Self {
        Self {
            weekly_percent: -1.0,
            hourly_percent: -1.0,
            reset_at: None,
            hourly_reset_at: None,
            limit_exceeded: false,
            model_id: String::new(),
        }
    }
}

impl Default for ApiUsage {
    fn default() -> Self {
        Self::unavailable()
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
struct RateLimitWindow {
    #[serde(default)]
    used_percentage: f64,
    #[serde(default)]
    resets_at: i64,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct RateLimitModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    display_name: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct NestedWindows {
    #[serde(default)]
    five_hour: Option<RateLimitWindow>,
    #[serde(default)]
    seven_day: Option<RateLimitWindow>,
}

#[derive(Debug, Deserialize, Default)]
struct RateLimitsFile {
    #[serde(default)]
    five_hour: Option<RateLimitWindow>,
    #[serde(default)]
    seven_day: Option<RateLimitWindow>,
    #[serde(default)]
    model_id: String,
    #[serde(default)]
    model: Option<RateLimitModel>,
    #[serde(default)]
    #[allow(dead_code)] // parsed for fidelity with the Go schema; not shown
    captured_at: i64,
    // The statusline wrapper dumps Claude Code's full statusline payload, where
    // the windows live nested under "rate_limits"; the top-level fields above
    // cover captures that store the block directly.
    #[serde(default)]
    rate_limits: Option<NestedWindows>,
}

/// Loads the most recent rate-limit data captured by the statusline wrapper.
/// Returns `None` only if the file is missing or unparseable; whatever was last
/// captured is always shown, regardless of age (stale-but-real beats a blank
/// meter).
pub fn read_usage(account_path: &Path) -> Option<ApiUsage> {
    let data = fs::read(account_path.join("rate_limits.json")).ok()?;
    let mut rl: RateLimitsFile = serde_json::from_slice(&data).ok()?;

    let mut out = ApiUsage::unavailable();
    if !rl.model_id.is_empty() {
        out.model_id = rl.model_id.clone();
    } else if let Some(m) = &rl.model {
        if !m.id.is_empty() {
            out.model_id = m.id.clone();
        } else if !m.display_name.is_empty() {
            out.model_id = m.display_name.clone();
        }
    }

    if let Some(nested) = rl.rate_limits.take() {
        if rl.seven_day.is_none() {
            rl.seven_day = nested.seven_day;
        }
        if rl.five_hour.is_none() {
            rl.five_hour = nested.five_hour;
        }
    }

    if let Some(sd) = &rl.seven_day {
        out.weekly_percent = clamp_pct(sd.used_percentage / 100.0);
        if sd.resets_at > 0 {
            out.reset_at = Utc.timestamp_opt(sd.resets_at, 0).single();
        }
    }
    if let Some(fh) = &rl.five_hour {
        out.hourly_percent = clamp_pct(fh.used_percentage / 100.0);
        if fh.resets_at > 0 {
            out.hourly_reset_at = Utc.timestamp_opt(fh.resets_at, 0).single();
        }
    }
    if out.weekly_percent >= 1.0 {
        out.limit_exceeded = true;
    }
    Some(out)
}

/// Clamps a fraction into `[0.0, 1.0]`.
pub fn clamp_pct(v: f64) -> f64 {
    if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    }
}
