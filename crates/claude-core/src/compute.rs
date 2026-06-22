//! Pure computation of the bar payload, ported from `internal/claude/stats.go`.
//! Clock-injected (`now`) so fixtures assert deterministically; generic over the
//! timezone of `now` so the month-boundary math runs in the same zone Go used
//! (`now.Location()`), while duration math uses absolute instants.

use crate::api::ApiUsage;
use crate::model::{Credentials, NotificationStates, SessionFile, StatsCache};
use crate::process::is_pid_running;
use chrono::{Datelike, Duration, NaiveDate, TimeZone};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// Number of cells in the `░▒▓█` usage meter (mirrors the JS `BAR_CHARS`).
pub const BAR_CHARS: usize = 9;

static MODEL_VERSION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-(\d+)-(\d+)").unwrap());

/// The computed display payload, ported field-for-field from Go's `BarData`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BarData {
    pub account_name: String,
    pub subscription_type: String,
    /// Messages summed across the current billing period.
    pub period_messages: i64,
    /// 0.0..=1.0 when `period_msg_limit > 0`.
    pub period_percent: f64,
    /// `1` sentinel when API-sourced percent is present, else `0`.
    pub period_msg_limit: i64,
    /// "TODAY" or "5-21" or "---".
    pub last_data_label: String,
    pub last_data_msgs: i64,
    /// 0.0..=1.0; negative if unavailable.
    pub hourly_percent: f64,
    pub hourly_reset_in: String,
    pub reset_in: String,
    pub primary_model: String,
    /// BUSY / IDLE / OFFLINE.
    pub status: String,
    pub limit_exceeded: bool,
    /// Unix ms.
    pub last_updated: i64,
}

/// Derives all display metrics from raw file data. `api_usage` may be `None` if
/// the live fetch failed and no file capture exists; local data is the fallback.
pub fn compute_bar_data<Tz>(
    account_name: &str,
    sc: Option<&StatsCache>,
    creds: Option<&Credentials>,
    sessions: &[SessionFile],
    notifs: Option<&NotificationStates>,
    api_usage: Option<&ApiUsage>,
    now: chrono::DateTime<Tz>,
) -> BarData
where
    Tz: TimeZone,
    Tz::Offset: Copy + std::fmt::Display,
{
    let tz = now.timezone();
    let period_start = tz
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap();
    let period_start_str = period_start.format("%Y-%m-%d").to_string();
    let today_str = now.format("%Y-%m-%d").to_string();

    // Sum messages for the billing period + track the most recent day with data.
    // The date comparison is intentionally lexical on the raw "YYYY-MM-DD"
    // strings (matching Go), not parsed.
    let mut period_msgs: i64 = 0;
    let mut last_date = String::new();
    let mut last_msgs: i64 = 0;
    if let Some(sc) = sc {
        for day in &sc.daily_activity {
            if day.date.as_str() >= period_start_str.as_str() {
                period_msgs += day.message_count;
            }
            if day.date.as_str() > last_date.as_str() && day.message_count > 0 {
                last_date = day.date.clone();
                last_msgs = day.message_count;
            }
        }
    }

    // Progress percent — from API only.
    let mut pct = 0.0;
    let mut show_pct = false;
    if let Some(api) = api_usage {
        if api.weekly_percent >= 0.0 {
            pct = api.weekly_percent;
            show_pct = true;
        }
    }

    // Human-readable label for the last-data date.
    let mut last_data_label = "---".to_string();
    if !last_date.is_empty() {
        if last_date == today_str {
            last_data_label = "TODAY".to_string();
        } else if let Ok(d) = NaiveDate::parse_from_str(&last_date, "%Y-%m-%d") {
            last_data_label = format!("{}-{}", d.month(), d.day());
        } else {
            last_data_label = last_date[5..].to_string();
        }
    }

    let reset_in = match api_usage.and_then(|a| a.reset_at) {
        Some(reset_at) => format_duration(reset_at.signed_duration_since(now)),
        None => {
            let (ny, nm) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            let next_reset = tz.with_ymd_and_hms(ny, nm, 1, 0, 0, 0).unwrap();
            format_duration(next_reset.signed_duration_since(now))
        }
    };

    let primary_model = match api_usage {
        Some(a) if !a.model_id.is_empty() => short_model_name(&a.model_id),
        _ => compute_primary_model(sc, &period_start_str),
    };

    let status = compute_status(sessions, now);

    // Trust live rate-limit data when present (recomputed each read, so it flips
    // back to false after a reset). The sticky notification flag is only
    // consulted when no live/file usage exists at all.
    let limit_exceeded = if let Some(a) = api_usage {
        a.limit_exceeded
    } else if let Some(n) = notifs {
        n.exceed_max_limit
            .as_ref()
            .map(|e| e.triggered)
            .unwrap_or(false)
    } else {
        false
    };

    let subscription_type = creds
        .map(|c| c.claude_ai_oauth.subscription_type.to_uppercase())
        .unwrap_or_default();

    let period_msg_limit = if show_pct { 1 } else { 0 };

    let mut hourly_percent = -1.0;
    let mut hourly_reset_in = "---".to_string();
    if let Some(a) = api_usage {
        if a.hourly_percent >= 0.0 {
            hourly_percent = a.hourly_percent;
        }
        if let Some(hr) = a.hourly_reset_at {
            hourly_reset_in = format_duration(hr.signed_duration_since(now));
        }
    }

    BarData {
        account_name: account_name.to_string(),
        subscription_type,
        period_messages: period_msgs,
        period_percent: pct,
        period_msg_limit,
        last_data_label,
        last_data_msgs: last_msgs,
        hourly_percent,
        hourly_reset_in,
        reset_in,
        primary_model,
        status,
        limit_exceeded,
        last_updated: now.timestamp_millis(),
    }
}

