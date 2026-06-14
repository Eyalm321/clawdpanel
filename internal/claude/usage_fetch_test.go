package claude

import (
	"testing"
	"time"
)

// Keep the whole package test suite off the network. loadBarDataAt now attempts
// a live fetch whenever a fixture carries an unexpired token (populated,
// no_usage), so without this stub those tests would make real HTTP calls and the
// file-derived assertions would be overridden. Individual tests that want to
// exercise the override path set fetchLiveUsage themselves and restore it.
func init() {
	fetchLiveUsage = func(string, time.Time) *APIUsage { return nil }
}

// parseUsageResponse maps the real /api/oauth/usage schema (utilization floats,
// RFC3339 resets_at) onto APIUsage. This body is a captured live response.
func TestParseUsageResponse_LiveSchema(t *testing.T) {
	body := []byte(`{
	  "five_hour":  { "utilization": 7.0,  "resets_at": "2026-06-14T16:50:00.732517+00:00" },
	  "seven_day":  { "utilization": 16.0, "resets_at": "2026-06-18T09:59:59.732539+00:00" },
	  "seven_day_opus": null,
	  "seven_day_sonnet": { "utilization": 0.0, "resets_at": "2026-06-18T09:59:59+00:00" },
	  "extra_usage": { "is_enabled": true, "monthly_limit": 20000, "used_credits": 7149.0 }
	}`)

	got := parseUsageResponse(body)
	if got == nil {
		t.Fatal("parseUsageResponse returned nil for a valid body")
	}
	if got.WeeklyPercent != 16.0/100.0 {
		t.Errorf("WeeklyPercent = %v, want 0.16", got.WeeklyPercent)
	}
	if got.HourlyPercent != 7.0/100.0 {
		t.Errorf("HourlyPercent = %v, want 0.07", got.HourlyPercent)
	}
	if got.LimitExceeded {
		t.Error("LimitExceeded = true, want false (weekly 16% < 100%)")
	}
	if got.ResetAt.IsZero() || got.ResetAt.Year() != 2026 {
		t.Errorf("ResetAt = %v, want a parsed 2026 timestamp", got.ResetAt)
	}
	if got.HourlyResetAt.IsZero() {
		t.Error("HourlyResetAt is zero, want the five_hour reset parsed")
	}
	// The endpoint has no model id; the caller backfills it from the file.
	if got.ModelID != "" {
		t.Errorf("ModelID = %q, want empty (endpoint carries no model)", got.ModelID)
	}
}

func TestParseUsageResponse_LimitExceeded(t *testing.T) {
	got := parseUsageResponse([]byte(`{"seven_day":{"utilization":100,"resets_at":""}}`))
	if got == nil {
		t.Fatal("nil for a 100% body")
	}
	if !got.LimitExceeded {
		t.Error("LimitExceeded = false, want true at 100% weekly")
	}
	if got.WeeklyPercent != 1.0 {
		t.Errorf("WeeklyPercent = %v, want 1.0", got.WeeklyPercent)
	}
}

func TestParseUsageResponse_Empty(t *testing.T) {
	// No windows present (or garbage) → nil so callers fall back to the file.
	if got := parseUsageResponse([]byte(`{"seven_day_opus":null}`)); got != nil {
		t.Errorf("got %+v, want nil for a body with no five_hour/seven_day", got)
	}
	if got := parseUsageResponse([]byte(`not json`)); got != nil {
		t.Errorf("got %+v, want nil for malformed body", got)
	}
}

// Live usage overrides the captured file, and the model id (absent from the
// endpoint) is backfilled from the file so the badge persists.
func TestLoadBarData_LiveOverridesFile(t *testing.T) {
	orig := fetchLiveUsage
	t.Cleanup(func() {
		fetchLiveUsage = orig
		usageMu.Lock()
		usageCache = map[string]cachedUsage{}
		usageMu.Unlock()
	})
	usageMu.Lock()
	usageCache = map[string]cachedUsage{}
	usageMu.Unlock()

	// populated's file says weekly 68% / model opus-4-7; the live source disagrees.
	fetchLiveUsage = func(string, time.Time) *APIUsage {
		return &APIUsage{WeeklyPercent: 0.25, HourlyPercent: 0.10, ModelID: ""}
	}

	got, err := loadBarDataAt("testdata/populated", "Acct", fixedNow)
	if err != nil {
		t.Fatalf("error: %v", err)
	}
	if got.PeriodPercent != 0.25 {
		t.Errorf("PeriodPercent = %v, want 0.25 (live must override the file's 0.68)", got.PeriodPercent)
	}
	if got.HourlyPercent != 0.10 {
		t.Errorf("HourlyPercent = %v, want 0.10 (live)", got.HourlyPercent)
	}
	if got.PeriodMsgLimit != 1 {
		t.Errorf("PeriodMsgLimit = %d, want 1 (live percent present)", got.PeriodMsgLimit)
	}
	if got.PrimaryModel != "OPUS 4.7" {
		t.Errorf("PrimaryModel = %q, want OPUS 4.7 (backfilled from file)", got.PrimaryModel)
	}
}
