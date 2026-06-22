# claude-core — Claude data engine (Go → Rust/Slint)

The heart of the HUD: read Claude Code's per-account state from disk, fetch live
usage from Anthropic, and compute the `BarData` payload the bar renders.

## Scope

Existing files (all under `internal/claude/`):

- `types.go` — JSON-mirroring structs (`StatsCache`, `Credentials`, `SessionFile`, `NotificationStates`, …).
- `api.go` — `APIUsage`, `rateLimitsFile`, `readUsage()` (parses `rate_limits.json`), `clampPct()`.
- `reader.go` — `readStatsCache`, `readCredentials`, `readSessions`, `readNotifications` (file I/O).
- `load.go` — `LoadBarData()` (the one public entry point), `loadBarDataAt()` (clock-injected core), `GetStatus()`.
- `stats.go` — `BarData`, `computeBarData()` + helpers (`computePrimaryModel`, `shortModelName`, `computeStatus`, `formatDuration`).
- `usage_fetch.go` (+ `usage_fetch_test.go`) — live `/api/oauth/usage` fetch, TTL cache, kill-switch.
- `process_other.go` / `process_windows.go` — `isPidRunning(pid)` (build-tagged per OS).
- `bardata_test.go`, `bardata_fallback_test.go` — golden tests over `testdata/*`.

Consumers (out of slice, but define the contract):
- `app.go:734 GetBarData()` — Wails binding; picks `cfg.Accounts[cfg.ActiveAccount]`, calls `LoadBarData(acc.Path, acc.Name)`.
- `app.go:1024 watchClaudeStatus()` — 500 ms ticker; calls `GetStatus(acc.Path)`, emits Wails event `claude:status` on change.
- `frontend/src/main.js:84/252` — polls `GetBarData()` every `cfg.refreshSeconds` (default **15 s**) and on each `claude:status` event.
- `internal/config/config.go` — `AccountConfig{Name, Path}`; default account `{name:"main", path: ~/.claude}`, `RefreshSeconds:15`, `ActiveAccount:0`.

Producer of `rate_limits.json` (NOT this slice): a `statusLine.command` node one-liner the installers wire into `~/.claude/settings.json`; it dumps Claude Code's full statusline payload + `captured_at: Date.now()` on every prompt. We only read it.

## Current behavior

### Files read (per account dir, default `~/.claude`)

| File | Struct | Notes |
|---|---|---|
| `stats-cache.json` | `StatsCache` | camelCase keys: `dailyActivity[]{date,messageCount,sessionCount,toolCallCount}`, `dailyModelTokens[]{date,tokensByModel{model:int64}}`, `modelUsage{model:{inputTokens,…}}`, `totalSessions/Messages`, `hourCounts`. `date` = `"YYYY-MM-DD"`. |
| `.credentials.json` | `Credentials.claudeAiOauth` | `{accessToken, subscriptionType, rateLimitTier, expiresAt}`. `expiresAt` = **Unix ms**. |
| `sessions/*.json` | `[]SessionFile` | each `{pid, sessionId, status, updatedAt(ms), version, kind}`. Dir scanned; non-`.json`/dirs skipped; bad files skipped. Seen statuses: `"busy"`, `"idle"`. |
| `config/notification_states.json` | `NotificationStates` | `exceed_max_limit / tokens_will_run_out / cost_will_exceed`, each `{triggered:bool, timestamp:string}`. |
| `rate_limits.json` | `rateLimitsFile` | `five_hour/seven_day{used_percentage(0–100), resets_at(Unix **sec**)}`, `model_id` or `model{id,display_name}`, `captured_at`. **Fallback:** windows may be nested under `rate_limits{…}` (full statusline dump) — top-level takes priority, else copied from nested. |

All readers tolerate missing/malformed files → `nil`/empty (graceful degrade). `readUsage` always returns the last capture **regardless of age** (stale-but-real beats blank).

### Live usage fetch (`usage_fetch.go`)

