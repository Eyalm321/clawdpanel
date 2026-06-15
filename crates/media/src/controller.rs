//! Single-track audio [`Controller`] — a 1:1 port of Go's
//! `internal/audio/controller.go`: retry-once on a stale URL, sticky error,
//! progress throttle, live zeroing, duplicate-state suppression.
//!
//! ## Threading contract (load-bearing)
//!
//! Player events arrive over a bounded channel and are processed by a dedicated
//! drain task ([`Controller::new`]); outward events are *queued* (never emitted
//! synchronously while the state lock is held). The Go code dodges a
//! self-deadlock here: emits happen under the controller mutex, and downstream
//! (the station player) reacts by calling straight back into the controller,
//! which would re-enter the same mutex. We keep the discipline two ways — the
//! state mutex is only held for short, await-free critical sections, and
//! [`Inner::handle_player_event`] computes its outcome under the lock but only
//! sends / spawns *after* dropping it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::backend;
use crate::error::Result;
use crate::event::{EmitFn, Event, State};
use crate::player::Player;
use crate::resolver::StreamResolver;
use crate::station::TrackController;

/// Throttle for the playhead/timeline ticks forwarded to the UI. The player
/// polls every 100ms (needed for EOS detection); ~2.5 timeline updates/sec is
/// plenty and keeps the event channel quiet. (Go `progressInterval`.)
const PROGRESS_INTERVAL: Duration = Duration::from_millis(400);

/// Mutable controller state (Go `Controller`'s mutexed fields).
struct CtrlState {
    active_video_id: String,
    active_url: String,
    current_state: State,
    cur_is_live: bool,
    retried: bool,
    last_progress: Option<Instant>,
}

impl CtrlState {
    fn new() -> Self {
        CtrlState {
            active_video_id: String::new(),
            active_url: String::new(),
            current_state: State::Idle,
            cur_is_live: false,
            retried: false,
            last_progress: None,
        }
    }
}

/// Shared controller internals. `Controller` is a thin `Arc<Inner>` handle; the
/// station holds `Arc<dyn TrackController>` = this same `Arc<Inner>`.
pub struct Inner {
    state: Mutex<CtrlState>,
    resolver: Arc<dyn StreamResolver>,
    player: Box<dyn Player>,
    /// Outward-event channel (`Some` in production: the dispatcher task drains
    /// it and calls `emit`). `None` in unit tests → synchronous emit.
    emit_tx: Option<mpsc::Sender<Event>>,
    emit: Mutex<Option<EmitFn>>,
    rt: Handle,
}

/// What [`Inner::handle_player_event`] decided to do, computed under the lock and
/// carried out after it is dropped.
enum Outcome {
    Nothing,
    Emit(Event),
    /// Kick the once-off forced re-resolve + replay for this video id.
    Retry(String),
}

impl Inner {
    /// Queues an outward event. Drops (with a log) if the dispatcher is
    /// saturated rather than blocking a caller that may matter; delivers
    /// synchronously when constructed without a dispatcher (tests).
    fn send(&self, ev: Event) {
        if let Some(tx) = &self.emit_tx {
            if tx.try_send(ev.clone()).is_err() {
                log::warn!("[audio] controller event queue full, dropping {}", ev.state.as_str());
            }
            return;
        }
        let emit = self.emit.lock().clone();
        if let Some(emit) = emit {
            emit(ev);
        }
    }

    /// Processes one raw player event (Go `handlePlayerEvent`). Runs on the drain
    /// task; takes `&Arc<Self>` so the retry path can spawn a task owning a
    /// clone.
    fn handle_player_event(self: &Arc<Self>, ev: Event) {
        let outcome = {
            let mut st = self.state.lock();

            // A terminal error is sticky until the next Play/Resume: the station
            // reacts to StateError by stopping the engine, and that stop's own
            // StateIdle must not repaint [ERR] back to a blank play button.
            if st.current_state == State::Error && ev.state == State::Idle {
                return;
            }

            // Duplicate state: suppress, EXCEPT a steady playing tick carries an
            // advanced playhead — forward it (throttled) as a Progress event so
            // the seek timeline moves without re-triggering the status/marquee.
            if ev.state == st.current_state && ev.err.is_empty() {
                if st.current_state == State::Playing
                    && st.last_progress.map_or(true, |t| t.elapsed() >= PROGRESS_INTERVAL)
                {
                    st.last_progress = Some(Instant::now());
                    let (pos, dur) = if st.cur_is_live {
                        (0.0, 0.0) // livestream NaturalDuration is a sentinel → show LIVE
                    } else {
                        (ev.position, ev.duration)
                    };
                    let mut prog = Event::with_video(State::Playing, st.active_video_id.clone());
                    prog.progress = true;
                    prog.position = pos;
                    prog.duration = dur;
                    drop(st);
                    self.send(prog);
                }
                return;
            }

            if ev.state == State::Error {
                if !st.retried && !st.active_video_id.is_empty() {
                    st.retried = true;
                    Outcome::Retry(st.active_video_id.clone())
                } else {
                    // Second error propagates (fall through to the update below).
                    Self::apply_and_build(&mut st, ev)
                }
            } else if ev.state == State::Loading && st.current_state == State::Playing {
                // Suppress a (re)load while already playing.
                Outcome::Nothing
            } else {
                Self::apply_and_build(&mut st, ev)
            }
        };

        match outcome {
            Outcome::Nothing => {}
            Outcome::Emit(ev) => self.send(ev),
            Outcome::Retry(video_id) => {
                let me = Arc::clone(self);
                self.rt.spawn(async move { me.do_retry(video_id).await });
            }
        }
    }

