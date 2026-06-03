package audio

import "errors"

type State string

const (
	StateIdle    State = "idle"
	StateLoading State = "loading"
	StatePlaying State = "playing"
	StatePaused  State = "paused"
	StateError   State = "error"
	// StateEnded means the current track played to its natural end (EOS).
	// Distinct from idle/paused so the station player can auto-advance.
	// Livestreams (HLS) never emit it.
	StateEnded State = "ended"
)

type Event struct {
	State   State  `json:"state"`
	VideoID string `json:"videoID,omitempty"`
	Err     string `json:"error,omitempty"`
	// StationIdx is stamped by the station player on events it forwards to the
	// frontend, so the UI can filter to the active station. The audio layer
	// itself leaves it at 0.
	StationIdx int `json:"stationIdx"`
	// Position/Duration are the current track's playhead and total length in
	// seconds, used by the bar's seek timeline. Duration is 0 for livestreams
	// (and briefly before a VOD's NaturalDuration is known).
	Position float64 `json:"position,omitempty"`
	Duration float64 `json:"duration,omitempty"`
	// Progress marks a throttled position tick (same playback state, advanced
	// playhead) rather than a state transition. The station player forwards
	// these straight through without running its advance/skip logic, and the
	// frontend uses them to move the timeline without touching the status UI.
	Progress bool `json:"progress,omitempty"`
}

type Player interface {
	Play(url string) error
	// Resume continues the currently-loaded track from its paused position
	// (distinct from Play, which loads a source and starts from the beginning).
	Resume() error
	Pause() error
	Stop() error
	SetVolume(v float64) error // 0..1
	// Seek jumps the current track's playhead to the given offset in seconds.
	// No-op / best-effort for livestreams and on backends that don't support it.
	Seek(seconds float64) error
	Close() error
}

var ErrUnsupported = errors.New("audio: unsupported platform")