fn compute_primary_model(sc: Option<&StatsCache>, period_start_str: &str) -> String {
    let Some(sc) = sc else {
        return "---".to_string();
    };
    let mut totals: HashMap<&str, i64> = HashMap::new();
    for day in &sc.daily_model_tokens {
        if day.date.as_str() >= period_start_str {
            for (model, tokens) in &day.tokens_by_model {
                *totals.entry(model.as_str()).or_insert(0) += *tokens;
            }
        }
    }
    let mut top_model = "";
    let mut top_tokens: i64 = 0;
    for (model, tokens) in &totals {
        if *tokens > top_tokens {
            top_tokens = *tokens;
            top_model = model;
        }
    }
    if top_model.is_empty() {
        if let Some(k) = sc.model_usage.keys().next() {
            top_model = k.as_str();
        }
    }
    short_model_name(top_model)
}

/// Converts a full model ID to a compact display name with version,
/// e.g. `"claude-opus-4-7"` → `"OPUS 4.7"`.
pub fn short_model_name(full: &str) -> String {
    if full.is_empty() {
        return "---".to_string();
    }
    let lower = full.to_lowercase();

    let family = if lower.contains("opus") {
        "OPUS"
    } else if lower.contains("sonnet") {
        "SONNET"
    } else if lower.contains("haiku") {
        "HAIKU"
    } else {
        // Default: upper-cased first 8 bytes (char-boundary-safe), else the whole
        // string. Go slices bytes (`full[:8]`); model ids are ASCII so this is
        // equivalent, but we clamp to a char boundary to avoid panics.
        if full.len() > 8 {
            let mut end = 8;
            while !full.is_char_boundary(end) {
                end -= 1;
            }
            return full[..end].to_uppercase();
        }
        return full.to_uppercase();
    };

    if let Some(m) = MODEL_VERSION_RE.captures(&lower) {
        return format!("{} {}.{}", family, &m[1], &m[2]);
    }
    family.to_string()
}

/// Derives BUSY/IDLE/OFFLINE from session-file freshness.
pub fn compute_status<Tz>(sessions: &[SessionFile], now: chrono::DateTime<Tz>) -> String
where
    Tz: TimeZone,
{
    let now_ms = now.timestamp_millis();
    for s in sessions {
        let age = now_ms - s.updated_at;
        if s.status != "idle" {
            if age < 5 * 60 * 1000 {
                return "BUSY".to_string();
            }
            if s.pid > 0 && is_pid_running(s.pid) {
                return "BUSY".to_string();
            }
        }
    }
    for s in sessions {
        if now_ms - s.updated_at < 60 * 60 * 1000 {
            return "IDLE".to_string();
        }
    }
    "OFFLINE".to_string()
}

/// Formats a countdown: `<=0` → "NOW"; `days>0` → "%dD %dH"; else "%dH %dM".
/// Integer truncation toward zero, matching Go's `int(d.Hours())`.
pub fn format_duration(d: Duration) -> String {
    if d <= Duration::zero() {
        return "NOW".to_string();
    }
    let total_hours = d.num_hours();
    let days = total_hours / 24;
    let hours = total_hours % 24;
    if days > 0 {
        return format!("{}D {}H", days, hours);
    }
    let minutes = d.num_minutes() % 60;
    format!("{}H {}M", total_hours, minutes)
}

/// Builds the 9-cell `░▒▓█` usage meter string (fill char by threshold, `·`
/// padding), mirroring the frontend `renderProgress`.
pub fn render_meter(pct: f64) -> String {
    let filled = ((pct * BAR_CHARS as f64).round() as i64)
        .clamp(0, BAR_CHARS as i64) as usize;
    let empty = BAR_CHARS - filled;
    let fill_char = if pct >= 0.85 {
        '█'
    } else if pct >= 0.55 {
        '▓'
    } else if pct >= 0.25 {
        '▒'
    } else {
        '░'
    };
    let mut out = String::with_capacity(BAR_CHARS * 3);
    for _ in 0..filled {
        out.push(fill_char);
    }
    for _ in 0..empty {
        out.push('·');
    }
    out
}
