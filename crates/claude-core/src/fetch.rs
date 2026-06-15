//! Live usage fetch, TTL cache, and kill-switch — ported from
//! `internal/claude/usage_fetch.go`.
//!
//! Live usage is fetched straight from Anthropic's OAuth usage endpoint rather
//! than the captured `rate_limits.json` file: the file path breaks under a proxy
//! (Claude Code asks the configured `ANTHROPIC_BASE_URL` for utilization, and
//! most proxies don't serve `/api/oauth/usage`, freezing the meter). Going to the
//! real host directly keeps usage live no matter what proxy Claude Code uses. The
//! file reader stays as the offline fallback.

use crate::api::{clamp_pct, ApiUsage};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const API_VERSION: &str = "2023-06-01";

/// Reuse a successful fetch without hitting the network for this long.
fn soft_ttl() -> Duration {
    Duration::seconds(60)
}
/// On a failed refresh, keep serving the last good live value for up to this long.
fn hard_ttl() -> Duration {
    Duration::minutes(5)
}

/// The injectable raw fetch step: token → future of normalized usage. Mirrors
/// Go's `fetchLiveUsage` package var so tests can stub it (returning `None` to
/// stay off the network).
pub type FetchFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<ApiUsage>> + Send>> + Send + Sync>;

/// Process- (or engine-) owned soft/hard TTL cache, keyed by access token.
/// Replaces Go's package-global `usageCache` without a `static`.
#[derive(Clone, Default)]
pub struct UsageCache(Arc<Mutex<HashMap<String, (DateTime<Utc>, ApiUsage)>>>);

impl UsageCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Reports whether the env kill-switch is set. Live fetching is on by default;
/// `CLAWDPANEL_DISABLE_LIVE_USAGE` ∈ {1,true,yes,on} reverts to file-only.
pub fn live_usage_disabled() -> bool {
    match std::env::var("CLAWDPANEL_DISABLE_LIVE_USAGE") {
        Ok(v) => matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

/// Returns the most recent authoritative usage for the token, fetching at most
/// once per `soft_ttl`. On a failed refresh it serves the last good value within
/// `hard_ttl`, otherwise `None` (→ file fallback). `now` is injected so the cache
/// is deterministic under test.
pub async fn live_usage(
    cache: &UsageCache,
    fetch: &FetchFn,
    token: &str,
    now: DateTime<Utc>,
) -> Option<ApiUsage> {
    // Soft path: a recent cached value, no network. The lock is never held across
    // the await below.
    {
        let map = cache.0.lock().unwrap();
        if let Some((at, data)) = map.get(token) {
            if now.signed_duration_since(*at) < soft_ttl() {
                return Some(data.clone());
            }
        }
    }

    if let Some(fresh) = fetch(token.to_string()).await {
        cache
            .0
            .lock()
            .unwrap()
            .insert(token.to_string(), (now, fresh.clone()));
        return Some(fresh);
    }

    // Refresh failed — keep the last good live value while it's still recent.
    let map = cache.0.lock().unwrap();
    if let Some((at, data)) = map.get(token) {
        if now.signed_duration_since(*at) < hard_ttl() {
            return Some(data.clone());
        }
    }
    None
}

#[derive(Debug, Deserialize, Default)]
struct UsageWindow {
    #[serde(default)]
    utilization: f64,
    #[serde(default)]
    resets_at: String,
}

#[derive(Debug, Deserialize, Default)]
struct UsageResponse {
    #[serde(default)]
    five_hour: Option<UsageWindow>,
    #[serde(default)]
    seven_day: Option<UsageWindow>,
}

/// Maps the `/api/oauth/usage` payload onto [`ApiUsage`]. Pure and unit-tested.
/// Returns `None` if the body has neither window (callers fall back to the file).
pub fn parse_usage_response(body: &[u8]) -> Option<ApiUsage> {
    let r: UsageResponse = serde_json::from_slice(body).ok()?;
    if r.five_hour.is_none() && r.seven_day.is_none() {
        return None;
    }

    let mut out = ApiUsage::unavailable();
    if let Some(sd) = &r.seven_day {
        out.weekly_percent = clamp_pct(sd.utilization / 100.0);
        if let Some(t) = parse_reset_time(&sd.resets_at) {
            out.reset_at = Some(t);
        }
    }
    if let Some(fh) = &r.five_hour {
        out.hourly_percent = clamp_pct(fh.utilization / 100.0);
        if let Some(t) = parse_reset_time(&fh.resets_at) {
            out.hourly_reset_at = Some(t);
        }
    }
    if out.weekly_percent >= 1.0 {
        out.limit_exceeded = true;
    }
    // The endpoint carries no model id; the caller backfills it from the file.
    Some(out)
}

fn parse_reset_time(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Performs the authenticated GET against the real Anthropic host, deliberately
/// ignoring `ANTHROPIC_BASE_URL` (hard-coded URL) so a proxy in Claude Code's
/// path can't intercept it. Any error (network, non-200, unparseable) → `None`.
pub async fn http_fetch_usage(token: &str, client: &reqwest::Client) -> Option<ApiUsage> {
    if token.is_empty() {
        return None;
    }
    let resp = client
        .get(USAGE_ENDPOINT)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .header("anthropic-version", API_VERSION)
        .header("User-Agent", "ClawdPanel")
        .send()
        .await
        .ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    // Body capped at 1 MiB, matching Go's io.LimitReader.
    let capped = &body[..body.len().min(1 << 20)];
    parse_usage_response(capped)
}

/// The default production [`FetchFn`]: a single reqwest client (10 s timeout,
/// rustls) shared across calls, wrapping [`http_fetch_usage`].
pub fn default_fetch_fn() -> FetchFn {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    Arc::new(move |token: String| {
        let client = client.clone();
        Box::pin(async move { http_fetch_usage(&token, &client).await })
    })
}
