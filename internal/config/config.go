package config

import (
	"encoding/json"
	"os"
	"path/filepath"
	"runtime"
)

type AccountConfig struct {
	Name string `json:"name"`
	Path string `json:"path"`
}

type HotkeyConfig struct {
	CycleMonitor       string `json:"cycleMonitor"`
	ToggleClickThrough string `json:"toggleClickThrough"`
}

// StationItemKind classifies one entry in a radio station's collection. It is a
// hint from URL parsing; the resolver is authoritative on live-vs-VOD (a
// watch?v= with a non-empty HLS manifest is treated as a livestream).
type StationItemKind string

const (
	ItemVideo      StationItemKind = "video"
	ItemPlaylist   StationItemKind = "playlist"
	ItemLivestream StationItemKind = "livestream"
)

// StationItem is one YouTube source in a station: a single video/livestream
// (ID is the 11-char video id) or a playlist (ID is the list id).
type StationItem struct {
	Kind StationItemKind `json:"kind"`
	ID   string          `json:"id"`
	Raw  string          `json:"raw,omitempty"` // original user input, for the editor
}

// StationConfig is a named, ordered collection of YouTube items played as a
// radio station, with a per-station shuffle toggle. Shuffle is driven from the
// bar's shuffle button (not the settings editor) and persisted here so the
// on/off state survives restarts.
type StationConfig struct {
	Name    string        `json:"name"`
	Items   []StationItem `json:"items"`
	Shuffle bool          `json:"shuffle"`
}

// FeatureConfig toggles which optional bar segments are active. Disabling a
// feature hides its segment AND, where one exists, frees the backing resource
// rather than merely hiding the UI — Radio is the notable case: when off, the
// native audio engine (a long-lived background player process) is never started
// / is torn down. The other flags are pure show/hide of a bar segment. All
// default true (see Defaults); a missing/partial "features" object in an older
// config keeps unspecified flags enabled because Load unmarshals over Defaults.
type FeatureConfig struct {
	Radio       bool `json:"radio"`       // #seg-radio + native audio engine
	Monitor     bool `json:"monitor"`     // #seg-mon cycler
	Theme       bool `json:"theme"`       // #seg-theme cycler
	WeeklyUsage bool `json:"weeklyUsage"` // #seg-msgs + #seg-reset
	HourlyUsage bool `json:"hourlyUsage"` // #seg-hourly + #seg-hourly-reset
}

type Config struct {
	Monitor          int              `json:"monitor"`
	Theme            string           `json:"theme"`
	Opacity          float64          `json:"opacity"`
	RefreshSeconds   int              `json:"refreshSeconds"`
	BarHeight        int              `json:"barHeight"`
	ActiveAccount    int              `json:"activeAccount"`
	Accounts         []AccountConfig  `json:"accounts"`
	Hotkeys          HotkeyConfig     `json:"hotkeys"`
	StartWithWindows bool             `json:"startWithWindows"`
	ClickThrough     bool             `json:"clickThrough"`
	AppBarMode       bool             `json:"appBarMode"` // push apps down (AppBar API)
	Pinned           bool             `json:"pinned"`     // false = auto-hide on mouseleave, undocked
	Stations         []StationConfig  `json:"stations"`
	ActiveStation    int              `json:"activeStation"`
	RadioVolume      float64          `json:"radioVolume"` // 0..1, persisted
	Features         FeatureConfig    `json:"features"`    // which bar segments are enabled
}

func AppDataDir() string {
	switch runtime.GOOS {
	case "windows":
		if v := os.Getenv("APPDATA"); v != "" {
			return filepath.Join(v, "ClawdPanel")
		}
	case "darwin":
		if h, err := os.UserHomeDir(); err == nil {
			return filepath.Join(h, "Library", "Application Support", "ClawdPanel")
		}
	default:
		if v := os.Getenv("XDG_CONFIG_HOME"); v != "" {
			return filepath.Join(v, "ClawdPanel")
		}
		if h, err := os.UserHomeDir(); err == nil {
			return filepath.Join(h, ".config", "ClawdPanel")
		}
	}
	return filepath.Join(os.TempDir(), "ClawdPanel")
}

func configPath() string {
	return filepath.Join(AppDataDir(), "config.json")
}

func Defaults() Config {
	home, err := os.UserHomeDir()
	if err != nil {
		home = ""
	}
	return Config{
		Monitor:        0,
		Theme:          "terminal-green",
		Opacity:        0.92,
		RefreshSeconds: 15,
		BarHeight:      28,
		ActiveAccount:  0,
		AppBarMode:     true,
		Accounts: []AccountConfig{
			{Name: "main", Path: filepath.Join(home, ".claude")},
		},
		Hotkeys: HotkeyConfig{
			CycleMonitor:       "Ctrl+Alt+M",
			ToggleClickThrough: "Ctrl+Alt+T",
		},
		StartWithWindows: false,
		ClickThrough:     false,
		Pinned:           true,
		// The two original hardcoded stations, migrated as defaults. Both are
		// livestreams, so they never reach end-of-track and loop forever —
		// preserving the pre-collections behavior byte-for-byte.
		Stations: []StationConfig{
			{Name: "CLAUDE FM", Items: []StationItem{{Kind: ItemLivestream, ID: "YmQ7jRgf4f0"}}},
			{Name: "LOFI GIRL", Items: []StationItem{{Kind: ItemLivestream, ID: "EWrX250Zhko"}}},
		},
		ActiveStation: 0,
		RadioVolume:   1.0,
		// Every optional segment on by default — preserves the pre-toggle bar.
		Features: FeatureConfig{
			Radio:       true,
			Monitor:     true,
			Theme:       true,
			WeeklyUsage: true,
			HourlyUsage: true,
		},
	}
}

func Load() (*Config, error) {
	data, err := os.ReadFile(configPath())
	if os.IsNotExist(err) {
		cfg := Defaults()
		return &cfg, nil
	}
	if err != nil {
		return nil, err
	}
	// Unmarshal over defaults so new fields get zero values
	cfg := Defaults()
	if err := json.Unmarshal(data, &cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}

func Save(cfg *Config) error {
	dir := AppDataDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	// Atomic write: temp file + rename
	tmp := configPath() + ".tmp"
	if err := os.WriteFile(tmp, data, 0644); err != nil {
		return err
	}
	return os.Rename(tmp, configPath())
}
