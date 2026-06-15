// Ported from Go: internal/reveal/reveal_test.go (test doubles) +
// internal/reveal/machine_test.go (Tick precedence/grace tests). These drive the
// auto-hide state machine through `tick` with a fake cursor (FakeOps.cursor_pos)
// and a fake clock (ManualClock), so the grace-timer and precedence rules are
// deterministic.
//
// On-bar hit box for test_mon(): x ∈ [100, 2020), y ∈ [50, 90).

use super::*;
use crate::{MonitorInfo, WindowOps};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

// ── test doubles ────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeState {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    moves: Vec<(i32, i32)>,
    shows: i32,
    hides: i32,
    click_sets: Vec<bool>,
    auto_hide: bool,
    full_screen: bool,
    cursor_x: i32,
    cursor_y: i32,
}

/// A [`WindowOps`] that records every call and lets tests read back the window
/// position. `window_rect` returns the last `move_to`, so a follow-up slide
/// continues from where the previous one left off.
#[derive(Clone)]
struct FakeOps {
    st: Arc<Mutex<FakeState>>,
}

impl FakeOps {
    fn new(auto_hide: bool) -> Self {
        FakeOps {
            st: Arc::new(Mutex::new(FakeState {
                auto_hide,
                ..Default::default()
            })),
        }
    }
    fn set_pos(&self, x: i32, y: i32) {
        let mut s = self.st.lock().unwrap();
        s.x = x;
        s.y = y;
    }
    fn set_cursor(&self, x: i32, y: i32) {
        let mut s = self.st.lock().unwrap();
        s.cursor_x = x;
        s.cursor_y = y;
    }
    fn set_full_screen(&self, v: bool) {
        self.st.lock().unwrap().full_screen = v;
    }
    fn move_count(&self) -> usize {
        self.st.lock().unwrap().moves.len()
    }
    fn hide_count(&self) -> i32 {
        self.st.lock().unwrap().hides
    }
    fn last_move(&self) -> (i32, i32) {
        let s = self.st.lock().unwrap();
        *s.moves.last().unwrap()
    }
    fn last_click_through(&self) -> Option<bool> {
        self.st.lock().unwrap().click_sets.last().copied()
    }
}

impl WindowOps for FakeOps {
    fn window_rect(&self) -> (i32, i32, i32, i32) {
        let s = self.st.lock().unwrap();
        (s.x, s.y, s.w, s.h)
    }
    fn move_to(&self, x: i32, y: i32) {
        let mut s = self.st.lock().unwrap();
        s.x = x;
        s.y = y;
        s.moves.push((x, y));
    }
    fn clip_top(&self, _w: i32, _h: i32, _t: i32) {}
    fn show(&self) {
        self.st.lock().unwrap().shows += 1;
    }
    fn hide(&self) {
        self.st.lock().unwrap().hides += 1;
    }
    fn set_click_through(&self, e: bool) {
        self.st.lock().unwrap().click_sets.push(e);
    }
    fn cursor_pos(&self) -> (i32, i32) {
        let s = self.st.lock().unwrap();
        (s.cursor_x, s.cursor_y)
    }
    fn full_screen_active(&self, _mon: &MonitorInfo) -> bool {
        self.st.lock().unwrap().full_screen
    }
    fn auto_hide_supported(&self) -> bool {
        self.st.lock().unwrap().auto_hide
    }
}

/// Drives the animation deterministically: time only moves when the test calls
/// `advance`, and frames only fire when the test calls `tick`. Each `animate_y`
/// thread gets its own ticker channel (appended in creation order).
#[derive(Clone)]
struct ManualClock {
    inner: Arc<Mutex<ManualInner>>,
}

struct ManualInner {
    base: Instant,
    offset: Duration,
    chans: Vec<Sender<Instant>>,
}

