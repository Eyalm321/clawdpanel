//! Golden tests ported 1:1 from `internal/claude/{bardata_test,
//! bardata_fallback_test, usage_fetch_test}.go`, run against the same
//! `testdata/*` fixtures. The live fetch is stubbed (`nil_fetch`) to keep the
//! suite off the network — the direct analogue of the Go `init()` stub.

use crate::api::ApiUsage;
use crate::compute::BarData;
use crate::fetch::{parse_usage_response, FetchFn, UsageCache};
use crate::load_bar_data_at;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use std::path::Path;
use std::sync::Arc;

// fixedNow matches the timestamps baked into testdata/populated:
//   - the busy session was updated one minute earlier
//   - seven_day resets three days four hours later  → "3D 4H"
//   - five_hour resets two hours thirty minutes later → "2H 30M"
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 31, 12, 0, 0).unwrap()
}

/// A fetch fn that always returns `None`, keeping the suite offline. Tests that
/// exercise the override path build their own.
fn nil_fetch() -> FetchFn {
    Arc::new(|_token: String| Box::pin(async { None }))
}

#[tokio::test]
async fn load_bar_data_populated_account() {
    let cache = UsageCache::new();
    let fetch = nil_fetch();
    let got =
        load_bar_data_at(Path::new("testdata/populated"), "Test Account", fixed_now(), &fetch, &cache)
            .await;

    let want = BarData {
        account_name: "Test Account".to_string(),
        subscription_type: "MAX".to_string(),
        period_messages: 350, // 100 (5-15) + 250 (5-20), both within May
        period_percent: 68.0 / 100.0,
        period_msg_limit: 1, // sentinel: API-sourced percent is present
        last_data_label: "5-20".to_string(),
        last_data_msgs: 250,
        hourly_percent: 32.5 / 100.0,
        hourly_reset_in: "2H 30M".to_string(),
        reset_in: "3D 4H".to_string(),
        primary_model: "OPUS 4.7".to_string(), // from rate_limits model_id
        status: "BUSY".to_string(),
        limit_exceeded: false, // weekly 0.68 < 1.0
        last_updated: fixed_now().timestamp_millis(),
    };

    assert_eq!(got, want);
}

// --- fallback / edge behaviour (bardata_fallback_test.go) ---

#[tokio::test]
async fn missing_usage_falls_back_to_stats() {
    let got = load_bar_data_at(
        Path::new("testdata/no_usage"),
        "Acct",
        fixed_now(),
        &nil_fetch(),
        &UsageCache::new(),
    )
    .await;

    assert_eq!(got.subscription_type, "PRO");
    assert_eq!(got.period_messages, 80);
    assert_eq!(got.period_percent, 0.0);
    assert_eq!(got.period_msg_limit, 0); // unavailable, not shown
    assert_eq!(got.hourly_percent, -1.0);
    assert_eq!(got.hourly_reset_in, "---");
    assert_eq!(got.reset_in, "12H 0M"); // month-boundary fallback
    assert_eq!(got.primary_model, "SONNET 4.6"); // stats-cache fallback
    assert_eq!(got.last_data_label, "5-10");
    assert_eq!(got.last_data_msgs, 80);
    assert_eq!(got.status, "OFFLINE");
    assert!(!got.limit_exceeded);
}

#[tokio::test]
async fn seconds_format_usage_loads_live_fields() {
    let got = load_bar_data_at(
        Path::new("testdata/fresh_seconds"),
        "Acct",
        fixed_now(),
        &nil_fetch(),
        &UsageCache::new(),
    )
    .await;

    assert_eq!(got.period_msg_limit, 1);
    assert_eq!(got.period_percent, 37.0 / 100.0);
    assert_eq!(got.hourly_percent, 35.0 / 100.0);
    assert_eq!(got.primary_model, "OPUS 4.8"); // from rate_limits model_id
}

#[tokio::test]
async fn limit_exceeded_live_wins_after_reset() {
    let got = load_bar_data_at(
        Path::new("testdata/exceeded_reset"),
        "Acct",
        fixed_now(),
        &nil_fetch(),
        &UsageCache::new(),
    )
    .await;

    assert!(!got.limit_exceeded); // live 50% must override sticky notification
    assert_eq!(got.period_percent, 50.0 / 100.0);
}

