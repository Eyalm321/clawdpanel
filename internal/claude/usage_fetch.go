package claude

import (
	"encoding/json"
	"io"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"
)

// Live usage is fetched straight from Anthropic's OAuth usage endpoint rather
// than relying on the rate_limits.json file the statusline hook captures. The
// file path breaks under a proxy: Claude Code asks the *configured base URL*
// (ANTHROPIC_BASE_URL — e.g. a Headroom proxy on localhost) for utilization, and
// most proxies don't serve /api/oauth/usage, so the windows stop refreshing and
// the meter freezes. Going to the real host directly keeps usage live no matter
// what proxy Claude Code itself is pointed at.
//
// The file reader (readUsage) stays as the offline fallback: when the fetch
// fails (no network, expired token, proxy-only environment with no direct
// egress) the bar still shows the last statusline capture.
const (
	usageEndpoint = "https://api.anthropic.com/api/oauth/usage"
	oauthBeta     = "oauth-2025-04-20"
	apiVersion    = "2023-06-01"

	// softTTL: reuse a successful fetch without hitting the network. The poll
	// runs every refreshSeconds (default 15s); the windows move on the order of
	// minutes, so one call per minute is plenty and keeps us a polite client.
	usageSoftTTL = 60 * time.Second
	// hardTTL: when a refresh fails, keep serving the last good live value for
	// up to this long before giving up and falling back to the captured file.
	usageHardTTL = 5 * time.Minute
)

// fetchLiveUsage is the raw fetch-and-parse step, indirected through a package
// var so tests can stub it (and so the cache in liveUsage can wrap it). Tests
// replace it with a nil-returning stub to keep the suite off the network — see
// usage_fetch_test.go.
var fetchLiveUsage = httpFetchUsage

// liveUsageDisabled reports whether the env kill-switch is set. Direct fetching
// is on by default; CLAWDPANEL_DISABLE_LIVE_USAGE=1 reverts to file-only.
func liveUsageDisabled() bool {
	switch strings.ToLower(strings.TrimSpace(os.Getenv("CLAWDPANEL_DISABLE_LIVE_USAGE"))) {
	case "1", "true", "yes", "on":
		return true
	}
	return false
}

type cachedUsage struct {
	at   time.Time
	data *APIUsage
}

var (
	usageMu    sync.Mutex
	usageCache = map[string]cachedUsage{} // keyed by access token
)

// liveUsage returns the most recent authoritative usage for the token, fetching
// from Anthropic at most once per usageSoftTTL. On a failed refresh it serves
// the last good value within usageHardTTL, otherwise nil (→ file fallback).
// now is injected so the cache is deterministic under test.
func liveUsage(token string, now time.Time) *APIUsage {
	usageMu.Lock()
	cached, ok := usageCache[token]
	usageMu.Unlock()

	if ok && now.Sub(cached.at) < usageSoftTTL {
		return cached.data
	}

	if fresh := fetchLiveUsage(token, now); fresh != nil {
		usageMu.Lock()
		usageCache[token] = cachedUsage{at: now, data: fresh}
		usageMu.Unlock()
		return fresh
	}

	// Refresh failed — keep the last good live value while it's still recent.
	if ok && now.Sub(cached.at) < usageHardTTL {
		return cached.data
	}
	return nil
}

// usageWindow mirrors one window of the /api/oauth/usage response. Note the
// field names differ from rate_limits.json: utilization is a 0–100 float (not
// used_percentage) and resets_at is an RFC3339 string (not a Unix epoch).
type usageWindow struct {
	Utilization float64 `json:"utilization"`
	ResetsAt    string  `json:"resets_at"`
}

type usageResponse struct {
	FiveHour *usageWindow `json:"five_hour"`
	SevenDay *usageWindow `json:"seven_day"`
}

// parseUsageResponse maps the endpoint payload onto the shared APIUsage shape
// computeBarData already consumes. Pure and clock-injected for tests. Returns
// nil if the body has neither window (so callers fall back to the file).
func parseUsageResponse(body []byte) *APIUsage {
	var r usageResponse
	if err := json.Unmarshal(body, &r); err != nil {
		return nil
	}
	if r.FiveHour == nil && r.SevenDay == nil {
		return nil
	}

	out := &APIUsage{WeeklyPercent: -1, HourlyPercent: -1}
	if r.SevenDay != nil {
		out.WeeklyPercent = clampPct(r.SevenDay.Utilization / 100.0)
		if t, ok := parseResetTime(r.SevenDay.ResetsAt); ok {
			out.ResetAt = t
		}
	}
	if r.FiveHour != nil {
		out.HourlyPercent = clampPct(r.FiveHour.Utilization / 100.0)
		if t, ok := parseResetTime(r.FiveHour.ResetsAt); ok {
			out.HourlyResetAt = t
		}
	}
	if out.WeeklyPercent >= 1.0 {
		out.LimitExceeded = true
	}
	// The usage endpoint carries no current-model id; ModelID is left empty and
	// the caller backfills it from the captured file so the model badge persists.
	return out
}

func parseResetTime(s string) (time.Time, bool) {
	if s == "" {
		return time.Time{}, false
	}
	if t, err := time.Parse(time.RFC3339, s); err == nil {
		return t, true
	}
	return time.Time{}, false
}

// httpFetchUsage performs the authenticated GET against the real Anthropic host,
// deliberately ignoring ANTHROPIC_BASE_URL so a proxy in Claude Code's path
// can't intercept it. Any error (network, non-200, unparseable) returns nil and
// the bar falls back to the captured file.
func httpFetchUsage(token string, now time.Time) *APIUsage {
	if token == "" {
		return nil
	}
	req, err := http.NewRequest(http.MethodGet, usageEndpoint, nil)
	if err != nil {
		return nil
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("anthropic-beta", oauthBeta)
	req.Header.Set("anthropic-version", apiVersion)
	req.Header.Set("User-Agent", "ClawdPanel")

	client := &http.Client{Timeout: 10 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return nil
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil
	}
	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return nil
	}
	return parseUsageResponse(body)
}
