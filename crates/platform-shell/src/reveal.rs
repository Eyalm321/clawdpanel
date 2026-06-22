//! The bar's auto-hide state machine: the slide animation, the cursor hover
//! hit-test, the grace-period collapse timer, the fullscreen/pinned/editor
//! precedence rules, and the click-through state.
//!
//! 1:1 port of the Go `internal/reveal/reveal.go`. It talks to the OS only
//! through the [`WindowOps`] seam (cursor + window ops), so the whole machine
//! can be exercised with a fake cursor + fake clock instead of a real window.
//! App owns only the OS poll loop, which calls [`Controller::tick`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{width_px, MonitorInfo, WindowOps};

const DEFAULT_SLIDE_DURATION: Duration = Duration::from_millis(200);
const DEFAULT_FRAME: Duration = Duration::from_millis(16); // ~60 fps
/// Grace period after the cursor leaves the bar before it collapses — lets the
/// user briefly overshoot and come back.
const DEFAULT_COLLAPSE_DELAY: Duration = Duration::from_millis(200);
/// How often [`Controller::run`] samples the cursor. mouseleave is unreliable on
/// small windows, so we poll the OS cursor rather than trust JS mouse events.
const DEFAULT_POLL: Duration = Duration::from_millis(80);

type NowFn = Box<dyn Fn() -> Instant + Send + Sync>;
type StopFn = Box<dyn FnOnce() + Send>;
type NewTicker = Box<dyn Fn(Duration) -> (Receiver<Instant>, StopFn) + Send + Sync>;
type DoneFn = Box<dyn Fn(u64) + Send + Sync>;

/// The production ticker: a thread emits an `Instant` every `d` until stopped.
fn real_ticker(d: Duration) -> (Receiver<Instant>, StopFn) {
    let (tx, rx) = channel();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s2 = stop.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(d);
        if s2.load(Ordering::Relaxed) {
            return;
        }
        if tx.send(Instant::now()).is_err() {
            return;
        }
    });
    (rx, Box::new(move || stop.store(true, Ordering::Relaxed)))
}

#[derive(Clone, Copy)]
struct Durations {
    slide: Duration,
    frame: Duration,
    collapse_delay: Duration,
    poll: Duration,
}

impl Default for Durations {
    fn default() -> Self {
        Self {
            slide: DEFAULT_SLIDE_DURATION,
            frame: DEFAULT_FRAME,
            collapse_delay: DEFAULT_COLLAPSE_DELAY,
            poll: DEFAULT_POLL,
        }
    }
}

#[derive(Default)]
struct State {
    configured: bool,
    mon: MonitorInfo,
    bar_height: i32,
    pinned: bool,
    user_click_through: bool,
    expanded: bool,
    editor_open: bool, // editor open forces expanded + suppresses hover collapse
    /// First tick the cursor was off the bar — `None` while it's on (Go's zero
    /// `time.Time`).
    left_bar_at: Option<Instant>,
}

/// Shared, thread-safe inner state. `animateY` slide loops run on their own
/// threads and capture an `Arc<Shared>`.
struct Shared {
    ops: Box<dyn WindowOps>,
    now: NowFn,
    new_ticker: NewTicker,
    /// Invoked as each `animate_y` returns (for any reason, incl. supersede).
    /// Test-only hook; `None` in production.
    on_done: Option<DoneFn>,
    durs: Mutex<Durations>,
    st: Mutex<State>,
    /// Bumped on every `set_expanded`; a running `animate_y` exits once it sees
    /// the bump, so a new slide cleanly supersedes an in-flight one.
    anim_gen: AtomicU64,
    last_collapse_cursor: Mutex<Option<(i32, i32)>>,
}

/// Owns the slide animation and click-through state behind [`WindowOps`]. Holds a
/// geometry/mode snapshot pushed in via [`Controller::configure`] rather than
/// reaching back into App config. Cheap to [`Clone`] (shares one `Arc<Shared>`),
/// so the single-instance reveal-ping handler can hold a handle to call
/// [`Controller::reveal`].
#[derive(Clone)]
pub struct Controller {
    sh: Arc<Shared>,
}

/// A consistent read of the controller's geometry/mode state, taken under the
/// lock so the animation/click-through math sees a coherent picture.
#[derive(Clone)]
struct Snapshot {
    mon: MonitorInfo,
    bar_height: i32,
    pinned: bool,
    user_click_through: bool,
    expanded: bool,
}

fn bottom_docked(s: &Snapshot) -> bool {
    s.mon.dock_edge == "bottom"
}

