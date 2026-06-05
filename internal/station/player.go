package station

import (
	"context"
	"errors"
	"log"
	"math/rand"
	"sync"
	"time"

	"clawdpanel/internal/audio"
	"clawdpanel/internal/config"
)

func init() {
	rand.Seed(time.Now().UnixNano())
}

var errStationRange = errors.New("station index out of range")

// trackController is the slice of audio.Controller the station player drives.
// Kept as an interface so the queue logic can be unit-tested with a fake.
type trackController interface {
	PlayVideo(ctx context.Context, videoID string) error
	Resume() error
	Pause() error
	Stop() error
	SetVolume(v float64) error
}

// playlistExpander is the slice of radio.Resolver used to expand playlists into
// ordered video IDs. (Per-track URL resolution happens inside the controller.)
type playlistExpander interface {
	ExpandPlaylist(ctx context.Context, playlistID string, forceRefresh bool) ([]string, error)
}

// maxFailStreak caps how many consecutive dead tracks we skip before declaring
// a station unavailable, regardless of queue length.
const maxFailStreak = 25

// StationPlayer owns the radio queue: it flattens a station's items (expanding
// playlists), optionally shuffles, and auto-advances + loops by driving a
// single-track audio.Controller one PlayVideo at a time. It sits ABOVE the
// controller; the controller stays single-track and owns URL resolution/retry.
type StationPlayer struct {
	ctrl     trackController
	resolver playlistExpander
	emit     func(audio.Event) // forward (enriched) events to the frontend

	mu         sync.Mutex
	stations   []config.StationConfig
	activeIdx  int
	queue      []string // ordered video IDs (livestreams included)
	cur        int
	shuffle    bool
	paused     bool
	failStreak int

	// epoch is bumped on every station (re)start so stale background playlist
	// expansions and in-flight play goroutines no-op once superseded.
	epoch        uint64
	cancelExpand context.CancelFunc
}

// New builds a StationPlayer wrapping the given controller and playlist
// expander. emit forwards events to the frontend.
func New(ctrl *audio.Controller, res playlistExpander, emit func(audio.Event)) *StationPlayer {
	return &StationPlayer{ctrl: ctrl, resolver: res, emit: emit}
}

// newWithController is the test seam: it accepts the trackController interface
// directly so a fake can be injected.
func newWithController(ctrl trackController, res playlistExpander, emit func(audio.Event)) *StationPlayer {
	return &StationPlayer{ctrl: ctrl, resolver: res, emit: emit}
}

// SetStations replaces the known station list (called on config load/save).
func (s *StationPlayer) SetStations(st []config.StationConfig) {
	s.mu.Lock()
	s.stations = st
	s.mu.Unlock()
}

// Play (re)starts the station at stationIdx. If it is already the active
// station and a queue exists, it resumes the current track from its paused
// position instead of rebuilding (so pause→play keeps your exact place).
func (s *StationPlayer) Play(stationIdx int) error {
	s.mu.Lock()
	if stationIdx < 0 || stationIdx >= len(s.stations) {
		s.mu.Unlock()
		return errStationRange
	}

	// Resume same station from the exact paused position (no reshuffle, no
	// restart — the controller continues the already-loaded track).
	if stationIdx == s.activeIdx && len(s.queue) > 0 {
		s.paused = false
		s.mu.Unlock()
		return s.ctrl.Resume()
	}

	// Switch / fresh start.
	s.epoch++
	epoch := s.epoch
	if s.cancelExpand != nil {
		s.cancelExpand()
	}
	ctx, cancel := context.WithCancel(context.Background())
	s.cancelExpand = cancel
	s.activeIdx = stationIdx
	station := s.stations[stationIdx]
	s.shuffle = station.Shuffle
	s.queue = nil
	s.cur = 0
	s.failStreak = 0
	s.paused = false
	s.mu.Unlock()

	// Loading feedback while the (possibly playlist-backed) queue is built.
	s.forward(audio.Event{State: audio.StateLoading, StationIdx: stationIdx})
	go s.buildAndStart(epoch, station, ctx)
	return nil
}

