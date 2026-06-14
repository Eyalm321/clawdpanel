// Package reveal owns the bar's auto-hide state machine: the slide animation, the
// cursor hover hit-test, the grace-period collapse timer, the
// fullscreen/pinned/editor precedence rules, and the click-through state. It
// talks to the OS only through the WindowOps seam (cursor + window ops), so the
// whole machine can be exercised with a fake cursor + fake clock instead of a
// real window. App owns only the OS poll loop, which calls Tick.
package reveal

import (
	"context"
	"log"
	"os"
	"sync"
	"sync/atomic"
	"time"

	"clawdpanel/internal/platform"
)

// revealDebug, when CLAWDPANEL_REVEAL_DEBUG is set, makes cursorOverBar log the
// live cursor position, the configured monitor's computed reveal zone, and the
// hit-test result every poll — for diagnosing why auto-hide reveal doesn't fire
// (cursor/monitor geometry mismatches on multi-monitor Linux).
var revealDebug = os.Getenv("CLAWDPANEL_REVEAL_DEBUG") != ""

// WindowOps is the narrow set of OS window operations the reveal machine needs.
// The production adapter binds a single window handle and forwards to
// internal/platform; tests inject a fake to assert slide positions and exercise
// generation/cancellation without a real OS window. Methods that don't take a
// handle in the platform layer (cursor/predicates) are on the seam too so the
// fake controls every input regardless of the host OS the test runs on.
type WindowOps interface {
	WindowRect() (left, top, width, height int)
	MoveTo(x, y int)
	ClipTop(width, height, topClip int)
	Show()
	Hide()
	// SetBounds moves+resizes the still-mapped window in one shot. Used by the
	// event-driven peek-collapse (resize to a sliver instead of unmapping) so the
	// window keeps a live surface that can receive the reveal-triggering enter.
	SetBounds(x, y, width, height int)
	// SetOpacity sets window opacity (0..1). _NET_WM_WINDOW_OPACITY is a pure
	// compositor blend and does NOT affect X11 input hit-testing, so the peek
	// strip can be made fully invisible (0) while still catching the reveal enter.
	SetOpacity(o float64)
	SetClickThrough(enabled bool)
	CursorPos() (x, y int)
	FullScreenActive(mon platform.MonitorInfo) bool
	AutoHideSupported() bool
}

const (
	defaultSlideDuration = 200 * time.Millisecond
	defaultFrame         = 16 * time.Millisecond // ~60 fps
	// defaultCollapseDelay is the grace period after the cursor leaves the bar
	// before it collapses — lets the user briefly overshoot and come back.
	defaultCollapseDelay = 200 * time.Millisecond
	// defaultPoll is how often Run samples the cursor. WebView2's mouseleave is
	// unreliable on small windows, so we poll the OS cursor rather than trust
	// JS mouse events for the hide trigger.
	defaultPoll = 80 * time.Millisecond
	// peekStripHeight is the sliver the bar collapses to in event/peek mode. It
	// stays mapped so its own surface still receives the pointer-enter that
	// triggers reveal (the global cursor poll is dead on Wayland).
	peekStripHeight = 2
)

// platformOps is the production WindowOps: it binds the window handle and
// forwards each call to the package-level internal/platform window functions.
type platformOps struct{ hwnd uintptr }

func (p platformOps) WindowRect() (int, int, int, int) { return platform.GetWindowSize(p.hwnd) }
func (p platformOps) MoveTo(x, y int)                  { platform.MoveWindow(p.hwnd, x, y) }
func (p platformOps) ClipTop(w, h, t int)              { platform.SetWindowClipTop(p.hwnd, w, h, t) }
func (p platformOps) Show()                            { platform.ShowWindow(p.hwnd) }
func (p platformOps) Hide()                            { platform.HideWindow(p.hwnd) }
func (p platformOps) SetBounds(x, y, w, h int)         { platform.SetWindowBounds(p.hwnd, x, y, w, h) }
func (p platformOps) SetOpacity(o float64)             { platform.SetOpacity(p.hwnd, o) }
func (p platformOps) SetClickThrough(e bool)           { platform.SetClickThrough(p.hwnd, e) }
func (p platformOps) CursorPos() (int, int)            { return platform.GetCursorPos() }
func (p platformOps) FullScreenActive(m platform.MonitorInfo) bool {
	return platform.IsFullScreenActive(m)
}
func (p platformOps) AutoHideSupported() bool { return platform.AutoHideSupported() }

