//! clawdpanel-claude-core — the Claude data engine for the Rust/Slint rewrite
//! (epic #47, slice S2 / #49). A 1:1 port of Go's `internal/claude`: read a
//! Claude account's on-disk state, fetch live usage from Anthropic, and compute
//! the [`BarData`] payload the HUD bar renders.

pub mod api;
pub mod compute;
pub mod config;
pub mod fetch;
pub mod model;
pub mod process;
pub mod reader;

pub use api::ApiUsage;
pub use compute::{render_meter, short_model_name, BarData, BAR_CHARS};
pub use fetch::{default_fetch_fn, parse_usage_response, FetchFn, UsageCache};

use chrono::{DateTime, Local, TimeZone, Utc};
use once_cell::sync::Lazy;
use std::path::Path;

// Process-global fetch fn + cache, mirroring Go's package-level `fetchLiveUsage`
// + `usageCache` so a multi-account poll loop shares one client and one cache.
static GLOBAL_FETCH: Lazy<FetchFn> = Lazy::new(default_fetch_fn);
static GLOBAL_CACHE: Lazy<UsageCache> = Lazy::new(UsageCache::new);

/// Turns a single Claude account directory into the bar payload. The one public
/// entry point: reads the five per-account files, fetches live usage when the
/// token is live and the kill-switch is off, and runs the pure computation.
pub async fn load_bar_data(account_path: &Path, account_name: &str) -> BarData {
    load_bar_data_at(
        account_path,
        account_name,
        Local::now(),
        &GLOBAL_FETCH,
        &GLOBAL_CACHE,
    )
    .await
}

/// Testable core of [`load_bar_data`] with the clock, fetch fn, and cache
/// injected. Generic over the timezone of `now` so calendar math matches Go's
/// `now.Location()`.
pub async fn load_bar_data_at<Tz>(
    account_path: &Path,
    account_name: &str,
    now: DateTime<Tz>,
    fetch: &FetchFn,
    cache: &UsageCache,
) -> BarData
where
    Tz: TimeZone,
    Tz::Offset: Copy + std::fmt::Display,
{
    let sc = reader::read_stats_cache(account_path);
    let creds = reader::read_credentials(account_path);
    let sessions = reader::read_sessions(account_path);
    let notifs = reader::read_notifications(account_path);
    let mut api_usage = api::read_usage(account_path);

    // Prefer authoritative usage fetched straight from Anthropic. It bypasses any
    // ANTHROPIC_BASE_URL proxy, which typically doesn't relay /api/oauth/usage and
    // so leaves the captured rate_limits.json frozen. The file stays the fallback.
    if let Some(creds_ref) = &creds {
        if !fetch::live_usage_disabled() {
            let oauth = &creds_ref.claude_ai_oauth;
            let token_live = oauth.expires_at == 0 || now.timestamp_millis() < oauth.expires_at;
            if !oauth.access_token.is_empty() && token_live {
                let now_utc = now.with_timezone(&Utc);
                if let Some(mut live) =
                    fetch::live_usage(cache, fetch, &oauth.access_token, now_utc).await
                {
                    // The usage endpoint carries no model id; keep the file's so
                    // the model badge doesn't blank out when live takes over.
                    if live.model_id.is_empty() {
                        if let Some(file) = &api_usage {
                            live.model_id = file.model_id.clone();
                        }
                    }
                    api_usage = Some(live);
                }
            }
        }
    }

    compute::compute_bar_data(
        account_name,
        sc.as_ref(),
        creds.as_ref(),
        &sessions,
        notifs.as_ref(),
        api_usage.as_ref(),
        now,
    )
}

/// Returns the computed active status (BUSY / IDLE / OFFLINE) for the account.
pub fn get_status(account_path: &Path) -> String {
    let sessions = reader::read_sessions(account_path);
    compute::compute_status(&sessions, Local::now())
}

/// Resolves the default account directory (`~/.claude`), matching Go's
/// `os.UserHomeDir`-based default. Returns `None` if no home dir is known.
pub fn default_account_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

#[cfg(test)]
mod tests;