- `GET https://api.anthropic.com/api/oauth/usage` — **hard-coded host**, deliberately bypasses any `ANTHROPIC_BASE_URL` proxy Claude Code is pointed at (proxies like Headroom don't relay this endpoint, freezing the file).
- Headers: `Authorization: Bearer <accessToken>`, `anthropic-beta: oauth-2025-04-20`, `anthropic-version: 2023-06-01`, `User-Agent: ClawdPanel`. Client timeout **10 s**, body capped at 1 MiB.
- Response (`usageResponse`): `five_hour/seven_day{utilization(0–100 **float**), resets_at(**RFC3339 string**)}`. Extra fields (`seven_day_opus/sonnet`, `extra_usage`) ignored. **No model id** in payload.
- `parseUsageResponse` → `APIUsage`: `WeeklyPercent = clamp(seven_day.utilization/100)`, `HourlyPercent = clamp(five_hour.utilization/100)`, reset times via `time.Parse(RFC3339)`, `LimitExceeded = WeeklyPercent ≥ 1.0`. Returns `nil` if both windows absent or body unparseable → file fallback.
- Cache: process-global `map[token]→{at,data}` under mutex. **Soft TTL 60 s** (serve cached, no network), **hard TTL 5 min** (on failed refresh, serve last good; else `nil`). Clock injected for tests; `fetchLiveUsage` is a swappable package var (tests stub it `nil` to stay offline).
- Kill-switch: `CLAWDPANEL_DISABLE_LIVE_USAGE` ∈ {1,true,yes,on} → file-only.
- Gating (`load.go`): fetch only when `creds != nil`, not disabled, `accessToken != ""`, and token live (`expiresAt == 0 || now.UnixMilli() < expiresAt`). On success **live overrides file**; if live carries no `ModelID`, backfill from the file so the model badge persists.

### Computation (`computeBarData`, clock-injected)

- **periodStart** = 1st of current month, `now.Location()` (local tz), as `"YYYY-MM-DD"`.
- **PeriodMessages** = Σ `dailyActivity.messageCount` where `date >= periodStart` (lexical string compare). **LastData** = most recent `dailyActivity` with `messageCount > 0` → `LastDataLabel`/`LastDataMsgs`.
- **PeriodPercent / showPct** = `apiUsage.WeeklyPercent` when `≥ 0`. **PeriodMsgLimit** = `1` sentinel when `showPct` (tells bar to render the meter), else `0`.
- **LastDataLabel** = `"TODAY"` if last date is today; else `"M-D"` (numeric, no zero-pad, e.g. `5-20`); else `"---"`.
- **ResetIn** = `formatDuration(apiUsage.ResetAt - now)` if set, else `formatDuration(nextMonth1st - now)`.
- **PrimaryModel** = `shortModelName(apiUsage.ModelID)` if set; else `computePrimaryModel` = top model by Σ tokens in period from `dailyModelTokens`, else first `modelUsage` key, else `"---"`.
- **Status** = `computeStatus` (see below).
- **LimitExceeded** = `apiUsage.LimitExceeded` when live present (recomputed each read → un-sticks after reset); else `notifs.exceed_max_limit.triggered` (sticky; only consulted when no live file).
- **SubscriptionType** = `UPPER(creds.subscriptionType)`. **HourlyPercent** = `apiUsage.HourlyPercent` (`-1` if none). **HourlyResetIn** = `formatDuration(HourlyResetAt-now)` or `"---"`. **LastUpdated** = `now.UnixMilli()`.

Helpers — exact semantics to preserve:
- `clampPct(v)` → clamp to `[0,1]`.
- `shortModelName(full)`: lower-case match `opus|sonnet|haiku` → family; version via regex `-(\d+)-(\d+)` → `"OPUS 4.7"`; default → `UPPER(full[:8])` (byte slice) or full; `""` → `"---"`.
- `computeStatus(sessions, now)`: **BUSY** if any session `status != "idle"` AND (`age < 5 min` OR `pid > 0 && isPidRunning(pid)`); else **IDLE** if any session `updatedAt` within 60 min; else **OFFLINE**.
- `formatDuration(d)`: `≤0`→`"NOW"`; `days>0`→`"%dD %dH"`; else `"%dH %dM"`. Integer truncation toward zero (`int(d.Hours())`).
- `isPidRunning`: unix `Signal(0)` via `os.FindProcess`+`Signal`; windows `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess == 259 (STILL_ACTIVE)`.

`BarData` (JSON → frontend): `accountName, subscriptionType, periodMessages(i64), periodPercent(0–1), periodMsgLimit(i64), lastDataLabel, lastDataMsgs(int), hourlyPercent(0–1 or -1), hourlyResetIn, resetIn, primaryModel, status, limitExceeded, lastUpdated(ms)`.

## Rust/Slint design

New workspace lib crate **`clawd-claude`** (pure data engine, no UI deps), modules:

```
clawd-claude/
  src/
    model.rs    # serde structs mirroring every JSON shape (1:1 with types.go/api.go)
    reader.rs   # read_stats_cache / _credentials / _sessions / _notifications / _usage  → Option<…>
    fetch.rs    # async live_usage + parse_usage_response + UsageCache + kill-switch
    compute.rs  # BarData + compute_bar_data + helpers (short_model_name, compute_status, format_duration)
    process.rs  # is_pid_running  (#[cfg(unix)] / #[cfg(windows)])
    engine.rs   # BarEngine: background poll task → Slint property push
    lib.rs      # load_bar_data() / load_bar_data_at() / get_status()
```

### Structs (`model.rs`)

Mirror Go with serde; tolerant by default (`Option<T>`, `#[serde(default)]`). Key attrs:

```rust
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]          // stats-cache.json uses camelCase
struct StatsCache { version: i64, last_computed_date: String,
    daily_activity: Vec<DailyActivity>, daily_model_tokens: Vec<DailyModelTokens>,
    model_usage: HashMap<String, ModelUsageDetail>, /* … */ }

#[derive(Deserialize, Default)]             // rate_limits.json is snake_case
struct RateLimitsFile {
    five_hour: Option<RateLimitWindow>, seven_day: Option<RateLimitWindow>,
    model_id: Option<String>, model: Option<RateLimitModel>, captured_at: Option<i64>,
    rate_limits: Option<NestedWindows>,     // fallback: full statusline dump
}
struct RateLimitWindow { used_percentage: f64, resets_at: i64 }   // resets_at = Unix sec
```

`ApiUsage` is the internal normalized form (`weekly_percent: f64` with `-1.0` sentinel, `reset_at: Option<DateTime<Local>>`, `limit_exceeded`, `model_id`). Use `f64` for percents (matches Go `float64`); keep counts as `i64`.

### Live fetch (`fetch.rs`)

```rust
async fn http_fetch_usage(token: &str, client: &reqwest::Client) -> Option<ApiUsage>;
fn parse_usage_response(body: &[u8]) -> Option<ApiUsage>;   // pure, unit-tested like Go
struct UsageCache(Mutex<HashMap<String, (DateTime<Local>, ApiUsage)>>);  // soft 60s / hard 5m
```

- `reqwest::Client` built once, `.timeout(Duration::from_secs(10))`; hard-coded URL string (no base-url indirection — same as Go). Headers set per call. Read body with a ~1 MiB cap.
- Inject `fetch_fn` as a boxed async closure (`type FetchFn = Arc<dyn Fn(String) -> BoxFuture<'static, Option<ApiUsage>> + Send + Sync>`) so tests stub it — direct analogue of the `fetchLiveUsage` package var.
- Clock injected as `now: DateTime<Local>` argument to `load_bar_data_at` and the cache, matching `loadBarDataAt`.

### Entry points (`lib.rs`)

```rust
pub async fn load_bar_data(account_path: &Path, account_name: &str) -> BarData;     // = LoadBarData
async fn load_bar_data_at(path, name, now: DateTime<Local>, fetch: &FetchFn) -> BarData;
pub fn get_status(account_path: &Path) -> String;                                   // = GetStatus
```

`load_bar_data_at`: read 5 files → if creds live & not disabled, `live_usage()` (override + backfill model id) → `compute_bar_data(...)`. Pure compute stays sync; only the fetch is async.

### Threading & Slint binding (`engine.rs`)

Replaces the Wails `GetBarData()` poll + `claude:status` event with a `BarEngine` owning a tokio runtime handle and a `slint::Weak<MainWindow>`:

- **Bar task** — `tokio::time::interval(refresh_seconds)` (default 15 s) → `load_bar_data` → push to UI via `weak.upgrade_in_event_loop(move |ui| ui.global::<ClaudeBar>().set_data(bar.into()))`.
- **Status task** — `interval(500 ms)` → `get_status` → only push `ClaudeBar.set_status(...)` when value changes (mirrors `watchClaudeStatus`' change-gate + the JS refresh-on-status-event).
- **Account switch** — `set_account(path, name)` swaps the watched dir and triggers an immediate bar refresh (replaces `SetActiveAccount` → re-poll).

Slint contract (lives here as the shared interface the **ui-slint** slice renders):

```slint
export struct ClaudeBarData {
    account-name: string, subscription-type: string,
    period-messages: int, period-percent: float, period-msg-limit: int,
    last-data-label: string, last-data-msgs: int,
    hourly-percent: float, hourly-reset-in: string,
    reset-in: string, primary-model: string, limit-exceeded: bool, last-updated: int,
}
export global ClaudeBar {
    in property <ClaudeBarData> data;
    in property <string> status;          // BUSY / IDLE / OFFLINE, pushed separately (500ms path)
}
```

Rust→Slint conversion in a `From<BarData> for ClaudeBarData` impl. `i64` counts narrow to Slint `int` (i32) — fine for message counts; `last-updated`/`period-messages` overflow guarded (clamp or carry as formatted `string` if the ui slice prefers).

## Crate picks

- **serde / serde_json** — JSON decode mirroring Go's tolerant `encoding/json` (Option + default).
- **reqwest** (`rustls-tls`) — async GET, custom headers, timeout; direct analogue of `net/http`.
- **tokio** (`rt-multi-thread,time,sync,macros`) — runtime, `interval` (= `time.NewTicker`), `Mutex` (= cache mutex).
- **chrono** — `DateTime<Local>`, Unix sec/ms conversions, RFC3339 parse, local-tz month boundary. (Go uses `time.Time`/`time.Unix`/`time.Parse`/`time.Date` in local zone.)
- **regex + once_cell/OnceLock** — `shortModelName`'s `-(\d+)-(\d+)`.
- **dirs** (or **directories**) — resolve default `~/.claude` (= Go `os.UserHomeDir`).
- **nix** (`#[cfg(unix)]`) — `signal::kill(Pid, None)` ⇒ signal-0 liveness check.
- **windows-sys** (`#[cfg(windows)]`) — `OpenProcess`/`GetExitCodeProcess`/`CloseHandle`, `STILL_ACTIVE = 259`.
- **tracing** (or `log`) — replaces `log.Printf`.
- **notify** (optional) — watch `sessions/` + `rate_limits.json` to drive status faster than the 500 ms tick (see risks).

## 1:1 fidelity risks

- **Date compare is lexical, not parsed.** Go does `day.Date >= periodStartStr` on the raw `"YYYY-MM-DD"` strings. Keep `&str` comparison in Rust (don't parse) so malformed-date ordering is byte-identical.
- **Local-tz month boundary.** `time.Date(y, month+1, 1, …, now.Location())` rolls Dec→Jan automatically. chrono must replicate: build first-of-month in `Local`, add `chrono::Months::new(1)` for the reset fallback. Watch DST and `Local` resolution.
- **`format_duration` integer truncation.** Go `int(d.Hours())` truncates toward zero. Compute from `d.num_seconds() as i64` with integer div; reproduce exact `"NOW"/"%dD %dH"/"%dH %dM"` strings (the golden test pins `"3D 4H"`, `"2H 30M"`, `"12H 0M"`).
- **`shortModelName` byte slice `full[:8]`.** Go slices bytes; model ids are ASCII so safe, but use a char-boundary-safe truncation in Rust to avoid panics on unexpected input.
- **Proxy behavior.** The bypass is of `ANTHROPIC_BASE_URL` (a Claude-Code var), achieved by hard-coding the host — *not* of OS `HTTP(S)_PROXY`. Go's default transport still honors `HTTP_PROXY`. So **leave reqwest's default proxy behavior on** (do *not* call `.no_proxy()`) to stay equivalent; only the hard-coded URL matters.
- **Percent type & sentinels.** Keep `f64` and the `-1.0`/`>=0` "available" convention, and the `PeriodMsgLimit == 1` display sentinel — the bar/ui slice depends on these exact values.
- **Global cache → instance.** Go's package-global `usageCache` becomes engine-owned `Arc<Mutex<…>>` keyed by token; semantics preserved (cross-account, soft/hard TTL), without a `static`.
- **`int64` → Slint `int`.** Slint `int` is i32; narrow message counts/`last_updated` deliberately or pass as string. Decide jointly with **ui-slint**.

## Effort

**M.** Mostly pure compute + serde + one async GET; logic is small and already test-pinned. Port the Go golden tests (`bardata_test.go`, `bardata_fallback_test.go`, `usage_fetch_test.go`) and reuse `testdata/*` verbatim as the acceptance gate.

**Ordering / dependencies:** land **early** — this slice defines the `ClaudeBarData` struct + `ClaudeBar` global that **ui-slint** renders and that the **app-shell** slice wires (engine spawn, account switch, refresh interval from config). No upstream deps within the rewrite; `process.rs` reuses the same per-OS split as `process_other.go`/`process_windows.go`. The statusline wrapper that writes `rate_limits.json` stays an installer concern (packaging slice), unchanged.
