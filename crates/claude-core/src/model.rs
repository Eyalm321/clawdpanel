//! Serde structs mirroring the on-disk JSON shapes Claude Code writes per
//! account, ported 1:1 from `internal/claude/types.go`. Every field is tolerant
//! (`#[serde(default)]` / `Option`) so a missing or malformed file degrades to
//! empty rather than erroring — matching Go's `encoding/json` behaviour.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DailyActivity {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub message_count: i64,
    #[serde(default)]
    pub session_count: i64,
    #[serde(default)]
    pub tool_call_count: i64,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DailyModelTokens {
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub tokens_by_model: HashMap<String, i64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageDetail {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    #[serde(default)]
    pub cache_creation_input_tokens: i64,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatsCache {
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub last_computed_date: String,
    #[serde(default)]
    pub daily_activity: Vec<DailyActivity>,
    #[serde(default)]
    pub daily_model_tokens: Vec<DailyModelTokens>,
    #[serde(default)]
    pub model_usage: HashMap<String, ModelUsageDetail>,
    #[serde(default)]
    pub total_sessions: i64,
    #[serde(default)]
    pub total_messages: i64,
    #[serde(default)]
    pub hour_counts: HashMap<String, i64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredentials {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub subscription_type: String,
    #[serde(default)]
    pub rate_limit_tier: String,
    #[serde(default)]
    pub expires_at: i64,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    #[serde(default)]
    pub claude_ai_oauth: OAuthCredentials,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionFile {
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct NotificationState {
    #[serde(default)]
    pub triggered: bool,
    #[serde(default)]
    pub timestamp: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct NotificationStates {
    #[serde(default)]
    pub exceed_max_limit: Option<NotificationState>,
    #[serde(default)]
    pub tokens_will_run_out: Option<NotificationState>,
    #[serde(default)]
    pub cost_will_exceed: Option<NotificationState>,
}
