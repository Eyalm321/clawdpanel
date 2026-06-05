package reveal

import (
	"sync"
	"testing"
	"time"

	"clawdpanel/internal/platform"
)

// ── test doubles ────────────────────────────────────────────────────────────

type point struct{ x, y int }

// fakeOps is a WindowOps that records every call and lets tests read back the
// window position. WindowRect returns the last MoveTo, so a follow-up slide
// continues from where the previous one left off.
type fakeOps struct {
	mu               sync.Mutex
	x, y, w, h       int
	moves            []point
	shows, hides     int
	clickSets        []bool
	autoHide         bool
	fullScreen       bool
	cursorX, cursorY int
}

func (f *fakeOps) WindowRect() (int, int, int, int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.x, f.y, f.w, f.h
}
func (f *fakeOps) MoveTo(x, y int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.x, f.y = x, y
	f.moves = append(f.moves, point{x, y})
}
func (f *fakeOps) ClipTop(w, h, t int) {}
func (f *fakeOps) Show()               { f.mu.Lock(); f.shows++; f.mu.Unlock() }
func (f *fakeOps) Hide()               { f.mu.Lock(); f.hides++; f.mu.Unlock() }
func (f *fakeOps) SetClickThrough(e bool) {
	f.mu.Lock()
	f.clickSets = append(f.clickSets, e)
	f.mu.Unlock()
}
func (f *fakeOps) CursorPos() (int, int) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.cursorX, f.cursorY
}
func (f *fakeOps) FullScreenActive(platform.MonitorInfo) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.fullScreen
}
func (f *fakeOps) AutoHideSupported() bool { f.mu.Lock(); defer f.mu.Unlock(); return f.autoHide }

func (f *fakeOps) setPos(x, y int)    { f.mu.Lock(); f.x, f.y = x, y; f.mu.Unlock() }
func (f *fakeOps) setCursor(x, y int) { f.mu.Lock(); f.cursorX, f.cursorY = x, y; f.mu.Unlock() }
func (f *fakeOps) moveCount() int     { f.mu.Lock(); defer f.mu.Unlock(); return len(f.moves) }
func (f *fakeOps) hideCount() int     { f.mu.Lock(); defer f.mu.Unlock(); return f.hides }
func (f *fakeOps) lastMove() point    { f.mu.Lock(); defer f.mu.Unlock(); return f.moves[len(f.moves)-1] }
func (f *fakeOps) lastClickThrough() (bool, bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.clickSets) == 0 {
		return false, false
	}
	return f.clickSets[len(f.clickSets)-1], true
}

// manualClock drives the animation deterministically: time only moves when the
// test calls advance, and frames only fire when the test calls tick. Each
// animateY goroutine gets its own ticker channel (appended in creation order).
type manualClock struct {
	mu    sync.Mutex
	t     time.Time
	chans []chan time.Time
}

func newManualClock() *manualClock { return &manualClock{t: time.Unix(1000, 0)} }
func (m *manualClock) now() time.Time {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.t
}
func (m *manualClock) newTicker(time.Duration) (<-chan time.Time, func()) {
	ch := make(chan time.Time, 1)
	m.mu.Lock()
	m.chans = append(m.chans, ch)
	m.mu.Unlock()
	return ch, func() {}
}
func (m *manualClock) advance(d time.Duration) { m.mu.Lock(); m.t = m.t.Add(d); m.mu.Unlock() }
func (m *manualClock) tickerCount() int        { m.mu.Lock(); defer m.mu.Unlock(); return len(m.chans) }
func (m *manualClock) tick(i int) {
	m.mu.Lock()
	ch, now := m.chans[i], m.t
	m.mu.Unlock()
	ch <- now
}

// ── helpers ─────────────────────────────────────────────────────────────────

const barHeight = 40

// onScreen Y = Top + WorkTopOffset = 50; offScreen Y = Top - barHeight = 10.
func testMon() platform.MonitorInfo {
	return platform.MonitorInfo{Left: 100, Top: 50, Width: 1920, PhysWidth: 1920, WorkTopOffset: 0}
}

func newTestController(fake *fakeOps, clk *manualClock, done func(uint64)) *Controller {
	c := newWithOps(fake)
	c.now = clk.now
	c.newTicker = clk.newTicker
	c.slide = 100 * time.Millisecond
	c.onDone = done
	return c
}

func waitFor(t *testing.T, cond func() bool, what string) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatalf("timed out waiting for %s", what)
}

func recvDone(t *testing.T, ch <-chan uint64) uint64 {
	t.Helper()
	select {
	case g := <-ch:
		return g
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for animation to finish")
		return 0
	}
}

// ── tests ───────────────────────────────────────────────────────────────────