// buildAndStart flattens the station into the queue and starts playback. For
// sequential stations it appends incrementally and starts the moment the first
// track is known; for shuffled stations it expands everything, then starts at a
// random track (so the shuffle covers the whole collection).
func (s *StationPlayer) buildAndStart(epoch uint64, station config.StationConfig, ctx context.Context) {
	started := false
	for _, item := range station.Items {
		if s.epochChanged(epoch) {
			return
		}

		// Re-parse on the fly to dynamically upgrade any legacy saved items
		actualItem := item
		if item.Raw != "" {
			if parsed, err := ParseItem(item.Raw); err == nil {
				actualItem = parsed
			}
		}

		var ids []string
		switch actualItem.Kind {
		case config.ItemPlaylist:
			got, err := s.resolver.ExpandPlaylist(ctx, actualItem.ID, false)
			if err != nil {
				log.Printf("[station] expand playlist %s failed: %v", actualItem.ID, err)
				continue
			}
			ids = got
		default:
			if actualItem.ID != "" {
				ids = []string{actualItem.ID}
			}
		}
		if len(ids) == 0 {
			continue
		}

		s.mu.Lock()
		if epoch != s.epoch {
			s.mu.Unlock()
			return
		}
		s.queue = append(s.queue, ids...)
		startNow := !started && !station.Shuffle
		var startID string
		if startNow {
			s.cur = 0
			startID = s.queue[0]
		}
		s.mu.Unlock()

		if startNow {
			started = true
			go s.playTrack(epoch, startID)
		}
	}

	// Finalize: shuffle-start at a random track, or report an empty station.
	s.mu.Lock()
	if epoch != s.epoch {
		s.mu.Unlock()
		return
	}
	if len(s.queue) == 0 {
		activeIdx := s.activeIdx
		s.mu.Unlock()
		s.forward(audio.Event{State: audio.StateError, StationIdx: activeIdx, Err: "station has no playable items"})
		return
	}
	if station.Shuffle && !started {
		s.cur = rand.Intn(len(s.queue))
		startID := s.queue[s.cur]
		s.mu.Unlock()
		go s.playTrack(epoch, startID)
		return
	}
	s.mu.Unlock()
}

// playTrack resolves+plays a single track via the controller, unless the epoch
// has moved on. Failures surface as a StateError event the controller emits,
// which OnAudioEvent turns into skip-on-failure.
func (s *StationPlayer) playTrack(epoch uint64, id string) {
	if s.epochChanged(epoch) {
		return
	}
	if err := s.ctrl.PlayVideo(context.Background(), id); err != nil {
		log.Printf("[station] play %s failed: %v", id, err)
	}
}

// OnAudioEvent receives every audio.Controller event. It auto-advances on
// natural end, skips dead tracks on terminal error, and forwards events to the
// frontend stamped with the active station index.
func (s *StationPlayer) OnAudioEvent(ev audio.Event) {
	const (
		actNone = iota
		actAdvance
		actSkip
		actGiveUp
	)

	// Progress ticks (advancing playhead, same playback state) are forwarded
	// straight to the frontend stamped with the active station — they must not
	// run the advance/skip/failStreak machine.
	if ev.Progress {
		s.mu.Lock()
		ev.StationIdx = s.activeIdx
		s.mu.Unlock()
		s.forward(ev)
		return
	}

	s.mu.Lock()
	activeIdx := s.activeIdx
	epoch := s.epoch
	var curID string
	if s.cur >= 0 && s.cur < len(s.queue) {
		curID = s.queue[s.cur]
	}

	action := actNone
	switch ev.State {
	case audio.StatePlaying:
		s.failStreak = 0
		s.paused = false
	case audio.StatePaused:
		s.paused = true
	case audio.StateEnded:
		// Natural end of the currently-playing track → advance + loop.
		if curID == "" || ev.VideoID == "" || ev.VideoID == curID {
			s.failStreak = 0
			action = actAdvance
		}
	case audio.StateError:
		// Terminal error for the current track (controller already retried
		// once) → skip to the next, or give up if the whole queue is dead.
		if curID != "" && (ev.VideoID == "" || ev.VideoID == curID) {
			s.failStreak++
			if s.failStreak >= s.failLimitLocked() {
				action = actGiveUp
			} else {
				action = actSkip
			}
		}
	}

	var nextID string
	if action == actAdvance || action == actSkip {
		if id, ok := s.advanceLocked(); ok {
			nextID = id
		} else {
			action = actNone
		}
	}
	s.mu.Unlock()

	// Forward to the frontend. Suppress the raw error while skipping a dead
	// track (transient — don't flicker [ERR]); emit a clear terminal error
	// only when we give up on the whole station.
	switch action {
	case actNone, actAdvance:
		fwd := ev
		fwd.StationIdx = activeIdx
		s.forward(fwd)
	case actGiveUp:
		// Whole queue is dead: fully stop (clears queue + bumps epoch so any
		// in-flight skip goroutines no-op) and report a clear terminal error.
		_ = s.Stop()
		s.forward(audio.Event{State: audio.StateError, StationIdx: activeIdx, Err: "station unavailable"})
	}

	if action == actAdvance || action == actSkip {
		go s.playTrack(epoch, nextID)
	}
}

