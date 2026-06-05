package station

import (
	"context"
	"errors"
	"sync"
	"testing"
	"time"

	"clawdpanel/internal/audio"
	"clawdpanel/internal/config"
)

type fakeController struct {
	mu          sync.Mutex
	played      chan string
	stopCount   int
	resumeCount int
	failIDs     map[string]bool
}

func newFakeController() *fakeController {
	return &fakeController{played: make(chan string, 64), failIDs: map[string]bool{}}
}

func (f *fakeController) PlayVideo(ctx context.Context, id string) error {
	f.played <- id
	f.mu.Lock()
	fail := f.failIDs[id]
	f.mu.Unlock()
	if fail {
		return errors.New("dead track")
	}
	return nil
}
func (f *fakeController) Pause() error            { return nil }
func (f *fakeController) SetVolume(float64) error { return nil }
func (f *fakeController) Resume() error {
	f.mu.Lock()
	f.resumeCount++
	f.mu.Unlock()
	return nil
}
func (f *fakeController) resumes() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.resumeCount
}
func (f *fakeController) Stop() error {
	f.mu.Lock()
	f.stopCount++
	f.mu.Unlock()
	return nil
}
func (f *fakeController) stops() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.stopCount
}

type fakeExpander struct {
	m   map[string][]string
	err map[string]error
}

func (f *fakeExpander) ExpandPlaylist(ctx context.Context, id string, force bool) ([]string, error) {
	if f.err != nil {
		if e := f.err[id]; e != nil {
			return nil, e
		}
	}
	return f.m[id], nil
}

func nextPlayed(t *testing.T, f *fakeController) string {
	t.Helper()
	select {
	case id := <-f.played:
		return id
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for PlayVideo")
		return ""
	}
}

func waitQueueLen(t *testing.T, s *StationPlayer, want int) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		s.mu.Lock()
		n := len(s.queue)
		s.mu.Unlock()
		if n == want {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("queue never reached length %d", want)
}

func TestAdvanceLockedLoops(t *testing.T) {
	s := &StationPlayer{queue: []string{"a", "b", "c"}}
	want := []string{"b", "c", "a", "b"}
	for i, w := range want {
		got, ok := s.advanceLocked()
		if !ok || got != w {
			t.Fatalf("advance %d = %q (ok=%v), want %q", i, got, ok, w)
		}
	}
}

func TestBuildSequentialQueue(t *testing.T) {
	fc := newFakeController()
	fe := &fakeExpander{m: map[string][]string{"P": {"p1", "p2"}}}
	s := newWithController(fc, fe, func(audio.Event) {})
	s.SetStations([]config.StationConfig{{
		Name: "S",
		Items: []config.StationItem{
			{Kind: config.ItemVideo, ID: "a"},
			{Kind: config.ItemPlaylist, ID: "P"},
			{Kind: config.ItemVideo, ID: "b"},
		},
	}})

	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}
	// Sequential start: first track plays immediately.
	if got := nextPlayed(t, fc); got != "a" {
		t.Fatalf("first played = %q, want a", got)
	}
	// Playlist expands and appends in order.
	waitQueueLen(t, s, 4)
	s.mu.Lock()
	got := append([]string(nil), s.queue...)
	s.mu.Unlock()
	want := []string{"a", "p1", "p2", "b"}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("queue = %v, want %v", got, want)
		}
	}
}

