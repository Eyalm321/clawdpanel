package audio

import (
	"context"
	"sync"
	"testing"
	"time"
)

type mockPlayer struct {
	mu            sync.Mutex
	playFunc      func(url string) error
	resumeFunc    func() error
	pauseFunc     func() error
	stopFunc      func() error
	setVolumeFunc func(v float64) error
	seekFunc      func(seconds float64) error
	closeFunc     func() error
}

func (m *mockPlayer) Play(url string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.playFunc != nil {
		return m.playFunc(url)
	}
	return nil
}

func (m *mockPlayer) Resume() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.resumeFunc != nil {
		return m.resumeFunc()
	}
	return nil
}

func (m *mockPlayer) Pause() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.pauseFunc != nil {
		return m.pauseFunc()
	}
	return nil
}

func (m *mockPlayer) Stop() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.stopFunc != nil {
		return m.stopFunc()
	}
	return nil
}

func (m *mockPlayer) SetVolume(v float64) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.setVolumeFunc != nil {
		return m.setVolumeFunc(v)
	}
	return nil
}

func (m *mockPlayer) Seek(seconds float64) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.seekFunc != nil {
		return m.seekFunc(seconds)
	}
	return nil
}

func (m *mockPlayer) Close() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closeFunc != nil {
		return m.closeFunc()
	}
	return nil
}

type mockResolver struct {
	mu         sync.Mutex
	streamFunc func(ctx context.Context, videoID string, forceRefresh bool) (string, error)
	calls      []string
	forceCalls []bool
}

func (r *mockResolver) Resolve(ctx context.Context, videoID string, forceRefresh bool) (ResolvedTrack, error) {
	r.mu.Lock()
	r.calls = append(r.calls, videoID)
	r.forceCalls = append(r.forceCalls, forceRefresh)
	r.mu.Unlock()
	if r.streamFunc != nil {
		url, err := r.streamFunc(ctx, videoID, forceRefresh)
		return ResolvedTrack{URL: url}, err
	}
	return ResolvedTrack{URL: "http://test.url/" + videoID}, nil
}

func TestController_PlayNormal(t *testing.T) {
	resolver := &mockResolver{}
	var events []Event
	var eventsMu sync.Mutex

	c := &Controller{
		resolver: resolver,
		emit: func(ev Event) {
			eventsMu.Lock()
			events = append(events, ev)
			eventsMu.Unlock()
		},
	}

	playCalled := false
	player := &mockPlayer{
		playFunc: func(url string) error {
			playCalled = true
			if url != "http://test.url/vid123" {
				t.Errorf("expected URL http://test.url/vid123, got %s", url)
			}
			return nil
		},
	}
	c.player = player

	err := c.PlayVideo(context.Background(), "vid123")
	if err != nil {
		t.Fatalf("unexpected play error: %v", err)
	}

	if !playCalled {
		t.Error("expected Player.Play to be called")
	}

	eventsMu.Lock()
	if len(events) != 1 || events[0].State != StateLoading || events[0].VideoID != "vid123" {
		t.Errorf("expected [loading] event for vid123, got %v", events)
	}
	eventsMu.Unlock()
}

func TestController_RetryOnce(t *testing.T) {
	resolver := &mockResolver{
		streamFunc: func(ctx context.Context, videoID string, forceRefresh bool) (string, error) {
			if forceRefresh {
				return "http://refreshed.url/" + videoID, nil
			}
			return "http://initial.url/" + videoID, nil
		},
	}

	var events []Event
	var eventsMu sync.Mutex
	doneChan := make(chan struct{})

	c := &Controller{
		resolver: resolver,
		emit: func(ev Event) {
			eventsMu.Lock()
			events = append(events, ev)
			if ev.State == StatePlaying {
				close(doneChan)
			}
			eventsMu.Unlock()
		},
	}

	playCount := 0
	player := &mockPlayer{
		playFunc: func(url string) error {
			playCount++
			if playCount == 1 {
				if url != "http://initial.url/vid123" {
					t.Errorf("expected http://initial.url/vid123, got %s", url)
				}
				// Simulate error callback asynchronously to mimic a player crash/HLS expiry
				go c.handlePlayerEvent(Event{State: StateError, Err: "HLS manifest expired"})
			} else if playCount == 2 {
				if url != "http://refreshed.url/vid123" {
					t.Errorf("expected http://refreshed.url/vid123, got %s", url)
				}
				// Success on second try
				go c.handlePlayerEvent(Event{State: StatePlaying})
			}
			return nil
		},
	}
	c.player = player

	err := c.PlayVideo(context.Background(), "vid123")
	if err != nil {
		t.Fatalf("unexpected play error: %v", err)
	}

	select {
	case <-doneChan:
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for playing state")
	}

	if playCount != 2 {
		t.Errorf("expected player to be played twice, got %d", playCount)
	}

	resolver.mu.Lock()
	if len(resolver.calls) != 2 {
		t.Errorf("expected resolver to be called twice, got %d", len(resolver.calls))
	}
	if resolver.forceCalls[0] != false || resolver.forceCalls[1] != true {
		t.Errorf("expected forceRefresh flags [false, true], got %v", resolver.forceCalls)
	}
	resolver.mu.Unlock()
}

func TestController_RetryFail(t *testing.T) {
	resolver := &mockResolver{}

	var events []Event
	var eventsMu sync.Mutex
	doneChan := make(chan struct{})

	c := &Controller{
		resolver: resolver,
		emit: func(ev Event) {
			eventsMu.Lock()
			events = append(events, ev)
			if ev.State == StateError && ev.Err == "HLS manifest expired again" {
				close(doneChan)
			}
			eventsMu.Unlock()
		},
	}

	playCount := 0
	player := &mockPlayer{
		playFunc: func(url string) error {
			playCount++
			if playCount == 1 {
				go c.handlePlayerEvent(Event{State: StateError, Err: "HLS manifest expired"})
			} else if playCount == 2 {
				// Fail on second try as well
				go c.handlePlayerEvent(Event{State: StateError, Err: "HLS manifest expired again"})
			}
			return nil
		},
	}
	c.player = player

	err := c.PlayVideo(context.Background(), "vid123")
	if err != nil {
		t.Fatalf("unexpected play error: %v", err)
	}

	select {
	case <-doneChan:
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for terminal error state")
	}

	if playCount != 2 {
		t.Errorf("expected play to attempt twice, got %d", playCount)
	}
}