// A slide drives the window all the way to its on-screen target when expanding
// and its off-screen target (then hides) when collapsing.
func TestSlideReachesTarget(t *testing.T) {
	fake := &fakeOps{}
	fake.setPos(100, 10) // start collapsed (off-screen)
	clk := newManualClock()
	done := make(chan uint64, 4)
	c := newTestController(fake, clk, func(g uint64) { done <- g })
	c.Configure(testMon(), barHeight, false, false)

	// Expand → reaches on-screen Y (50).
	c.SetExpanded(true)
	waitFor(t, func() bool { return clk.tickerCount() >= 1 }, "expand ticker")
	clk.advance(c.slide) // elapsed >= slide → final frame
	clk.tick(0)
	recvDone(t, done)
	if got := fake.lastMove(); got != (point{100, 50}) {
		t.Fatalf("after expand lastMove = %v, want {100 50}", got)
	}

	// Collapse → reaches off-screen Y (10) and hides.
	c.SetExpanded(false)
	waitFor(t, func() bool { return clk.tickerCount() >= 2 }, "collapse ticker")
	clk.advance(c.slide)
	clk.tick(1)
	recvDone(t, done)
	if got := fake.lastMove(); got != (point{100, 10}) {
		t.Fatalf("after collapse lastMove = %v, want {100 10}", got)
	}
	if fake.hideCount() == 0 {
		t.Error("collapse did not hide the window after reaching the off-screen target")
	}
}

// A new reveal mid-collapse supersedes the in-flight slide: the old animation
// stops touching the window the moment the generation is bumped, and the window
// ends up at the NEW target, not the abandoned one.
func TestNewRevealSupersedesInFlight(t *testing.T) {
	fake := &fakeOps{}
	fake.setPos(100, 50) // start expanded (on-screen)
	clk := newManualClock()
	done := make(chan uint64, 4)
	c := newTestController(fake, clk, func(g uint64) { done <- g })
	c.Configure(testMon(), barHeight, false, false)
	c.mu.Lock()
	c.expanded = true // start expanded (skip the initial slide-in)
	c.mu.Unlock()

	// Begin collapsing (generation 1).
	c.SetExpanded(false)
	waitFor(t, func() bool { return clk.tickerCount() >= 1 }, "collapse ticker")

	// One partial frame: the window moves part-way toward off-screen.
	clk.advance(c.slide / 4)
	clk.tick(0)
	waitFor(t, func() bool { return fake.moveCount() >= 1 }, "partial collapse frame")
	partial := fake.lastMove()
	if !(partial.y < 50 && partial.y > 10) {
		t.Fatalf("partial frame y = %d, want between 10 and 50", partial.y)
	}
	movesBeforeSupersede := fake.moveCount()

	// Supersede with a reveal (generation 2). The collapse goroutine is still
	// blocked on its ticker.
	c.SetExpanded(true)
	waitFor(t, func() bool { return clk.tickerCount() >= 2 }, "reveal ticker")

	// Fire the collapse goroutine's next frame: it sees the bumped generation
	// and bails without moving the window.
	clk.tick(0)
	if g := recvDone(t, done); g != 1 {
		t.Fatalf("expected superseded collapse (gen 1) to finish first, got gen %d", g)
	}
	if fake.moveCount() != movesBeforeSupersede {
		t.Errorf("superseded slide kept moving the window: %d moves, want %d",
			fake.moveCount(), movesBeforeSupersede)
	}

	// Drive the reveal to completion: it reaches the on-screen target.
	clk.advance(c.slide)
	clk.tick(1)
	if g := recvDone(t, done); g != 2 {
		t.Fatalf("expected reveal (gen 2) to finish, got gen %d", g)
	}
	if got := fake.lastMove(); got != (point{100, 50}) {
		t.Fatalf("after supersede lastMove = %v, want on-screen {100 50}", got)
	}
}

// Click-through is engaged when the bar is auto-hidden (unpinned + collapsed on a
// platform that supports it), otherwise it follows the user preference.
func TestApplyClickThrough(t *testing.T) {
	cases := []struct {
		name                               string
		autoHide, pinned, expanded, userCT bool
		want                               bool
	}{
		{"collapsed autohide forces clickthrough", true, false, false, false, true},
		{"expanded does not force clickthrough", true, false, true, false, false},
		{"pinned ignores autohide", true, true, false, false, false},
		{"unsupported follows user pref on", false, false, false, true, true},
		{"unsupported follows user pref off", false, false, false, false, false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			fake := &fakeOps{autoHide: tc.autoHide}
			c := newWithOps(fake)
			// Set the state directly: pinned+collapsed can't arise via Init
			// (pinned forces expanded), but ApplyClickThrough must still handle it.
			c.mu.Lock()
			c.configured = true
			c.mon = testMon()
			c.barHeight = barHeight
			c.pinned = tc.pinned
			c.expanded = tc.expanded
			c.userClickThrough = tc.userCT
			c.mu.Unlock()
			c.ApplyClickThrough()
			got, ok := fake.lastClickThrough()
			if !ok {
				t.Fatal("SetClickThrough was never called")
			}
			if got != tc.want {
				t.Errorf("click-through = %v, want %v", got, tc.want)
			}
		})
	}
}
