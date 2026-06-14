package reveal

import (
	"testing"
	"time"
)

// These tests drive the auto-hide state machine through Tick with a fake cursor
// (fakeOps.CursorPos) and a fake clock (manualClock), so the grace-timer and
// precedence rules are deterministic. The shared fakeOps / manualClock / testMon
// / newTestController helpers live in reveal_test.go.
//
// On-bar hit box for testMon(): x ∈ [100, 2020), y ∈ [50, 90).

// The grace period delays the collapse: the bar stays up until the cursor has
// been gone for collapseDelay.
func TestTickGraceDelaysCollapse(t *testing.T) {
	fake := &fakeOps{autoHide: true}
	clk := newManualClock()
	c := newTestController(fake, clk, nil)
	c.collapseDelay = 100 * time.Millisecond
	c.Configure(testMon(), barHeight, false, false) // unpinned

	fake.setCursor(150, 60) // on-bar
	c.Init()
	if !c.Expanded() {
		t.Fatal("Init with cursor on the bar should start expanded")
	}

	// Cursor leaves: the first off-bar tick starts the grace timer only.
	fake.setCursor(150, 300) // off-bar
	c.Tick()
	if !c.Expanded() {
		t.Fatal("first off-bar tick must not collapse (grace timer just started)")
	}

	// Still inside the grace window: no collapse.
	clk.advance(c.collapseDelay - time.Millisecond)
	c.Tick()
	if !c.Expanded() {
		t.Fatal("collapsed before the grace delay elapsed")
	}

	// Grace elapsed: collapse.
	clk.advance(2 * time.Millisecond)
	c.Tick()
	if c.Expanded() {
		t.Fatal("bar should have collapsed after the grace delay")
	}
}

// In event mode (Wayland) the hover signal comes from SetHover (the GTK motion
// controller), not the cursor poll, and collapse resizes to a peek strip instead
// of hiding — so its surface survives to receive the reveal-triggering enter.
func TestEventModeHoverDrivesPeek(t *testing.T) {
	fake := &fakeOps{autoHide: true}
	clk := newManualClock()
	c := newTestController(fake, clk, nil)
	c.collapseDelay = 100 * time.Millisecond
	c.SetEventMode(true)
	c.Configure(testMon(), barHeight, false, false) // unpinned

	c.SetHover(true) // pointer on the bar — no cursor coords involved
	c.Init()
	if !c.Expanded() {
		t.Fatal("Init with hover should start expanded")
	}

	// Pointer leaves → grace timer → collapse to a peek strip (not hide).
	c.SetHover(false)
	c.Tick()
	clk.advance(c.collapseDelay + time.Millisecond)
	c.Tick()
	if c.Expanded() {
		t.Fatal("bar should collapse after the pointer leaves + grace")
	}
	if b, ok := fake.lastBounds(); !ok || b[3] != peekStripHeight {
		t.Errorf("collapse should resize to a %dpx peek strip, got %v (ok=%v)", peekStripHeight, b, ok)
	}
	if n := fake.hideCount(); n != 0 {
		t.Errorf("event mode must not unmap the window (Hide called %d times)", n)
	}

	// Pointer re-enters the peek strip → reveal (resize back to full bar height).
	c.SetHover(true)
	c.Tick()
	if !c.Expanded() {
		t.Fatal("hover on the peek strip should reveal the bar")
	}
	if b, _ := fake.lastBounds(); b[3] != barHeight {
		t.Errorf("reveal should resize back to bar height %d, got %v", barHeight, b)
	}
}

// A cursor that returns before the grace delay cancels the pending collapse and
// restarts the timer, so a later tick past the ORIGINAL deadline doesn't collapse.
func TestTickCursorReturnCancelsCollapse(t *testing.T) {
	fake := &fakeOps{autoHide: true}
	clk := newManualClock()
	c := newTestController(fake, clk, nil)
	c.collapseDelay = 100 * time.Millisecond
	c.Configure(testMon(), barHeight, false, false)

	fake.setCursor(150, 60) // on-bar
	c.Init()                // expanded

	// Cursor leaves: start the grace timer.
	fake.setCursor(150, 300) // off-bar
	c.Tick()

	// Part-way through the grace window the cursor returns.
	clk.advance(c.collapseDelay / 2)
	fake.setCursor(150, 60) // back on-bar
	c.Tick()
	if !c.Expanded() {
		t.Fatal("cursor back on the bar must keep it expanded")
	}

	// Cursor leaves again; the grace timer restarts from now, so a tick past what
	// would have been the original deadline must not collapse.
	fake.setCursor(150, 300) // off-bar
	c.Tick()                 // restarts the grace timer
	clk.advance(c.collapseDelay/2 + time.Millisecond)
	c.Tick()
	if !c.Expanded() {
		t.Fatal("collapse should have been cancelled and the grace timer restarted")
	}
}

// Precedence: fullscreen forces collapse; pinned and editor-open force expanded.
func TestTickPrecedence(t *testing.T) {
	t.Run("fullscreen forces collapse", func(t *testing.T) {
		fake := &fakeOps{autoHide: true}
		c := newTestController(fake, newManualClock(), nil)
		c.Configure(testMon(), barHeight, false, false)
		fake.setCursor(150, 60) // on-bar — would normally keep it expanded
		c.Init()                // expanded
		fake.mu.Lock()
		fake.fullScreen = true
		fake.mu.Unlock()

		c.Tick()
		if c.Expanded() {
			t.Fatal("fullscreen must force collapse even with the cursor on the bar")
		}
	})

	t.Run("fullscreen beats pinned", func(t *testing.T) {
		// The whole point of Windows fullscreen detection: a borderless
		// fullscreen app must collapse the bar even while it's pinned. Tick
		// checks fullscreen before pinned, so this must hold.
		fake := &fakeOps{autoHide: true}
		c := newTestController(fake, newManualClock(), nil)
		c.Configure(testMon(), barHeight, true, false) // pinned
		c.Init()                                        // pinned ⇒ expanded
		fake.mu.Lock()
		fake.fullScreen = true
		fake.mu.Unlock()

		c.Tick()
		if c.Expanded() {
			t.Fatal("fullscreen must force collapse even when pinned")
		}
	})

	t.Run("pinned forces expanded", func(t *testing.T) {
		fake := &fakeOps{autoHide: true}
		c := newTestController(fake, newManualClock(), nil)
		// Pinned but currently collapsed (e.g. just after a fullscreen
		// suppression): a tick must restore it.
		c.Configure(testMon(), barHeight, true, false)
		fake.setCursor(150, 300) // off-bar — irrelevant while pinned
		c.Tick()
		if !c.Expanded() {
			t.Fatal("pinned must force the bar expanded")
		}
	})

	t.Run("editor open forces expanded and suppresses collapse", func(t *testing.T) {
		fake := &fakeOps{autoHide: true}
		clk := newManualClock()
		c := newTestController(fake, clk, nil)
		c.Configure(testMon(), barHeight, false, false)
		fake.setCursor(150, 300) // off-bar
		c.Init()                 // collapsed
		if c.Expanded() {
			t.Fatal("precondition: should start collapsed off-bar")
		}

		c.SetEditorOpen(true)
		if !c.Expanded() {
			t.Fatal("opening the editor must expand the bar")
		}

		// While the editor is open an off-bar tick must NOT collapse.
		clk.advance(time.Second)
		c.Tick()
		if !c.Expanded() {
			t.Fatal("editor open must suppress hover collapse")
		}
	})
}
