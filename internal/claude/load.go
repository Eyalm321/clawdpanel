package claude

import "time"

// LoadBarData turns a single Claude account directory into the display payload
// shown on the bar. It is the one public entry point of this package: it reads
// the five per-account files (stats cache, credentials, sessions, notification
// states, captured rate limits) and runs the pure computation over them. The
// individual readers and the compute step are package internals — callers point
// LoadBarData at an account path and get back a *BarData.
//
// Missing or unreadable files are tolerated: each reader falls back to nil and
// the computation degrades gracefully (this mirrors the previous behaviour, in
// which read errors were ignored). The error return is reserved for future
// hard-failure cases; today LoadBarData always succeeds.
func LoadBarData(accountPath, accountName string) (*BarData, error) {
	return loadBarDataAt(accountPath, accountName, time.Now())
}

// loadBarDataAt is the testable core of LoadBarData with the clock injected, so
// fixtures can be asserted deterministically regardless of when the test runs.
func loadBarDataAt(accountPath, accountName string, now time.Time) (*BarData, error) {
	sc, _ := readStatsCache(accountPath)
	creds, _ := readCredentials(accountPath)
	sessions := readSessions(accountPath)
	notifs := readNotifications(accountPath)
	apiUsage := readUsage(accountPath)

	// Prefer authoritative usage fetched straight from Anthropic. It bypasses
	// any ANTHROPIC_BASE_URL proxy (e.g. Headroom), which typically doesn't relay
	// /api/oauth/usage and so leaves the captured rate_limits.json frozen. The
	// file (apiUsage) stays as the offline fallback when the fetch is unavailable.
	if creds != nil && !liveUsageDisabled() {
		oauth := creds.ClaudeAiOauth
		tokenLive := oauth.ExpiresAt == 0 || now.UnixMilli() < oauth.ExpiresAt
		if oauth.AccessToken != "" && tokenLive {
			if live := liveUsage(oauth.AccessToken, now); live != nil {
				// The usage endpoint carries no model id; keep the file's so the
				// model badge doesn't blank out when live data takes over.
				if live.ModelID == "" && apiUsage != nil {
					live.ModelID = apiUsage.ModelID
				}
				apiUsage = live
			}
		}
	}

	return computeBarData(accountName, sc, creds, sessions, notifs, apiUsage, now), nil
}

// GetStatus returns the computed active status for the account path.
func GetStatus(accountPath string) string {
	sessions := readSessions(accountPath)
	return computeStatus(sessions, time.Now())
}