func realTicker(d time.Duration) (<-chan time.Time, func()) {
	t := time.NewTicker(d)
	return t.C, t.Stop
}

// Controller owns the slide animation and click-through state behind WindowOps.
// It holds a geometry/mode snapshot pushed in via Configure (refreshed on dock /
// pin / click-through changes) rather than reaching back into App config.
type Controller struct {
	ops           WindowOps
	now           func() time.Time
	newTicker     func(time.Duration) (<-chan time.Time, func())
	slide         time.Duration
	frame         time.Duration
	collapseDelay time.Duration
	poll          time.Duration

	// onDone, when non-nil, is invoked as each animateY goroutine returns (for
	// any reason, including supersede). Test-only hook; nil in production.
	onDone func(gen uint64)

	mu               sync.Mutex
	configured       bool
	mon              platform.MonitorInfo
	barHeight        int
	pinned           bool
	userClickThrough bool
	expanded         bool
	editorOpen       bool      // editor open forces expanded + suppresses hover collapse
	leftBarAt        time.Time // first tick the cursor was off the bar — zero while it's on

	// animGen is bumped on every SetExpanded; a running animateY exits once it
	// sees the bump, so a new slide cleanly supersedes an in-flight one.
	animGen atomic.Uint64

	// eventMode swaps the cursor source from the frozen global poll to GTK
	// motion-controller hover events (Wayland), and makes collapse resize to a
	// peek strip instead of unmapping (so the surface survives to catch the
	// reveal enter). hoverFlag is the latched hover state fed by SetHover.
	eventMode atomic.Bool
	hoverFlag atomic.Bool

	// expandOpacity is the opacity restored when the peek strip expands back to the
	// full bar; collapsing sets opacity 0 so the strip vanishes while still
	// catching the reveal enter. Defaults to 1 (set from config via SetExpandOpacity).
	expandOpacity float64
}

// New builds a production Controller bound to the given native window handle.
func New(hwnd uintptr) *Controller {
	return newWithOps(platformOps{hwnd: hwnd})
}

// newWithOps is the test seam: it injects the WindowOps (a fake) and wires the
// real clock/ticker + default durations, which in-package tests may override.
func newWithOps(ops WindowOps) *Controller {
	return &Controller{
		ops:           ops,
		now:           time.Now,
		newTicker:     realTicker,
		slide:         defaultSlideDuration,
		frame:         defaultFrame,
		collapseDelay: defaultCollapseDelay,
		poll:          defaultPoll,
		expandOpacity: 1.0,
	}
}

// snapshot is a consistent read of the controller's geometry/mode state, taken
// under the lock so the animation/click-through math sees a coherent picture.
type snapshot struct {
	mon              platform.MonitorInfo
	barHeight        int
	pinned           bool
	userClickThrough bool
	expanded         bool
}

func (c *Controller) snap() snapshot {
	c.mu.Lock()
	defer c.mu.Unlock()
	return snapshot{c.mon, c.barHeight, c.pinned, c.userClickThrough, c.expanded}
}

func widthOf(mon platform.MonitorInfo) int {
	if mon.PhysWidth != 0 {
		return mon.PhysWidth
	}
	return mon.Width
}

// onScreenY is the bar's resting top: below any chrome above it (e.g. the
// macOS menu bar via WorkTopOffset) when top-docked, hugging the monitor's
// bottom when bottom-docked (Linux picks the edge per monitor — see
// MonitorInfo.DockEdge). offScreenY fully clears the screen past the docked
// edge so the window is gone when collapsed.
func bottomDocked(s snapshot) bool { return s.mon.DockEdge == "bottom" }

func onScreenY(s snapshot) int {
	if bottomDocked(s) {
		return int(s.mon.Top) + s.mon.Height - s.barHeight
	}
	return int(s.mon.Top) + s.mon.WorkTopOffset
}

func offScreenY(s snapshot) int {
	if bottomDocked(s) {
		return int(s.mon.Top) + s.mon.Height
	}
	return int(s.mon.Top) - s.barHeight
}