    /// Commits a non-duplicate, non-retry event to state and returns it for
    /// emit, applying the video-id default + live zeroing (Go's tail).
    fn apply_and_build(st: &mut CtrlState, mut ev: Event) -> Outcome {
        st.current_state = ev.state;
        if ev.video_id.is_empty() {
            ev.video_id = st.active_video_id.clone();
        }
        if st.cur_is_live {
            ev.position = 0.0;
            ev.duration = 0.0;
        }
        Outcome::Emit(ev)
    }

    /// The retry-once body: re-resolve with `force_refresh=true` and replay,
    /// unless the target video changed meanwhile. Runs as its own task.
    async fn do_retry(self: Arc<Self>, video_id: String) {
        let fresh = match self.resolver.resolve(&video_id, true).await {
            Ok(t) => t,
            Err(e) => {
                self.send(Event::error(video_id, e.to_string()));
                return;
            }
        };
        {
            let mut st = self.state.lock();
            if st.active_video_id != video_id {
                return; // target changed in the meantime
            }
            st.active_url = fresh.url.clone();
            st.cur_is_live = fresh.is_live;
        }
        self.player.set_live_hint(fresh.is_live);
        if let Err(e) = self.player.play(&fresh.url) {
            self.send(Event::error(video_id, e.to_string()));
        }
    }

    // ── controller methods (driven by the station / app) ──

    fn resume(&self) -> Result<()> {
        let id = {
            let st = self.state.lock();
            if st.active_video_id.is_empty() {
                return Ok(());
            }
            st.active_video_id.clone()
        };
        self.player.resume()?;
        self.state.lock().current_state = State::Playing;
        self.send(Event::with_video(State::Playing, id));
        Ok(())
    }

    fn pause(&self) -> Result<()> {
        self.player.pause()
    }

    fn stop(&self) -> Result<()> {
        {
            let mut st = self.state.lock();
            st.active_video_id.clear();
            st.current_state = State::Idle;
        }
        self.player.stop()
    }

    fn set_volume(&self, v: f64) -> Result<()> {
        self.player.set_volume(v)
    }

    fn seek(&self, seconds: f64) -> Result<()> {
        {
            let st = self.state.lock();
            if st.active_video_id.is_empty() {
                return Ok(());
            }
        }
        self.player.seek(seconds)
    }

    fn close(&self) -> Result<()> {
        self.player.close()
    }
}

#[async_trait]
impl TrackController for Inner {
    async fn play_video(&self, video_id: &str) -> Result<()> {
        {
            let st = self.state.lock();
            if st.active_video_id == video_id && st.current_state == State::Playing {
                return Ok(());
            }
        }
        {
            let mut st = self.state.lock();
            st.active_video_id = video_id.to_string();
            st.current_state = State::Loading;
            st.retried = false;
        }
        self.send(Event::with_video(State::Loading, video_id));

        let track = match self.resolver.resolve(video_id, false).await {
            Ok(t) => t,
            Err(e) => {
                self.state.lock().current_state = State::Error;
                self.send(Event::error(video_id, e.to_string()));
                return Err(e);
            }
        };
        {
            let mut st = self.state.lock();
            if st.active_video_id != video_id {
                return Ok(()); // superseded by a newer PlayVideo
            }
            st.active_url = track.url.clone();
            st.cur_is_live = track.is_live;
        }
        self.player.set_live_hint(track.is_live);
        if let Err(e) = self.player.play(&track.url) {
            self.send(Event::error(video_id, e.to_string()));
            return Err(e);
        }
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        Inner::resume(self)
    }
    fn pause(&self) -> Result<()> {
        Inner::pause(self)
    }
    fn stop(&self) -> Result<()> {
        Inner::stop(self)
    }
    fn set_volume(&self, v: f64) -> Result<()> {
        Inner::set_volume(self, v)
    }
}