/// The bar's resting top: below any chrome above it (e.g. the macOS menu bar via
/// `work_top_offset`) when top-docked, hugging the monitor's bottom when
/// bottom-docked.
fn on_screen_y(s: &Snapshot) -> i32 {
    if bottom_docked(s) {
        s.mon.top + s.mon.height - s.bar_height
    } else {
        s.mon.top + s.mon.work_top_offset
    }
}

/// Position of the window when collapsed: positioned off-screen except for a
/// 2-pixel trigger strip at the screen edge.
fn off_screen_y(s: &Snapshot) -> i32 {
    if bottom_docked(s) {
        s.mon.top + s.mon.height - 2
    } else {
        s.mon.top - s.bar_height + 2
    }
}

impl Controller {
    /// Builds a production Controller bound to the given [`WindowOps`] (the
    /// X11/win/mac window adapter). Real clock + ticker.
    pub fn new(ops: Box<dyn WindowOps>) -> Self {
        Self::build(ops, Box::new(Instant::now), Box::new(real_ticker), None)
    }

    fn build(ops: Box<dyn WindowOps>, now: NowFn, new_ticker: NewTicker, on_done: Option<DoneFn>) -> Self {
        Controller {
            sh: Arc::new(Shared {
                ops,
                now,
                new_ticker,
                on_done,
                durs: Mutex::new(Durations::default()),
                st: Mutex::new(State::default()),
                anim_gen: AtomicU64::new(0),
                last_collapse_cursor: Mutex::new(None),
            }),
        }
    }

    fn snap(&self) -> Snapshot {
        let st = self.sh.st.lock().unwrap();
        Snapshot {
            mon: st.mon.clone(),
            bar_height: st.bar_height,
            pinned: st.pinned,
            user_click_through: st.user_click_through,
            expanded: st.expanded,
        }
    }

    /// Refreshes the geometry/mode snapshot and re-applies click-through. Call it
    /// wherever the bar is (re)docked and on pin / click-through changes.
    pub fn configure(&self, mon: MonitorInfo, bar_height: i32, pinned: bool, click_through: bool) {
        {
            let mut st = self.sh.st.lock().unwrap();
            st.mon = mon;
            st.bar_height = bar_height;
            st.pinned = pinned;
            st.user_click_through = click_through;
            st.configured = true;
        }
        self.apply_click_through();
    }

    /// Updates the user click-through preference and re-applies it (the tray
    /// toggle, which changes nothing about geometry).
    pub fn set_user_click_through(&self, enabled: bool) {
        self.sh.st.lock().unwrap().user_click_through = enabled;
        self.apply_click_through();
    }

    /// Sets the initial visual state without animating: pinned ⇒ expanded, else
    /// follow the cursor. When starting collapsed it snaps the window above the
    /// screen edge and hides it so nothing flashes on launch. Call after
    /// [`Controller::configure`].
    pub fn init(&self) {
        let mut s = {
            let st = self.sh.st.lock().unwrap();
            Snapshot {
                mon: st.mon.clone(),
                bar_height: st.bar_height,
                pinned: st.pinned,
                user_click_through: st.user_click_through,
                expanded: false,
            }
        };

        let expanded = s.pinned || self.cursor_over_bar(&s);
        s.expanded = expanded;
        self.sh.st.lock().unwrap().expanded = expanded;

        self.apply_click_through();
        if !expanded {
            self.sh.ops.move_to(s.mon.left, off_screen_y(&s));
            // Full clip so even if a monitor sits above, the window can't spill
            // onto it.
            self.sh.ops.clip_top(width_px(&s.mon), s.bar_height, s.bar_height);
        }
    }

    /// Reports whether the bar is currently on-screen.
    pub fn expanded(&self) -> bool {
        self.sh.st.lock().unwrap().expanded
    }

    /// Slides the bar on-screen (the single-instance re-launch path).
    pub fn reveal(&self) {
        self.set_expanded(true);
    }