// Configure refreshes the geometry/mode snapshot and re-applies click-through.
// Call it wherever the bar is (re)docked and on pin / click-through changes.
func (c *Controller) Configure(mon platform.MonitorInfo, barHeight int, pinned, clickThrough bool) {
	c.mu.Lock()
	c.mon = mon
	c.barHeight = barHeight
	c.pinned = pinned
	c.userClickThrough = clickThrough
	c.configured = true
	c.mu.Unlock()
	c.ApplyClickThrough()
}

// SetEventMode switches the controller to event-driven hover (the GTK motion
// controller feeds SetHover) with peek-strip collapse — used on Wayland sessions
// where the global cursor poll is frozen. Off keeps the classic global poll +
// full hide (real X11 / Windows / macOS).
func (c *Controller) SetEventMode(on bool) { c.eventMode.Store(on) }

// SetHover latches the pointer-over-bar state from the GTK motion controller.
// The poll loop (Tick) consumes it via cursorOverBar, so the grace timer,
// pinned/fullscreen precedence and the rest of the policy keep working unchanged.
func (c *Controller) SetHover(over bool) { c.hoverFlag.Store(over) }

// SetExpandOpacity sets the opacity the bar is restored to when the peek strip
// expands (the user's configured window opacity). The collapsed strip is always 0.
func (c *Controller) SetExpandOpacity(o float64) {
	c.mu.Lock()
	c.expandOpacity = o
	c.mu.Unlock()
}

// SetUserClickThrough updates the user click-through preference and re-applies it
// (used by the tray toggle, which changes nothing about geometry).
func (c *Controller) SetUserClickThrough(enabled bool) {
	c.mu.Lock()
	c.userClickThrough = enabled
	c.mu.Unlock()
	c.ApplyClickThrough()
}

// Init sets the initial visual state without animating: pinned ⇒ expanded, else
// follow the cursor. When starting collapsed it snaps the window above the screen
// edge and hides it so nothing flashes on launch. Call after Configure.
func (c *Controller) Init() {
	c.mu.Lock()
	s := snapshot{c.mon, c.barHeight, c.pinned, c.userClickThrough, false}
	c.mu.Unlock()

	expanded := s.pinned || !c.ops.AutoHideSupported() || c.cursorOverBar(s)
	s.expanded = expanded
	c.mu.Lock()
	c.expanded = expanded
	c.mu.Unlock()

	c.ApplyClickThrough()
	if !expanded {
		if c.eventMode.Load() {
			c.peekResize(s, false)
		} else {
			c.ops.MoveTo(int(s.mon.Left), offScreenY(s))
			// Full clip so even if a monitor sits above, the window can't spill
			// onto it before Hide takes effect.
			c.ops.ClipTop(widthOf(s.mon), s.barHeight, s.barHeight)
			c.ops.Hide()
		}
	}
}

// Expanded reports whether the bar is currently on-screen.
func (c *Controller) Expanded() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.expanded
}

// Reveal slides the bar on-screen (used by the single-instance re-launch path).
func (c *Controller) Reveal() { c.SetExpanded(true) }

// SetExpanded transitions the bar on/off screen by sliding the OS window itself
// (so the dark window background travels with the bar, leaving no leftover
// frame). It's a no-op if already in the target state or not yet configured.
// Every call supersedes any in-flight slide.
func (c *Controller) SetExpanded(expanded bool) {
	c.mu.Lock()
	if !c.configured || c.expanded == expanded {
		c.mu.Unlock()
		return
	}
	c.expanded = expanded
	s := snapshot{c.mon, c.barHeight, c.pinned, c.userClickThrough, expanded}
	c.mu.Unlock()

	c.ApplyClickThrough()

	// Event/peek mode: no slide — resize between full bar and peek strip in place.
	if c.eventMode.Load() {
		c.peekResize(s, expanded)
		return
	}

	target := onScreenY(s)
	if !expanded {
		target = offScreenY(s)
	}
	gen := c.animGen.Add(1)
	if expanded {
		c.ops.Show() // reveal the off-screen window so MoveTo can slide it in
	}
	go c.animateY(s, target, gen, !expanded)
}