/// Single-track player controller. Cheap to clone (`Arc` handle).
#[derive(Clone)]
pub struct Controller(Arc<Inner>);

impl Controller {
    /// Builds the controller with the platform's native player, wiring the
    /// player→controller drain and the controller→emit dispatcher onto `rt`.
    /// Call [`set_emit`](Controller::set_emit) once the station is built.
    pub fn new(resolver: Arc<dyn StreamResolver>, rt: Handle) -> Result<Self> {
        let (ptx, prx) = mpsc::channel::<Event>(64);
        let player = backend::new_player(ptx)?;
        let (etx, erx) = mpsc::channel::<Event>(64);
        let inner = Arc::new(Inner {
            state: Mutex::new(CtrlState::new()),
            resolver,
            player,
            emit_tx: Some(etx),
            emit: Mutex::new(None),
            rt: rt.clone(),
        });
        spawn_drains(&inner, prx, erx, &rt);
        Ok(Controller(inner))
    }

    /// Late-binds the outward-event sink (the station player's `on_audio_event`).
    pub fn set_emit(&self, emit: EmitFn) {
        *self.0.emit.lock() = Some(emit);
    }

    /// The station-facing handle (so the station drives this controller through
    /// the [`TrackController`] seam).
    pub fn track_controller(&self) -> Arc<dyn TrackController> {
        self.0.clone()
    }

    pub fn resume(&self) -> Result<()> {
        self.0.resume()
    }
    pub fn pause(&self) -> Result<()> {
        self.0.pause()
    }
    pub fn stop(&self) -> Result<()> {
        self.0.stop()
    }
    pub fn set_volume(&self, v: f64) -> Result<()> {
        self.0.set_volume(v)
    }
    pub fn seek(&self, seconds: f64) -> Result<()> {
        self.0.seek(seconds)
    }
    pub fn close(&self) -> Result<()> {
        self.0.close()
    }
}