    /// Transitions the bar on/off screen by sliding the OS window itself (so the
    /// dark window background travels with the bar, leaving no leftover frame).
    /// No-op if already in the target state or not yet configured. Every call
    /// supersedes any in-flight slide.
    pub fn set_expanded(&self, expanded: bool) {
        let s = {
            let mut st = self.sh.st.lock().unwrap();
            if !st.configured || st.expanded == expanded {
                return;
            }
            st.expanded = expanded;
            Snapshot {
                mon: st.mon.clone(),
                bar_height: st.bar_height,
                pinned: st.pinned,
                user_click_through: st.user_click_through,
                expanded,
            }
        };

        eprintln!("[reveal] set_expanded: {} (pinned: {})", expanded, s.pinned);
        self.apply_click_through();

        if !expanded {
            let (cx, cy) = self.sh.ops.cursor_pos();
            *self.sh.last_collapse_cursor.lock().unwrap() = Some((cx, cy));
            eprintln!("[reveal] set_expanded(false): saved collapse cursor pos: ({}, {})", cx, cy);
        } else {
            *self.sh.last_collapse_cursor.lock().unwrap() = None;
        }

        let target = if expanded {
            on_screen_y(&s)
        } else {
            off_screen_y(&s)
        };
        let generation = self.sh.anim_gen.fetch_add(1, Ordering::SeqCst) + 1;
        if expanded {
            self.sh.ops.show(); // reveal the off-screen window so move_to can slide it in
        }
        let sh = Arc::clone(&self.sh);
        std::thread::spawn(move || animate_y(sh, s, target, generation));
    }

    /// Sets the window's click-through from the user preference OR, where
    /// auto-hide is wired up, the "invisible collapsed" state — so a hidden bar
    /// can't eat clicks. On platforms without auto-hide this reduces to the user
    /// preference alone.
    pub fn apply_click_through(&self) {
        let s = self.snap();
        let auto_hide = self.sh.ops.auto_hide_supported() && !s.pinned && !s.expanded;
        self.sh.ops.set_click_through(s.user_click_through || auto_hide);
    }

    /// Reports whether the OS cursor is inside the bar's hit box.
    /// Since the window is kept mapped and positioned with a 2-pixel trigger strip
    /// at the screen edge when collapsed, we can rely entirely on the window's
    /// hover detection.
    fn cursor_over_bar(&self, s: &Snapshot) -> bool {
        if !self.sh.ops.is_hovered() {
            return false;
        }
        if !s.expanded {
            let (cx, cy) = self.sh.ops.cursor_pos();
            let last_collapse = self.sh.last_collapse_cursor.lock().unwrap();
            if let Some((lcx, lcy)) = *last_collapse {
                if cx == lcx && cy == lcy {
                    return false;
                }
            }
        }
        true
    }

    /// Forces the bar expanded while the inline accounts editor is shown (it's
    /// launched with the cursor off-bar and must stay open until dismissed). On
    /// close, the machine re-evaluates against the current cursor position.
    pub fn set_editor_open(&self, open: bool) {
        let pinned = {
            let mut st = self.sh.st.lock().unwrap();
            st.editor_open = open;
            st.pinned
        };

        if open && !pinned {
            self.set_expanded(true);
        }
        if !open {
            self.tick();
        }
    }

    /// Applies a pin-state change: pinned ⇒ always expanded; unpinned ⇒ follow
    /// the cursor (the user just clicked the pin icon, so the cursor is on the
    /// bar — avoids a flicker before the next poll). Resets the grace timer.
    pub fn set_pinned(&self, pinned: bool) {
        let s = {
            let mut st = self.sh.st.lock().unwrap();
            st.pinned = pinned;
            st.left_bar_at = None;
            Snapshot {
                mon: st.mon.clone(),
                bar_height: st.bar_height,
                pinned,
                user_click_through: st.user_click_through,
                expanded: st.expanded,
            }
        };

        self.apply_click_through();
        self.set_expanded(pinned || self.cursor_over_bar(&s));
    }