// peekResize is the event-mode collapse/expand: resize the still-mapped window
// between the full bar height and a peek sliver, in place (no slide, no unmap),
// so the window keeps a live surface that receives the reveal-triggering enter.
func (c *Controller) peekResize(s snapshot, expanded bool) {
	x := int(s.mon.Left)
	width := widthOf(s.mon)
	y := onScreenY(s)
	c.animGen.Add(1) // supersede any in-flight slide
	if expanded {
		c.mu.Lock()
		o := c.expandOpacity
		c.mu.Unlock()
		c.ops.Show()
		c.ops.SetBounds(x, y, width, s.barHeight)
		c.ops.SetOpacity(o)
	} else {
		// Collapse to a thin sliver. It MUST stay visible enough to locate (a
		// fully-invisible strip is unfindable — there's no cue where to hover to
		// reveal), so keep it at the configured opacity, just very short.
		c.ops.SetBounds(x, y, width, peekStripHeight)
	}
}

// ApplyClickThrough sets the window's click-through from the user preference OR,
// where auto-hide is wired up, the "invisible collapsed" state — so a hidden bar
// can't eat clicks. On platforms without auto-hide this reduces to the user
// preference alone.
func (c *Controller) ApplyClickThrough() {
	s := c.snap()
	autoHide := c.ops.AutoHideSupported() && !s.pinned && !s.expanded
	c.ops.SetClickThrough(s.userClickThrough || autoHide)
}

// cursorOverBar reports whether the OS cursor is inside the bar's hit box: the
// monitor's full width, from its true top edge down to the bar's bottom. The
// menu-bar slice above the bar (macOS WorkTopOffset) counts as "on the bar" so
// the user can reveal it from the screen edge; on Windows/Linux WorkTopOffset is
// 0 and this is just [Top, Top+BarHeight].
func (c *Controller) cursorOverBar(s snapshot) bool {
	// Event mode: the GTK motion controller already told us whether the pointer is
	// over the bar (or its peek strip) — no global cursor query (it's frozen on
	// Wayland), no geometry hit-test.
	if c.eventMode.Load() {
		over := c.hoverFlag.Load()
		if revealDebug {
			log.Printf("[reveal] eventMode hover=%v", over)
		}
		return over
	}
	cx, cy := c.ops.CursorPos()
	over := c.hitTestBar(s, cx, cy)
	if revealDebug {
		width := widthOf(s.mon)
		yLo, yHi := int(s.mon.Top), int(s.mon.Top)+s.mon.WorkTopOffset+s.barHeight
		if bottomDocked(s) {
			yHi = int(s.mon.Top) + s.mon.Height
			yLo = yHi - s.barHeight
		}
		log.Printf("[reveal] cursor=(%d,%d) mon=%q L=%d T=%d W=%d off=%d bottom=%v zone x[%d,%d) y[%d,%d) -> over=%v",
			cx, cy, s.mon.Name, int(s.mon.Left), int(s.mon.Top), width, s.mon.WorkTopOffset, bottomDocked(s),
			int(s.mon.Left), int(s.mon.Left)+width, yLo, yHi, over)
	}
	return over
}

func (c *Controller) hitTestBar(s snapshot, cx, cy int) bool {
	if cx < 0 && cy < 0 {
		return false // platform stub (no cursor source)
	}
	width := widthOf(s.mon)
	if cx < int(s.mon.Left) || cx >= int(s.mon.Left)+width {
		return false
	}
	if bottomDocked(s) {
		monBottom := int(s.mon.Top) + s.mon.Height
		return cy >= monBottom-s.barHeight && cy < monBottom
	}
	return cy >= int(s.mon.Top) && cy < int(s.mon.Top)+s.mon.WorkTopOffset+s.barHeight
}

// SetEditorOpen forces the bar expanded while the inline accounts editor is shown
// (it's launched with the cursor off-bar and must stay open until dismissed). On
// close, the machine re-evaluates against the current cursor position.
func (c *Controller) SetEditorOpen(open bool) {
	c.mu.Lock()
	c.editorOpen = open
	pinned := c.pinned
	c.mu.Unlock()

	if open && !pinned {
		c.SetExpanded(true)
	}
	if !open {
		c.Tick()
	}
}

// SetPinned applies a pin-state change: pinned ⇒ always expanded; unpinned ⇒
// follow the cursor (the user just clicked the pin icon, so the cursor is on the
// bar — avoids a flicker before the next poll). Resets the grace timer.
func (c *Controller) SetPinned(pinned bool) {
	c.mu.Lock()
	c.pinned = pinned
	c.leftBarAt = time.Time{}
	s := snapshot{c.mon, c.barHeight, pinned, c.userClickThrough, c.expanded}
	c.mu.Unlock()

	c.ApplyClickThrough()
	c.SetExpanded(pinned || c.cursorOverBar(s))
}