impl ManualClock {
    fn new() -> Self {
        ManualClock {
            inner: Arc::new(Mutex::new(ManualInner {
                base: Instant::now(),
                offset: Duration::ZERO,
                chans: Vec::new(),
            })),
        }
    }
    fn now_fn(&self) -> NowFn {
        let i = self.inner.clone();
        Box::new(move || {
            let g = i.lock().unwrap();
            g.base + g.offset
        })
    }
    fn new_ticker_fn(&self) -> NewTicker {
        let i = self.inner.clone();
        Box::new(move |_d| {
            let (tx, rx) = channel();
            i.lock().unwrap().chans.push(tx);
            (rx, Box::new(|| {}) as StopFn)
        })
    }
    fn advance(&self, d: Duration) {
        self.inner.lock().unwrap().offset += d;
    }
    fn ticker_count(&self) -> usize {
        self.inner.lock().unwrap().chans.len()
    }
    fn tick(&self, idx: usize) {
        let g = self.inner.lock().unwrap();
        let now = g.base + g.offset;
        let _ = g.chans[idx].send(now);
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

const BAR_HEIGHT: i32 = 40;

// onScreen Y = top + work_top_offset = 50; offScreen Y = top - bar_height = 10.
fn test_mon() -> MonitorInfo {
    MonitorInfo {
        left: 100,
        top: 50,
        width: 1920,
        phys_width: 1920,
        work_top_offset: 0,
        ..Default::default()
    }
}

fn new_test_controller(fake: &FakeOps, clk: &ManualClock, done: Option<DoneFn>) -> Controller {
    let c = Controller::build(
        Box::new(fake.clone()),
        clk.now_fn(),
        clk.new_ticker_fn(),
        done,
    );
    c.sh.durs.lock().unwrap().slide = Duration::from_millis(100);
    c
}

fn done_channel() -> (DoneFn, std::sync::mpsc::Receiver<u64>) {
    let (tx, rx) = channel::<u64>();
    let tx = Mutex::new(tx);
    (Box::new(move |g| { let _ = tx.lock().unwrap().send(g); }), rx)
}

fn wait_for<F: Fn() -> bool>(cond: F, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("timed out waiting for {what}");
}

fn recv_done(rx: &std::sync::mpsc::Receiver<u64>) -> u64 {
    rx.recv_timeout(Duration::from_secs(2))
        .expect("timed out waiting for animation to finish")
}

// ── slide / supersede / click-through tests (reveal_test.go) ─────────────────

#[test]
fn slide_reaches_target() {
    let fake = FakeOps::new(false);
    fake.set_pos(100, 10); // start collapsed (off-screen)
    let clk = ManualClock::new();
    let (done, rx) = done_channel();
    let c = new_test_controller(&fake, &clk, Some(done));
    c.configure(test_mon(), BAR_HEIGHT, false, false);

    // Expand → reaches on-screen Y (50).
    c.set_expanded(true);
    wait_for(|| clk.ticker_count() >= 1, "expand ticker");
    let slide = c.sh.durs.lock().unwrap().slide;
    clk.advance(slide); // elapsed >= slide → final frame
    clk.tick(0);
    recv_done(&rx);
    assert_eq!(fake.last_move(), (100, 50), "after expand");

    // Collapse → reaches off-screen Y (10) and hides.
    c.set_expanded(false);
    wait_for(|| clk.ticker_count() >= 2, "collapse ticker");
    clk.advance(slide);
    clk.tick(1);
    recv_done(&rx);
    assert_eq!(fake.last_move(), (100, 10), "after collapse");
    assert!(fake.hide_count() > 0, "collapse did not hide the window");
}

#[test]
fn new_reveal_supersedes_in_flight() {
    let fake = FakeOps::new(false);
    fake.set_pos(100, 50); // start expanded (on-screen)
    let clk = ManualClock::new();
    let (done, rx) = done_channel();
    let c = new_test_controller(&fake, &clk, Some(done));
    c.configure(test_mon(), BAR_HEIGHT, false, false);
    c.sh.st.lock().unwrap().expanded = true; // start expanded (skip the initial slide-in)

    let slide = c.sh.durs.lock().unwrap().slide;

    // Begin collapsing (generation 1).
    c.set_expanded(false);
    wait_for(|| clk.ticker_count() >= 1, "collapse ticker");

    // One partial frame: the window moves part-way toward off-screen.
    clk.advance(slide / 4);
    clk.tick(0);
    wait_for(|| fake.move_count() >= 1, "partial collapse frame");
    let partial = fake.last_move();
    assert!(partial.1 < 50 && partial.1 > 10, "partial frame y = {}", partial.1);
    let moves_before_supersede = fake.move_count();

    // Supersede with a reveal (generation 2). The collapse thread is still
    // blocked on its ticker.
    c.set_expanded(true);
    wait_for(|| clk.ticker_count() >= 2, "reveal ticker");

    // Fire the collapse thread's next frame: it sees the bumped generation and
    // bails without moving the window.
    clk.tick(0);
    assert_eq!(recv_done(&rx), 1, "expected superseded collapse (gen 1) to finish first");
    assert_eq!(
        fake.move_count(),
        moves_before_supersede,
        "superseded slide kept moving the window"
    );

    // Drive the reveal to completion: it reaches the on-screen target.
    clk.advance(slide);
    clk.tick(1);
    assert_eq!(recv_done(&rx), 2, "expected reveal (gen 2) to finish");
    assert_eq!(fake.last_move(), (100, 50), "after supersede");
}

#[test]
fn apply_click_through() {
    struct Case {
        name: &'static str,
        auto_hide: bool,
        pinned: bool,
        expanded: bool,
        user_ct: bool,
        want: bool,
    }
    let cases = [
        Case { name: "collapsed autohide forces clickthrough", auto_hide: true, pinned: false, expanded: false, user_ct: false, want: true },
        Case { name: "expanded does not force clickthrough", auto_hide: true, pinned: false, expanded: true, user_ct: false, want: false },
        Case { name: "pinned ignores autohide", auto_hide: true, pinned: true, expanded: false, user_ct: false, want: false },
        Case { name: "unsupported follows user pref on", auto_hide: false, pinned: false, expanded: false, user_ct: true, want: true },
        Case { name: "unsupported follows user pref off", auto_hide: false, pinned: false, expanded: false, user_ct: false, want: false },
    ];
    for tc in cases {
        let fake = FakeOps::new(tc.auto_hide);
        let c = Controller::new(Box::new(fake.clone()));
        // Set the state directly: pinned+collapsed can't arise via Init (pinned
        // forces expanded), but ApplyClickThrough must still handle it.
        {
            let mut st = c.sh.st.lock().unwrap();
            st.configured = true;
            st.mon = test_mon();
            st.bar_height = BAR_HEIGHT;
            st.pinned = tc.pinned;
            st.expanded = tc.expanded;
            st.user_click_through = tc.user_ct;
        }
        c.apply_click_through();
        let got = fake.last_click_through().expect("set_click_through was never called");
        assert_eq!(got, tc.want, "case: {}", tc.name);
    }
}

// ── Tick grace / precedence tests (machine_test.go) ──────────────────────────

#[test]
fn tick_grace_delays_collapse() {
    let fake = FakeOps::new(true);
    let clk = ManualClock::new();
    let c = new_test_controller(&fake, &clk, None);
    c.sh.durs.lock().unwrap().collapse_delay = Duration::from_millis(100);
    c.configure(test_mon(), BAR_HEIGHT, false, false); // unpinned

    fake.set_cursor(150, 60); // on-bar
    c.init();
    assert!(c.expanded(), "Init with cursor on the bar should start expanded");

    // Cursor leaves: the first off-bar tick starts the grace timer only.
    fake.set_cursor(150, 300); // off-bar
    c.tick();
    assert!(c.expanded(), "first off-bar tick must not collapse");

    // Still inside the grace window: no collapse.
    let cd = c.sh.durs.lock().unwrap().collapse_delay;
    clk.advance(cd - Duration::from_millis(1));
    c.tick();
    assert!(c.expanded(), "collapsed before the grace delay elapsed");

    // Grace elapsed: collapse.
    clk.advance(Duration::from_millis(2));
    c.tick();
    assert!(!c.expanded(), "bar should have collapsed after the grace delay");
}

#[test]
fn tick_cursor_return_cancels_collapse() {
    let fake = FakeOps::new(true);
    let clk = ManualClock::new();
    let c = new_test_controller(&fake, &clk, None);
    c.sh.durs.lock().unwrap().collapse_delay = Duration::from_millis(100);
    c.configure(test_mon(), BAR_HEIGHT, false, false);

    fake.set_cursor(150, 60); // on-bar
    c.init(); // expanded

    // Cursor leaves: start the grace timer.
    fake.set_cursor(150, 300); // off-bar
    c.tick();

    // Part-way through the grace window the cursor returns.
    let cd = c.sh.durs.lock().unwrap().collapse_delay;
    clk.advance(cd / 2);
    fake.set_cursor(150, 60); // back on-bar
    c.tick();
    assert!(c.expanded(), "cursor back on the bar must keep it expanded");

    // Cursor leaves again; the grace timer restarts from now, so a tick past what
    // would have been the original deadline must not collapse.
    fake.set_cursor(150, 300); // off-bar
    c.tick(); // restarts the grace timer
    clk.advance(cd / 2 + Duration::from_millis(1));
    c.tick();
    assert!(c.expanded(), "collapse should have been cancelled and the grace timer restarted");
}

#[test]
fn tick_precedence_fullscreen_forces_collapse() {
    let fake = FakeOps::new(true);
    let c = new_test_controller(&fake, &ManualClock::new(), None);
    c.configure(test_mon(), BAR_HEIGHT, false, false);
    fake.set_cursor(150, 60); // on-bar — would normally keep it expanded
    c.init(); // expanded
    fake.set_full_screen(true);

    c.tick();
    assert!(!c.expanded(), "fullscreen must force collapse even with the cursor on the bar");
}

#[test]
fn tick_precedence_fullscreen_beats_pinned() {
    let fake = FakeOps::new(true);
    let c = new_test_controller(&fake, &ManualClock::new(), None);
    c.configure(test_mon(), BAR_HEIGHT, true, false); // pinned
    c.init(); // pinned ⇒ expanded
    fake.set_full_screen(true);

    c.tick();
    assert!(!c.expanded(), "fullscreen must force collapse even when pinned");
}

#[test]
fn tick_precedence_pinned_forces_expanded() {
    let fake = FakeOps::new(true);
    let c = new_test_controller(&fake, &ManualClock::new(), None);
    // Pinned but currently collapsed (e.g. just after a fullscreen suppression):
    // a tick must restore it.
    c.configure(test_mon(), BAR_HEIGHT, true, false);
    fake.set_cursor(150, 300); // off-bar — irrelevant while pinned
    c.tick();
    wait_for(|| c.expanded(), "pinned forces expanded");
}

#[test]
fn tick_precedence_editor_open_forces_expanded() {
    let fake = FakeOps::new(true);
    let clk = ManualClock::new();
    let c = new_test_controller(&fake, &clk, None);
    c.configure(test_mon(), BAR_HEIGHT, false, false);
    fake.set_cursor(150, 300); // off-bar
    c.init(); // collapsed
    assert!(!c.expanded(), "precondition: should start collapsed off-bar");

    c.set_editor_open(true);
    assert!(c.expanded(), "opening the editor must expand the bar");

    // While the editor is open an off-bar tick must NOT collapse.
    clk.advance(Duration::from_secs(1));
    c.tick();
    assert!(c.expanded(), "editor open must suppress hover collapse");
}