#[tokio::test]
async fn limit_exceeded_sticky_fallback() {
    let got = load_bar_data_at(
        Path::new("testdata/exceeded_sticky"),
        "Acct",
        fixed_now(),
        &nil_fetch(),
        &UsageCache::new(),
    )
    .await;

    assert!(got.limit_exceeded); // sticky notification is the only signal
    assert_eq!(got.period_msg_limit, 0); // no live percent available
}

#[tokio::test]
async fn last_data_label_today() {
    let got = load_bar_data_at(
        Path::new("testdata/today"),
        "Acct",
        fixed_now(),
        &nil_fetch(),
        &UsageCache::new(),
    )
    .await;

    assert_eq!(got.last_data_label, "TODAY");
    assert_eq!(got.last_data_msgs, 42);
}

#[tokio::test]
async fn empty_account_uses_sentinels() {
    let dir = std::env::temp_dir().join("clawd_s2_empty_account");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let got = load_bar_data_at(&dir, "Acct", fixed_now(), &nil_fetch(), &UsageCache::new()).await;

    assert_eq!(got.subscription_type, "");
    assert_eq!(got.period_messages, 0);
    assert_eq!(got.period_msg_limit, 0);
    assert_eq!(got.hourly_percent, -1.0);
    assert_eq!(got.last_data_label, "---");
    assert_eq!(got.last_data_msgs, 0);
    assert_eq!(got.primary_model, "---");
    assert_eq!(got.status, "OFFLINE");
    assert_eq!(got.reset_in, "12H 0M"); // month-boundary fallback
    assert!(!got.limit_exceeded);

    let _ = std::fs::remove_dir_all(&dir);
}

// --- live fetch parse + override (usage_fetch_test.go) ---

#[test]
fn parse_usage_response_live_schema() {
    let body = br#"{
      "five_hour":  { "utilization": 7.0,  "resets_at": "2026-06-14T16:50:00.732517+00:00" },
      "seven_day":  { "utilization": 16.0, "resets_at": "2026-06-18T09:59:59.732539+00:00" },
      "seven_day_opus": null,
      "seven_day_sonnet": { "utilization": 0.0, "resets_at": "2026-06-18T09:59:59+00:00" },
      "extra_usage": { "is_enabled": true, "monthly_limit": 20000, "used_credits": 7149.0 }
    }"#;

    let got = parse_usage_response(body).expect("valid body");
    assert_eq!(got.weekly_percent, 16.0 / 100.0);
    assert_eq!(got.hourly_percent, 7.0 / 100.0);
    assert!(!got.limit_exceeded); // weekly 16% < 100%
    assert_eq!(got.reset_at.expect("seven_day reset").year(), 2026);
    assert!(got.hourly_reset_at.is_some());
    assert_eq!(got.model_id, ""); // endpoint carries no model
}

#[test]
fn parse_usage_response_limit_exceeded() {
    let got = parse_usage_response(br#"{"seven_day":{"utilization":100,"resets_at":""}}"#)
        .expect("100% body");
    assert!(got.limit_exceeded);
    assert_eq!(got.weekly_percent, 1.0);
}

#[test]
fn parse_usage_response_empty() {
    // No windows present (or garbage) → None so callers fall back to the file.
    assert!(parse_usage_response(br#"{"seven_day_opus":null}"#).is_none());
    assert!(parse_usage_response(b"not json").is_none());
}

#[tokio::test]
async fn live_overrides_file() {
    // populated's file says weekly 68% / model opus-4-7; the live source disagrees.
    let fetch: FetchFn = Arc::new(|_token: String| {
        Box::pin(async {
            Some(ApiUsage {
                weekly_percent: 0.25,
                hourly_percent: 0.10,
                model_id: String::new(),
                reset_at: None,
                hourly_reset_at: None,
                limit_exceeded: false,
            })
        })
    });

    let got = load_bar_data_at(
        Path::new("testdata/populated"),
        "Acct",
        fixed_now(),
        &fetch,
        &UsageCache::new(),
    )
    .await;

    assert_eq!(got.period_percent, 0.25); // live overrides the file's 0.68
    assert_eq!(got.hourly_percent, 0.10);
    assert_eq!(got.period_msg_limit, 1);
    assert_eq!(got.primary_model, "OPUS 4.7"); // backfilled from file
}
