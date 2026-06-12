package claude

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"
)

// APIUsage holds real-time usage data sourced from rate_limits.json,
// which is captured by our statusline wrapper inside each account dir.
// Schema mirrors the rate_limits block Claude Code passes to statusline scripts.
type APIUsage struct {
	WeeklyPercent float64   // 0.0–1.0; negative if unavailable
	HourlyPercent float64   // 0.0–1.0; negative if unavailable
	ResetAt       time.Time // seven_day reset; zero if unavailable
	HourlyResetAt time.Time // five_hour reset; zero if unavailable
	LimitExceeded bool
	ModelID       string
}

type rateLimitWindow struct {
	UsedPercentage float64 `json:"used_percentage"`
	ResetsAt       int64   `json:"resets_at"`
}

type rateLimitModel struct {
	ID          string `json:"id"`
	DisplayName string `json:"display_name"`
}

type rateLimitsFile struct {
	FiveHour   *rateLimitWindow `json:"five_hour"`
	SevenDay   *rateLimitWindow `json:"seven_day"`
	ModelID    string           `json:"model_id"`
	Model      *rateLimitModel  `json:"model"`
	CapturedAt int64            `json:"captured_at"`

	// The statusline wrapper dumps Claude Code's full statusline payload,
	// where the windows live nested under "rate_limits"; the top-level
	// fields above cover captures that store the block directly.
	RateLimits *struct {
		FiveHour *rateLimitWindow `json:"five_hour"`
		SevenDay *rateLimitWindow `json:"seven_day"`
	} `json:"rate_limits"`
}

// readUsage loads the most recent rate-limit data captured by the statusline
// wrapper. Returns nil only if the file is missing or unparseable; whatever was
// last captured is always shown, regardless of age (the bar prefers stale-but-
// real usage over a blank meter).
func readUsage(accountPath string) *APIUsage {
	data, err := os.ReadFile(filepath.Join(accountPath, "rate_limits.json"))
	if err != nil {
		return nil
	}
	var rl rateLimitsFile
	if json.Unmarshal(data, &rl) != nil {
		return nil
	}

	out := &APIUsage{WeeklyPercent: -1, HourlyPercent: -1}
	if rl.ModelID != "" {
		out.ModelID = rl.ModelID
	} else if rl.Model != nil {
		if rl.Model.ID != "" {
			out.ModelID = rl.Model.ID
		} else if rl.Model.DisplayName != "" {
			out.ModelID = rl.Model.DisplayName
		}
	}

	if rl.RateLimits != nil {
		if rl.SevenDay == nil {
			rl.SevenDay = rl.RateLimits.SevenDay
		}
		if rl.FiveHour == nil {
			rl.FiveHour = rl.RateLimits.FiveHour
		}
	}
	if rl.SevenDay != nil {
		out.WeeklyPercent = clampPct(rl.SevenDay.UsedPercentage / 100.0)
		if rl.SevenDay.ResetsAt > 0 {
			out.ResetAt = time.Unix(rl.SevenDay.ResetsAt, 0)
		}
	}
	if rl.FiveHour != nil {
		out.HourlyPercent = clampPct(rl.FiveHour.UsedPercentage / 100.0)
		if rl.FiveHour.ResetsAt > 0 {
			out.HourlyResetAt = time.Unix(rl.FiveHour.ResetsAt, 0)
		}
	}
	if out.WeeklyPercent >= 1.0 {
		out.LimitExceeded = true
	}
	return out
}

func clampPct(v float64) float64 {
	if v < 0 {
		return 0
	}
	if v > 1.0 {
		return 1.0
	}
	return v
}