// advanceLocked moves cur to the next track. With shuffle on it jumps to a
// random track other than the current one (so auto-advance order is random with
// no immediate repeats); otherwise it steps sequentially, wrapping to 0 (loop).
// Caller holds s.mu.
func (s *StationPlayer) advanceLocked() (string, bool) {
	n := len(s.queue)
	if n == 0 {
		return "", false
	}
	if s.shuffle && n > 1 {
		// Pick uniformly among the n-1 tracks that aren't current.
		next := rand.Intn(n - 1)
		if next >= s.cur {
			next++
		}
		s.cur = next
	} else {
		s.cur++
		if s.cur >= n {
			s.cur = 0
		}
	}
	return s.queue[s.cur], true
}

// retreatLocked moves cur to the previous track, wrapping to the end. It always
// steps sequentially (there's no shuffle play-history to walk back through), so
// it's the natural inverse of "next track". Caller holds s.mu.
func (s *StationPlayer) retreatLocked() (string, bool) {
	n := len(s.queue)
	if n == 0 {
		return "", false
	}
	s.cur--
	if s.cur < 0 {
		s.cur = n - 1
	}
	return s.queue[s.cur], true
}

// failLimitLocked is the skip-on-failure threshold: never more than the queue
// length, capped at maxFailStreak, and at least 1. Caller holds s.mu.
func (s *StationPlayer) failLimitLocked() int {
	n := len(s.queue)
	if n > maxFailStreak {
		n = maxFailStreak
	}
	if n < 1 {
		n = 1
	}
	return n
}

func (s *StationPlayer) epochChanged(e uint64) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.epoch != e
}

// Pause pauses playback (keeps the queue/position).
func (s *StationPlayer) Pause() error {
	s.mu.Lock()
	s.paused = true
	s.mu.Unlock()
	return s.ctrl.Pause()
}

// SetShuffle sets the shuffle mode for the station at stationIdx (in memory;
// config persistence is the app's job). It is a pure mode toggle: it records the
// flag and, for the active station, updates the live shuffle state so subsequent
// auto-advance (and manual Next) is randomized. It never starts, stops, or jumps
// playback — toggling shuffle while paused stays paused, and while playing the
// current track keeps playing until it ends.
func (s *StationPlayer) SetShuffle(stationIdx int, on bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if stationIdx >= 0 && stationIdx < len(s.stations) {
		s.stations[stationIdx].Shuffle = on
	}
	if stationIdx == s.activeIdx {
		s.shuffle = on
	}
	return nil
}

// Next manually advances to the next track within the active station.
func (s *StationPlayer) Next() error {
	s.mu.Lock()
	if len(s.queue) == 0 {
		s.mu.Unlock()
		return nil
	}
	id, ok := s.advanceLocked()
	epoch := s.epoch
	s.mu.Unlock()
	if ok {
		go s.playTrack(epoch, id)
	}
	return nil
}

// Prev manually steps back to the previous track within the active station.
func (s *StationPlayer) Prev() error {
	s.mu.Lock()
	if len(s.queue) == 0 {
		s.mu.Unlock()
		return nil
	}
	id, ok := s.retreatLocked()
	epoch := s.epoch
	s.mu.Unlock()
	if ok {
		go s.playTrack(epoch, id)
	}
	return nil
}

// Stop halts playback and clears the queue, cancelling any in-flight expansion.
func (s *StationPlayer) Stop() error {
	s.mu.Lock()
	s.epoch++
	if s.cancelExpand != nil {
		s.cancelExpand()
		s.cancelExpand = nil
	}
	s.queue = nil
	s.cur = 0
	s.failStreak = 0
	s.paused = false
	s.mu.Unlock()
	return s.ctrl.Stop()
}

// SetVolume delegates to the controller. Persistence of RadioVolume is the
// app's responsibility (it owns the config).
func (s *StationPlayer) SetVolume(v float64) error {
	return s.ctrl.SetVolume(v)
}

func (s *StationPlayer) forward(ev audio.Event) {
	if s.emit != nil {
		s.emit(ev)
	}
}