    /// Advances the auto-hide state machine one step from the current cursor
    /// position; the OS cursor poller ([`Controller::run`]) calls it. Precedence:
    /// editor-open and pinned force expanded, fullscreen forces collapsed,
    /// otherwise the bar follows the cursor and collapses after the grace delay
    /// once the cursor has left.
    pub fn tick(&self) {
        let s = {
            let st = self.sh.st.lock().unwrap();
            if !st.configured || st.editor_open {
                return;
            }
            Snapshot {
                mon: st.mon.clone(),
                bar_height: st.bar_height,
                pinned: st.pinned,
                user_click_through: st.user_click_through,
                expanded: st.expanded,
            }
        };

        let (cx, cy) = self.sh.ops.cursor_pos();
        thread_local! {
            static LAST_POS: std::cell::Cell<(i32, i32)> = std::cell::Cell::new((-999, -999));
        }
        LAST_POS.with(|cell| {
            let last = cell.get();
            if last != (cx, cy) {
                cell.set((cx, cy));
                eprintln!("[reveal] cursor moved to ({}, {}), pinned={}, expanded={}", cx, cy, s.pinned, s.expanded);
            }
        });

        // Fullscreen takes precedence over pin/hover: while a frontmost app is in
        // native fullscreen, force-collapse the bar (the tray icon stays). On
        // platforms with no fullscreen detection this is a no-op.
        if self.sh.ops.full_screen_active(&s.mon) {
            if self.expanded() {
                self.set_expanded(false);
            }
            return;
        }
        if s.pinned {
            if !self.expanded() {
                self.set_expanded(true);
            }
            return;
        }
        if self.cursor_over_bar(&s) {
            self.sh.st.lock().unwrap().left_bar_at = None;
            self.set_expanded(true);
            return;
        }
        // Cursor off the bar — start the grace timer on the first off-tick; only
        // collapse once it's been gone for collapse_delay.
        let collapse_delay = self.sh.durs.lock().unwrap().collapse_delay;
        let grace_elapsed = {
            let mut st = self.sh.st.lock().unwrap();
            match st.left_bar_at {
                None => {
                    st.left_bar_at = Some((self.sh.now)());
                    return;
                }
                Some(t) => (self.sh.now)().saturating_duration_since(t) >= collapse_delay,
            }
        };
        if grace_elapsed && self.expanded() {
            self.set_expanded(false);
        }
    }

    /// Polls the cursor every `poll` and drives the machine via [`Controller::tick`]
    /// on a background thread until `stop` is signalled. App starts this once the
    /// native window handle is known.
    pub fn spawn_run(&self) -> RunHandle {
        let sh = Arc::clone(&self.sh);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = stop.clone();
        let poll = self.sh.durs.lock().unwrap().poll;
        let ctrl = Controller { sh };
        std::thread::spawn(move || loop {
            std::thread::sleep(poll);
            if s2.load(Ordering::Relaxed) {
                return;
            }
            ctrl.tick();
        });
        RunHandle { stop }
    }
}

/// Stops the [`Controller::spawn_run`] poll loop when dropped or via `stop`.
pub struct RunHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl RunHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Slides the window's top edge to `target_y` over `slide` with an ease-out
/// cubic, repositioning the top clip each frame so the portion above `mon.top`
/// stays masked (multi-monitor spill). A newer `set_expanded` bumps
/// `anim_gen`; this loop sees the bump and exits without touching the window.
fn animate_y(sh: Arc<Shared>, s: Snapshot, target_y: i32, generation: u64) {
    animate_y_run(&sh, s, target_y, generation);
    if let Some(d) = &sh.on_done {
        d(generation);
    }
}

fn animate_y_run(sh: &Shared, s: Snapshot, target_y: i32, generation: u64) {
    let x = s.mon.left;
    let mon_top = s.mon.top;
    let width = width_px(&s.mon);
    let bar_h = s.bar_height;
    let bottom = bottom_docked(&s);

    // Once any pixel has crossed above mon.top, clip one extra pixel to absorb
    // DPI/rounding slop that would otherwise leave a row on the monitor above.
    let clip_for = |y: i32| -> i32 {
        if bottom {
            return 0; // slides off the bottom; nothing above to spill onto
        }
        let top = mon_top - y;
        if top > 0 {
            top + 1
        } else {
            top
        }
    };

    let (_, start_y, _, _) = sh.ops.window_rect();
    eprintln!("[reveal] animate_y_run: start_y={}, target_y={}", start_y, target_y);
    if start_y == target_y {
        return;
    }
    let start = (sh.now)();
    let (slide, frame) = {
        let g = sh.durs.lock().unwrap();
        (g.slide, g.frame)
    };
    let (tick_c, stop) = (sh.new_ticker)(frame);

    for _ in tick_c.iter() {
        if sh.anim_gen.load(Ordering::SeqCst) != generation {
            break; // superseded by a newer slide
        }
        let elapsed = (sh.now)().saturating_duration_since(start);
        if elapsed >= slide {
            sh.ops.move_to(x, target_y);
            sh.ops.clip_top(width, bar_h, clip_for(target_y));
            break;
        }
        let t = elapsed.as_secs_f64() / slide.as_secs_f64();
        let t = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t); // ease-out cubic
        let y = start_y + ((target_y - start_y) as f64 * t) as i32;
        sh.ops.move_to(x, y);
        sh.ops.clip_top(width, bar_h, clip_for(y));
    }
    stop();
}

#[cfg(test)]
mod tests {
    include!("reveal_tests.rs");
}