/// Spawns the player→controller drain (raw events → `handle_player_event`) and
/// the controller→emit dispatcher (queued outward events → `emit`). Both hold
/// only a `Weak` so they exit once the controller is dropped.
fn spawn_drains(
    inner: &Arc<Inner>,
    mut prx: mpsc::Receiver<Event>,
    mut erx: mpsc::Receiver<Event>,
    rt: &Handle,
) {
    let weak = Arc::downgrade(inner);
    rt.spawn(async move {
        while let Some(ev) = prx.recv().await {
            match weak.upgrade() {
                Some(inner) => inner.handle_player_event(ev),
                None => break,
            }
        }
    });
    let weak = Arc::downgrade(inner);
    rt.spawn(async move {
        while let Some(ev) = erx.recv().await {
            let emit = match weak.upgrade() {
                Some(inner) => inner.emit.lock().clone(),
                None => break,
            };
            if let Some(emit) = emit {
                emit(ev);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ResolvedTrack;
    use std::sync::mpsc as stdmpsc;

    /// A mock native player whose `play` runs a test-supplied closure, which can
    /// emit player events back through the chan-A sender (the Go mock's
    /// `go c.handlePlayerEvent(...)`).
    struct MockPlayer {
        tx: mpsc::Sender<Event>,
        play_fn: Mutex<Box<dyn FnMut(&str, &mpsc::Sender<Event>) -> Result<()> + Send>>,
    }
    impl Player for MockPlayer {
        fn play(&self, url: &str) -> Result<()> {
            (self.play_fn.lock())(url, &self.tx)
        }
        fn resume(&self) -> Result<()> {
            Ok(())
        }
        fn pause(&self) -> Result<()> {
            Ok(())
        }
        fn stop(&self) -> Result<()> {
            Ok(())
        }
        fn set_volume(&self, _v: f64) -> Result<()> {
            Ok(())
        }
        fn seek(&self, _s: f64) -> Result<()> {
            Ok(())
        }
    }

    /// Records resolve calls + their force flag; URL chosen by a closure.
    struct MockResolver {
        calls: Mutex<Vec<(String, bool)>>,
        url_fn: Box<dyn Fn(&str, bool) -> Result<ResolvedTrack> + Send + Sync>,
    }
    #[async_trait]
    impl StreamResolver for MockResolver {
        async fn resolve(&self, video_id: &str, force: bool) -> Result<ResolvedTrack> {
            self.calls.lock().push((video_id.to_string(), force));
            (self.url_fn)(video_id, force)
        }
    }

    /// Builds a test controller with a synchronous emit (`emit_tx = None`) and a
    /// mock player wired to chan A; the drain runs on `rt`.
    fn build(
        rt: &Handle,
        resolver: Arc<dyn StreamResolver>,
        emit: EmitFn,
        play_fn: impl FnMut(&str, &mpsc::Sender<Event>) -> Result<()> + Send + 'static,
    ) -> Controller {
        let (ptx, prx) = mpsc::channel::<Event>(64);
        let player = Box::new(MockPlayer {
            tx: ptx,
            play_fn: Mutex::new(Box::new(play_fn)),
        });
        let inner = Arc::new(Inner {
            state: Mutex::new(CtrlState::new()),
            resolver,
            player,
            emit_tx: None,
            emit: Mutex::new(Some(emit)),
            rt: rt.clone(),
        });
        let weak = Arc::downgrade(&inner);
        let mut prx = prx;
        rt.spawn(async move {
            while let Some(ev) = prx.recv().await {
                match weak.upgrade() {
                    Some(inner) => inner.handle_player_event(ev),
                    None => break,
                }
            }
        });
        Controller(inner)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    // Ported from Go controller_test.go::TestController_PlayNormal.
    #[test]
    fn play_normal_emits_loading_and_plays() {
        let rt = rt();
        let events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let ev2 = events.clone();
        let emit: EmitFn = Arc::new(move |ev| ev2.lock().push(ev));
        let resolver = Arc::new(MockResolver {
            calls: Mutex::new(Vec::new()),
            url_fn: Box::new(|id, _| Ok(ResolvedTrack { url: format!("http://test.url/{id}"), is_live: false })),
        });
        let played = Arc::new(Mutex::new(None::<String>));
        let p2 = played.clone();
        let ctrl = build(rt.handle(), resolver, emit, move |url, _tx| {
            *p2.lock() = Some(url.to_string());
            Ok(())
        });

        rt.block_on(ctrl.track_controller().play_video("vid123")).unwrap();

        assert_eq!(played.lock().as_deref(), Some("http://test.url/vid123"));
        let evs = events.lock();
        assert_eq!(evs.len(), 1, "events: {evs:?}");
        assert_eq!(evs[0].state, State::Loading);
        assert_eq!(evs[0].video_id, "vid123");
    }

    // Ported from Go controller_test.go::TestController_RetryOnce.
    #[test]
    fn retry_once_then_plays() {
        let rt = rt();
        let (done_tx, done_rx) = stdmpsc::channel::<()>();
        let emit: EmitFn = Arc::new(move |ev: Event| {
            if ev.state == State::Playing {
                let _ = done_tx.send(());
            }
        });
        let resolver = Arc::new(MockResolver {
            calls: Mutex::new(Vec::new()),
            url_fn: Box::new(|id, force| {
                let url = if force { format!("http://refreshed.url/{id}") } else { format!("http://initial.url/{id}") };
                Ok(ResolvedTrack { url, is_live: false })
            }),
        });
        let resolver_calls = resolver.clone();
        let play_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pc = play_count.clone();
        let ctrl = build(rt.handle(), resolver, emit, move |url, tx| {
            let n = pc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == 1 {
                assert_eq!(url, "http://initial.url/vid123");
                let _ = tx.try_send(Event::error("", "HLS manifest expired"));
            } else if n == 2 {
                assert_eq!(url, "http://refreshed.url/vid123");
                let _ = tx.try_send(Event::state(State::Playing));
            }
            Ok(())
        });

        rt.block_on(ctrl.track_controller().play_video("vid123")).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).expect("timed out waiting for playing");

        assert_eq!(play_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        let calls = resolver_calls.calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, false);
        assert_eq!(calls[1].1, true);
    }

    // Ported from Go controller_test.go::TestController_RetryFail.
    #[test]
    fn retry_fail_propagates_second_error() {
        let rt = rt();
        let (done_tx, done_rx) = stdmpsc::channel::<()>();
        let emit: EmitFn = Arc::new(move |ev: Event| {
            if ev.state == State::Error && ev.err == "HLS manifest expired again" {
                let _ = done_tx.send(());
            }
        });
        let resolver = Arc::new(MockResolver {
            calls: Mutex::new(Vec::new()),
            url_fn: Box::new(|id, _| Ok(ResolvedTrack { url: format!("http://test.url/{id}"), is_live: false })),
        });
        let play_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pc = play_count.clone();
        let ctrl = build(rt.handle(), resolver, emit, move |_url, tx| {
            let n = pc.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == 1 {
                let _ = tx.try_send(Event::error("", "HLS manifest expired"));
            } else if n == 2 {
                let _ = tx.try_send(Event::error("", "HLS manifest expired again"));
            }
            Ok(())
        });

        rt.block_on(ctrl.track_controller().play_video("vid123")).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).expect("timed out waiting for terminal error");

        assert_eq!(play_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