func TestEndedAdvancesAndForwards(t *testing.T) {
	fc := newFakeController()
	var mu sync.Mutex
	var events []audio.State
	s := newWithController(fc, &fakeExpander{}, func(ev audio.Event) {
		mu.Lock()
		events = append(events, ev.State)
		mu.Unlock()
	})
	s.queue = []string{"a", "b"}
	s.cur = 0

	s.OnAudioEvent(audio.Event{State: audio.StateEnded, VideoID: "a"})
	if got := nextPlayed(t, fc); got != "b" {
		t.Fatalf("after ended, played = %q, want b", got)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(events) == 0 || events[0] != audio.StateEnded {
		t.Fatalf("expected ended event forwarded, got %v", events)
	}
}

func TestEndedWrapsToStartAndLoops(t *testing.T) {
	fc := newFakeController()
	s := newWithController(fc, &fakeExpander{}, func(audio.Event) {})
	s.queue = []string{"only"}
	s.cur = 0
	s.OnAudioEvent(audio.Event{State: audio.StateEnded, VideoID: "only"})
	if got := nextPlayed(t, fc); got != "only" {
		t.Fatalf("single-VOD loop played = %q, want only", got)
	}
}

func TestStaleEndedIgnored(t *testing.T) {
	fc := newFakeController()
	s := newWithController(fc, &fakeExpander{}, func(audio.Event) {})
	s.queue = []string{"a", "b"}
	s.cur = 1 // currently on "b"
	// A late StateEnded for the already-passed "a" must not advance.
	s.OnAudioEvent(audio.Event{State: audio.StateEnded, VideoID: "a"})
	select {
	case got := <-fc.played:
		t.Fatalf("stale ended caused playback of %q", got)
	case <-time.After(100 * time.Millisecond):
		// good — nothing played
	}
}

func TestSkipOnFailure(t *testing.T) {
	fc := newFakeController()
	s := newWithController(fc, &fakeExpander{}, func(audio.Event) {})
	s.queue = []string{"a", "b", "c"}
	s.cur = 0
	s.OnAudioEvent(audio.Event{State: audio.StateError, VideoID: "a", Err: "dead"})
	if got := nextPlayed(t, fc); got != "b" {
		t.Fatalf("after error, played = %q, want b (skip)", got)
	}
}

func TestGiveUpWhenAllDead(t *testing.T) {
	fc := newFakeController()
	var mu sync.Mutex
	var lastErr string
	s := newWithController(fc, &fakeExpander{}, func(ev audio.Event) {
		if ev.State == audio.StateError {
			mu.Lock()
			lastErr = ev.Err
			mu.Unlock()
		}
	})
	s.queue = []string{"a"} // single dead track → give up immediately
	s.cur = 0
	s.OnAudioEvent(audio.Event{State: audio.StateError, VideoID: "a", Err: "dead"})

	select {
	case got := <-fc.played:
		t.Fatalf("gave up but still played %q", got)
	case <-time.After(100 * time.Millisecond):
	}
	if fc.stops() == 0 {
		t.Error("expected controller Stop on give-up")
	}
	mu.Lock()
	defer mu.Unlock()
	if lastErr != "station unavailable" {
		t.Fatalf("expected 'station unavailable' error, got %q", lastErr)
	}
}

func TestPlayingResetsFailStreak(t *testing.T) {
	s := newWithController(newFakeController(), &fakeExpander{}, func(audio.Event) {})
	s.failStreak = 5
	s.OnAudioEvent(audio.Event{State: audio.StatePlaying, VideoID: "a"})
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.failStreak != 0 {
		t.Fatalf("failStreak = %d, want 0 after playing", s.failStreak)
	}
}

func TestPlayOutOfRange(t *testing.T) {
	s := newWithController(newFakeController(), &fakeExpander{}, func(audio.Event) {})
	s.SetStations([]config.StationConfig{{Name: "only"}})
	if err := s.Play(5); err == nil {
		t.Fatal("expected error for out-of-range station index")
	}
}

func TestEpochCancelsStaleExpansion(t *testing.T) {
	fc := newFakeController()
	// Expander blocks until released, simulating a slow playlist fetch.
	release := make(chan struct{})
	fe := &blockingExpander{release: release, ids: []string{"x1", "x2"}}
	s := newWithController(fc, fe, func(audio.Event) {})
	s.SetStations([]config.StationConfig{
		{Name: "A", Items: []config.StationItem{{Kind: config.ItemPlaylist, ID: "PA"}}},
		{Name: "B", Items: []config.StationItem{{Kind: config.ItemVideo, ID: "b"}}},
	})

	if err := s.Play(0); err != nil { // kicks off blocking expansion at epoch 1
		t.Fatal(err)
	}
	// Switch to station B before A's expansion returns → epoch bumps.
	if err := s.Play(1); err != nil {
		t.Fatal(err)
	}
	if got := nextPlayed(t, fc); got != "b" {
		t.Fatalf("station B should play b, got %q", got)
	}
	// Now let the stale expansion finish; its results must be discarded.
	close(release)
	time.Sleep(50 * time.Millisecond)
	s.mu.Lock()
	q := append([]string(nil), s.queue...)
	s.mu.Unlock()
	for _, id := range q {
		if id == "x1" || id == "x2" {
			t.Fatalf("stale expansion leaked into queue: %v", q)
		}
	}
}

type blockingExpander struct {
	release <-chan struct{}
	ids     []string
}

func (b *blockingExpander) ExpandPlaylist(ctx context.Context, id string, force bool) ([]string, error) {
	select {
	case <-b.release:
		return b.ids, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-time.After(2 * time.Second):
		return b.ids, nil
	}
}

func TestShuffleStartPlaysRandomQueueTrack(t *testing.T) {
	fc := newFakeController()
	fe := &fakeExpander{m: map[string][]string{"P": {"p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "p10"}}}
	s := newWithController(fc, fe, func(audio.Event) {})
	s.SetStations([]config.StationConfig{{
		Name:    "S",
		Shuffle: true,
		Items: []config.StationItem{
			{Kind: config.ItemPlaylist, ID: "P"},
		},
	}})

	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}

	// Shuffle picks a random *start index*; the queue itself keeps playlist
	// order (we no longer mutate the slice).
	firstPlayed := nextPlayed(t, fc)
	waitQueueLen(t, s, 10)

	s.mu.Lock()
	startID := s.queue[s.cur]
	inQueue := false
	for _, id := range s.queue {
		if id == firstPlayed {
			inQueue = true
			break
		}
	}
	s.mu.Unlock()

	if !inQueue {
		t.Fatalf("first played %q is not in the queue", firstPlayed)
	}
	if firstPlayed != startID {
		t.Fatalf("first played = %q, but queue[cur] = %q", firstPlayed, startID)
	}
}

func TestAdvanceShuffleNoImmediateRepeat(t *testing.T) {
	s := &StationPlayer{queue: []string{"a", "b", "c", "d", "e"}, shuffle: true}
	for i := 0; i < 200; i++ {
		prev := s.cur
		id, ok := s.advanceLocked()
		if !ok {
			t.Fatal("advanceLocked returned ok=false on a non-empty queue")
		}
		if s.cur == prev {
			t.Fatalf("shuffle advance repeated the current index %d", prev)
		}
		if id != s.queue[s.cur] {
			t.Fatalf("returned id %q != queue[cur] %q", id, s.queue[s.cur])
		}
	}
}

func TestSetShuffleIsModeOnlyToggle(t *testing.T) {
	fc := newFakeController()
	fe := &fakeExpander{m: map[string][]string{"P": {"p1", "p2", "p3", "p4", "p5"}}}
	s := newWithController(fc, fe, func(audio.Event) {})
	s.SetStations([]config.StationConfig{{
		Name:    "S",
		Shuffle: false,
		Items: []config.StationItem{
			{Kind: config.ItemPlaylist, ID: "P"},
		},
	}})

	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}
	first := nextPlayed(t, fc) // sequential start → p1
	waitQueueLen(t, s, 5)

	// Toggling shuffle is a pure mode change: it must not start, stop, or jump
	// playback (so toggling it while paused stays paused).
	if err := s.SetShuffle(0, true); err != nil {
		t.Fatal(err)
	}
	select {
	case id := <-fc.played:
		t.Fatalf("shuffle toggle triggered playback (played %q); it should be mode-only", id)
	case <-time.After(200 * time.Millisecond):
	}

	s.mu.Lock()
	on := s.shuffle
	s.mu.Unlock()
	if !on {
		t.Fatal("expected shuffle flag to be on")
	}

	// The mode still takes effect on the next advance: shuffle picks a track
	// other than the current one.
	if err := s.Next(); err != nil {
		t.Fatal(err)
	}
	if jumped := nextPlayed(t, fc); jumped == first {
		t.Fatalf("shuffled advance replayed the same track %q", jumped)
	}
}

func TestPrevStepsBackwardWithWrap(t *testing.T) {
	fc := newFakeController()
	fe := &fakeExpander{m: map[string][]string{"P": {"p1", "p2", "p3"}}}
	s := newWithController(fc, fe, func(audio.Event) {})
	s.SetStations([]config.StationConfig{{
		Name:  "S",
		Items: []config.StationItem{{Kind: config.ItemPlaylist, ID: "P"}},
	}})

	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}
	if got := nextPlayed(t, fc); got != "p1" {
		t.Fatalf("sequential start = %q, want p1", got)
	}
	waitQueueLen(t, s, 3)

	// Prev from the first track wraps to the last.
	if err := s.Prev(); err != nil {
		t.Fatal(err)
	}
	if got := nextPlayed(t, fc); got != "p3" {
		t.Fatalf("prev from first = %q, want p3 (wrap to end)", got)
	}
	// Prev again steps back one more.
	if err := s.Prev(); err != nil {
		t.Fatal(err)
	}
	if got := nextPlayed(t, fc); got != "p2" {
		t.Fatalf("prev = %q, want p2", got)
	}
}

func TestPlayResumesViaController(t *testing.T) {
	fc := newFakeController()
	fe := &fakeExpander{m: map[string][]string{"P": {"p1", "p2", "p3"}}}
	s := newWithController(fc, fe, func(audio.Event) {})
	s.SetStations([]config.StationConfig{{
		Name:    "S",
		Shuffle: false,
		Items: []config.StationItem{
			{Kind: config.ItemPlaylist, ID: "P"},
		},
	}})

	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}
	_ = nextPlayed(t, fc) // p1
	waitQueueLen(t, s, 3)

	if err := s.Pause(); err != nil {
		t.Fatal(err)
	}
	// Re-play the active station: this must RESUME (continue the loaded track),
	// not start a new PlayVideo from the beginning.
	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}
	select {
	case id := <-fc.played:
		t.Fatalf("resume should not call PlayVideo, but played %q", id)
	case <-time.After(200 * time.Millisecond):
	}
	if got := fc.resumes(); got != 1 {
		t.Fatalf("expected exactly 1 Resume call, got %d", got)
	}
}

func TestLegacyUpgradeOnTheFly(t *testing.T) {
	fc := newFakeController()
	fe := &fakeExpander{m: map[string][]string{"PLAbcdEfGhIjKlMnOpQrSt": {"p1", "p2"}}}
	s := newWithController(fc, fe, func(audio.Event) {})
	
	// Item is saved as ItemVideo with ID of a video, but Raw contains list= playlist parameter.
	s.SetStations([]config.StationConfig{{
		Name:    "Legacy Station",
		Shuffle: false,
		Items: []config.StationItem{
			{
				Kind: config.ItemVideo,
				ID:   "EWrX250Zhko",
				Raw:  "https://www.youtube.com/watch?v=EWrX250Zhko&list=PLAbcdEfGhIjKlMnOpQrSt",
			},
		},
	}})

	if err := s.Play(0); err != nil {
		t.Fatal(err)
	}

	// Playlist should expand on the fly.
	waitQueueLen(t, s, 2)
	s.mu.Lock()
	got := append([]string(nil), s.queue...)
	s.mu.Unlock()

	want := []string{"p1", "p2"}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("expected expanded playlist queue %v, got %v", want, got)
		}
	}
}