// Tick advances the auto-hide state machine one step from the current cursor
// position; the OS cursor poller (Run) calls it. Precedence: editor-open and
// pinned force expanded, fullscreen forces collapsed, otherwise the bar follows
// the cursor and collapses after the grace delay once the cursor has left.
func (c *Controller) Tick() {
	c.mu.Lock()
	if !c.configured || c.editorOpen {
		c.mu.Unlock()
		return
	}
	s := snapshot{c.mon, c.barHeight, c.pinned, c.userClickThrough, c.expanded}
	c.mu.Unlock()

	// Fullscreen takes precedence over pin/hover: while a frontmost app is in
	// native fullscreen, force-collapse the bar (the tray icon stays). On
	// platforms with no fullscreen detection this is a no-op.
	if c.ops.FullScreenActive(s.mon) {
		if c.Expanded() {
			c.SetExpanded(false)
		}
		return
	}
	// Pinned, or auto-hide isn't trustworthy on this platform/session (e.g.
	// XWayland on a Wayland session, where the global cursor poll freezes) → keep
	// the bar shown. Without the AutoHideSupported guard the loop would chase a
	// stale cursor and leave the bar stuck shown or hidden.
	if s.pinned || !c.ops.AutoHideSupported() {
		if !c.Expanded() {
			c.SetExpanded(true)
		}
		return
	}
	if c.cursorOverBar(s) {
		c.mu.Lock()
		c.leftBarAt = time.Time{}
		c.mu.Unlock()
		c.SetExpanded(true)
		return
	}
	// Cursor off the bar — start the grace timer on the first off-tick; only
	// collapse once it's been gone for collapseDelay.
	c.mu.Lock()
	if c.leftBarAt.IsZero() {
		c.leftBarAt = c.now()
		c.mu.Unlock()
		return
	}
	graceElapsed := c.now().Sub(c.leftBarAt) >= c.collapseDelay
	c.mu.Unlock()
	if graceElapsed && c.Expanded() {
		c.SetExpanded(false)
	}
}

// Run polls the cursor every c.poll and drives the machine via Tick until ctx is
// cancelled. App starts this once the native window handle is known.
func (c *Controller) Run(ctx context.Context) {
	ticker := time.NewTicker(c.poll)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			c.Tick()
		}
	}
}

// animateY slides the window's top edge to targetY over c.slide with an ease-out
// cubic, repositioning the top clip each frame so the portion above mon.Top stays
// masked (multi-monitor spill). If hideAfter, the window is hidden once it
// reaches the off-screen target. A newer SetExpanded bumps animGen; this loop
// sees the bump and exits without touching the window further.
func (c *Controller) animateY(s snapshot, targetY int, gen uint64, hideAfter bool) {
	if c.onDone != nil {
		defer c.onDone(gen)
	}
	x := int(s.mon.Left)
	monTop := int(s.mon.Top)
	width := widthOf(s.mon)
	barH := s.barHeight

	_, startY, _, _ := c.ops.WindowRect()
	if startY == targetY {
		if hideAfter {
			c.ops.Hide()
		}
		return
	}
	start := c.now()
	tickC, stop := c.newTicker(c.frame)
	defer stop()

	// Once any pixel has crossed above mon.Top, clip one extra pixel to absorb
	// DPI/rounding slop that would otherwise leave a row on the monitor above.
	clipFor := func(y int) int {
		if bottomDocked(s) {
			return 0 // slides off the bottom; nothing above to spill onto
		}
		top := monTop - y
		if top > 0 {
			top++
		}
		return top
	}

	for range tickC {
		if c.animGen.Load() != gen {
			return // superseded by a newer slide
		}
		elapsed := c.now().Sub(start)
		if elapsed >= c.slide {
			c.ops.MoveTo(x, targetY)
			c.ops.ClipTop(width, barH, clipFor(targetY))
			if hideAfter {
				c.ops.Hide()
			}
			return
		}
		t := float64(elapsed) / float64(c.slide)
		t = 1 - (1-t)*(1-t)*(1-t) // ease-out cubic
		y := startY + int(float64(targetY-startY)*t)
		c.ops.MoveTo(x, y)
		c.ops.ClipTop(width, barH, clipFor(y))
	}
}
