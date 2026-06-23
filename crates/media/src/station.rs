//! [`StationPlayer`] — a 1:1 port of Go's `internal/station/player.go`. It owns
//! the radio queue (flatten items, expand playlists, optional shuffle), drives a
//! single-track [`TrackController`] one `play_video` at a time, and
//! auto-advances / loops / skips-on-fail. It sits ABOVE the controller; the
//! controller stays single-track and owns URL resolution + retry.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use rand::Rng;
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use clawdpanel_types::{StationConfig, StationItemKind};

use crate::error::{Error, Result};
use crate::event::{EmitFn, Event, State};
use crate::parse::{has_multiple_tracks, parse_item};
use crate::resolver::PlaylistExpander;

/// Caps how many consecutive dead tracks we skip before declaring a station
/// unavailable, regardless of queue length. (Go `maxFailStreak`.)
const MAX_FAIL_STREAK: usize = 25;

/// The slice of the audio controller the station player drives. A trait so the
/// queue logic unit-tests with a fake (Go's `trackController` interface).
#[async_trait]
pub trait TrackController: Send + Sync {
    async fn play_video(&self, video_id: &str) -> Result<()>;
    fn resume(&self) -> Result<()>;
    fn pause(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn set_volume(&self, v: f64) -> Result<()>;
}

/// Mutable station state (Go `StationPlayer`'s mutexed fields). `cancel_expand`
/// lives here so it is taken/replaced atomically with the epoch bump.
struct StationState {
    stations: Vec<StationConfig>,
    active_idx: usize,
    queue: Vec<String>,
    cur: usize,
    shuffle: bool,
    paused: bool,
    fail_streak: usize,
    epoch: u64,
    cancel_expand: Option<CancellationToken>,
}

impl StationState {
    fn new() -> Self {
        StationState {
            stations: Vec::new(),
            active_idx: 0,
            queue: Vec::new(),
            cur: 0,
            shuffle: false,
            paused: false,
            fail_streak: 0,
            epoch: 0,
            cancel_expand: None,
        }
    }

    /// Moves `cur` to the next track. Shuffle → a random track other than the
    /// current one; else step sequentially, wrapping to 0 (loop). (Go
    /// `advanceLocked`.)
    fn advance_locked(&mut self) -> Option<String> {
        let n = self.queue.len();
        if n == 0 {
            return None;
        }
        if self.shuffle && n > 1 {
            let mut next = rand::thread_rng().gen_range(0..n - 1);
            if next >= self.cur {
                next += 1;
            }
            self.cur = next;
        } else {
            self.cur += 1;
            if self.cur >= n {
                self.cur = 0;
            }
        }
        Some(self.queue[self.cur].clone())
    }

    /// Steps `cur` back one, wrapping to the end. Always sequential (no shuffle
    /// history to walk). (Go `retreatLocked`.)
    fn retreat_locked(&mut self) -> Option<String> {
        let n = self.queue.len();
        if n == 0 {
            return None;
        }
        if self.cur == 0 {
            self.cur = n - 1;
        } else {
            self.cur -= 1;
        }
        Some(self.queue[self.cur].clone())
    }

    /// Skip-on-failure threshold: ≤ queue length, capped at `MAX_FAIL_STREAK`,
    /// at least 1. (Go `failLimitLocked`.)
    fn fail_limit_locked(&self) -> usize {
        self.queue.len().clamp(1, MAX_FAIL_STREAK)
    }
}

/// The radio queue engine. Use as an `Arc<StationPlayer>` — `play`, `next`,
/// `prev` and `on_audio_event` spawn tasks that hold a clone.
pub struct StationPlayer {
    ctrl: Arc<dyn TrackController>,
    resolver: Arc<dyn PlaylistExpander>,
    emit: EmitFn,
    state: Mutex<StationState>,
    rt: Handle,
}

impl StationPlayer {
    /// Builds a station player wrapping `ctrl` + `resolver`; `emit` forwards
    /// (enriched) events to the UI.
    pub fn new(
        ctrl: Arc<dyn TrackController>,
        resolver: Arc<dyn PlaylistExpander>,
        emit: EmitFn,
        rt: Handle,
    ) -> Arc<Self> {
        Arc::new(StationPlayer {
            ctrl,
            resolver,
            emit,
            state: Mutex::new(StationState::new()),
            rt,
        })
    }

    /// Replaces the known station list (config load/save).
    pub fn set_stations(&self, st: Vec<StationConfig>) {
        self.state.lock().stations = st;
    }

    fn forward(&self, ev: Event) {
        (self.emit)(ev);
    }

    fn epoch_changed(&self, e: u64) -> bool {
        self.state.lock().epoch != e
    }

    /// Reports whether the station at `idx` can be stepped track-by-track
    /// (config-only; the `RadioStationHasTracks` binding).
    pub fn station_has_tracks(&self, idx: usize) -> bool {
        let st = self.state.lock();
        st.stations.get(idx).map(has_multiple_tracks).unwrap_or(false)
    }

    /// (Re)starts the station at `station_idx`. Same active station with an
    /// existing queue → resume the current track (keep your exact place); else a
    /// fresh start that (re)builds the queue. (Go `Play`.)
    pub fn play(self: &Arc<Self>, station_idx: usize) -> Result<()> {
        let (station, epoch, cancel) = {
            let mut st = self.state.lock();
            if station_idx >= st.stations.len() {
                return Err(Error::new("station index out of range"));
            }
            // Resume the active station from its exact paused position.
            if station_idx == st.active_idx && !st.queue.is_empty() {
                st.paused = false;
                drop(st);
                return self.ctrl.resume();
            }
            // Switch / fresh start: bump epoch, cancel any prior expansion.
            st.epoch += 1;
            let epoch = st.epoch;
            if let Some(tok) = st.cancel_expand.take() {
                tok.cancel();
            }
            let cancel = CancellationToken::new();
            st.cancel_expand = Some(cancel.clone());
            st.active_idx = station_idx;
            let station = st.stations[station_idx].clone();
            st.shuffle = station.shuffle;
            st.queue.clear();
            st.cur = 0;
            st.fail_streak = 0;
            st.paused = false;
            (station, epoch, cancel)
        };

        // Loading feedback while the (possibly playlist-backed) queue is built.
        self.forward(Event {
            station_idx: station_idx as i32,
            ..Event::state(State::Loading)
        });
        let me = Arc::clone(self);
        self.rt.spawn(async move { me.build_and_start(epoch, station, cancel).await });
        Ok(())
    }

    /// Flattens the station into the queue and starts playback: sequential →
    /// append incrementally + start at queue[0] the moment it's known; shuffle →
    /// expand everything, then start at a random track. (Go `buildAndStart`.)
    async fn build_and_start(
        self: Arc<Self>,
        epoch: u64,
        station: StationConfig,
        cancel: CancellationToken,
    ) {
        let mut started = false;
        for item in &station.items {
            if self.epoch_changed(epoch) {
                return;
            }
            // Re-parse on the fly to upgrade any legacy saved items.
            let actual = if !item.raw.is_empty() {
                parse_item(&item.raw).unwrap_or_else(|_| item.clone())
            } else {
                item.clone()
            };

            let ids: Vec<String> = match actual.kind {
                StationItemKind::Playlist => {
                    match self
                        .resolver
                        .expand_playlist(&actual.id, false, cancel.clone())
                        .await
                    {
                        Ok(got) => got,
                        Err(e) => {
                            log::warn!("[station] expand playlist {} failed: {e}", actual.id);
                            continue;
                        }
                    }
                }
                _ => {
                    if actual.id.is_empty() {
                        Vec::new()
                    } else {
                        vec![actual.id.clone()]
                    }
                }
            };
            if ids.is_empty() {
                continue;
            }

            let start_id = {
                let mut st = self.state.lock();
                if epoch != st.epoch {
                    return;
                }
                st.queue.extend(ids);
                if !started && !station.shuffle {
                    st.cur = 0;
                    Some(st.queue[0].clone())
                } else {
                    None
                }
            };
            if let Some(id) = start_id {
                started = true;
                let me = Arc::clone(&self);
                self.rt.spawn(async move { me.play_track(epoch, id).await });
            }
        }

        // Finalize: shuffle-start at a random track, or report an empty station.
        let shuffle_start = {
            let mut st = self.state.lock();
            if epoch != st.epoch {
                return;
            }
            if st.queue.is_empty() {
                let active_idx = st.active_idx as i32;
                drop(st);
                self.forward(Event {
                    station_idx: active_idx,
                    ..Event::error("", "station has no playable items")
                });
                return;
            }
            if station.shuffle && !started {
                st.cur = rand::thread_rng().gen_range(0..st.queue.len());
                Some(st.queue[st.cur].clone())
            } else {
                None
            }
        };
        if let Some(id) = shuffle_start {
            let me = Arc::clone(&self);
            self.rt.spawn(async move { me.play_track(epoch, id).await });
        }
    }

    /// Plays a single track via the controller unless the epoch moved on.
    /// Failures surface as a `StateError` the controller emits, which
    /// `on_audio_event` turns into skip-on-failure. (Go `playTrack`.)
    async fn play_track(self: Arc<Self>, epoch: u64, id: String) {
        if self.epoch_changed(epoch) {
            return;
        }
        if let Err(e) = self.ctrl.play_video(&id).await {
            log::warn!("[station] play {id} failed: {e}");
        }
    }

    /// Receives every controller event: auto-advance on natural end, skip dead
    /// tracks on terminal error, forward to the UI stamped with the active
    /// station index. (Go `OnAudioEvent`.)
    pub fn on_audio_event(self: &Arc<Self>, ev: Event) {
        // Progress ticks forward straight (stamped), skipping the machine.
        if ev.progress {
            let active_idx = self.state.lock().active_idx as i32;
            self.forward(Event { station_idx: active_idx, ..ev });
            return;
        }

        let (action, active_idx, epoch, next_id) = {
            let mut st = self.state.lock();
            let active_idx = st.active_idx as i32;
            let epoch = st.epoch;
            let cur_id = st.queue.get(st.cur).cloned().unwrap_or_default();

            let mut action = Action::None;
            match ev.state {
                State::Playing => {
                    st.fail_streak = 0;
                    st.paused = false;
                }
                State::Paused => st.paused = true,
                State::Ended => {
                    // Natural end of the currently-playing track → advance + loop.
                    if cur_id.is_empty() || ev.video_id.is_empty() || ev.video_id == cur_id {
                        st.fail_streak = 0;
                        action = Action::Advance;
                    }
                }
                State::Error
                    // Terminal error for the current track → skip to next, or give
                    // up if the whole queue is dead.
                    if !cur_id.is_empty() && (ev.video_id.is_empty() || ev.video_id == cur_id) => {
                        st.fail_streak += 1;
                        action = if st.fail_streak >= st.fail_limit_locked() {
                            Action::GiveUp
                        } else {
                            Action::Skip
                        };
                    }
                _ => {}
            }

            let mut next_id = String::new();
            if matches!(action, Action::Advance | Action::Skip) {
                match st.advance_locked() {
                    Some(id) => next_id = id,
                    None => action = Action::None,
                }
            }
            (action, active_idx, epoch, next_id)
        };

        // Forward (suppressing the raw error while skipping a dead track).
        match action {
            Action::None | Action::Advance => {
                self.forward(Event { station_idx: active_idx, ..ev });
            }
            Action::GiveUp => {
                let _ = self.stop();
                self.forward(Event {
                    station_idx: active_idx,
                    ..Event::error("", "station unavailable")
                });
            }
            Action::Skip => {}
        }

        if matches!(action, Action::Advance | Action::Skip) {
            let me = Arc::clone(self);
            self.rt.spawn(async move { me.play_track(epoch, next_id).await });
        }
    }

    /// Pauses playback (keeps queue/position).
    pub fn pause(&self) -> Result<()> {
        self.state.lock().paused = true;
        self.ctrl.pause()
    }

    /// Pure mode toggle (never starts/jumps playback). (Go `SetShuffle`.)
    pub fn set_shuffle(&self, station_idx: usize, on: bool) -> Result<()> {
        let mut st = self.state.lock();
        if station_idx < st.stations.len() {
            st.stations[station_idx].shuffle = on;
        }
        if station_idx == st.active_idx {
            st.shuffle = on;
        }
        Ok(())
    }

    /// Manually advances to the next track within the active station.
    pub fn next(self: &Arc<Self>) -> Result<()> {
        let (id, epoch) = {
            let mut st = self.state.lock();
            if st.queue.is_empty() {
                return Ok(());
            }
            (st.advance_locked(), st.epoch)
        };
        if let Some(id) = id {
            let me = Arc::clone(self);
            self.rt.spawn(async move { me.play_track(epoch, id).await });
        }
        Ok(())
    }

    /// Manually steps back to the previous track within the active station.
    pub fn prev(self: &Arc<Self>) -> Result<()> {
        let (id, epoch) = {
            let mut st = self.state.lock();
            if st.queue.is_empty() {
                return Ok(());
            }
            (st.retreat_locked(), st.epoch)
        };
        if let Some(id) = id {
            let me = Arc::clone(self);
            self.rt.spawn(async move { me.play_track(epoch, id).await });
        }
        Ok(())
    }

    /// Halts playback, clears the queue, cancels any in-flight expansion.
    pub fn stop(&self) -> Result<()> {
        {
            let mut st = self.state.lock();
            st.epoch += 1;
            if let Some(tok) = st.cancel_expand.take() {
                tok.cancel();
            }
            st.queue.clear();
            st.cur = 0;
            st.fail_streak = 0;
            st.paused = false;
        }
        self.ctrl.stop()
    }

    /// Delegates to the controller (config persistence is the app's job).
    pub fn set_volume(&self, v: f64) -> Result<()> {
        self.ctrl.set_volume(v)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Advance,
    Skip,
    GiveUp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawdpanel_types::StationItem;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc as stdmpsc;
    use std::time::{Duration, Instant};

    // ── fakes ──

    struct FakeController {
        played_tx: stdmpsc::Sender<String>,
        played_rx: Mutex<stdmpsc::Receiver<String>>,
        stop_count: AtomicUsize,
        resume_count: AtomicUsize,
        fail_ids: Mutex<HashMap<String, bool>>,
    }
    impl FakeController {
        fn new() -> Arc<Self> {
            let (tx, rx) = stdmpsc::channel();
            Arc::new(FakeController {
                played_tx: tx,
                played_rx: Mutex::new(rx),
                stop_count: AtomicUsize::new(0),
                resume_count: AtomicUsize::new(0),
                fail_ids: Mutex::new(HashMap::new()),
            })
        }
        fn next_played(&self) -> String {
            self.played_rx
                .lock()
                .recv_timeout(Duration::from_secs(2))
                .expect("timed out waiting for play_video")
        }
        fn try_next_played(&self, d: Duration) -> Option<String> {
            self.played_rx.lock().recv_timeout(d).ok()
        }
    }
    #[async_trait]
    impl TrackController for FakeController {
        async fn play_video(&self, id: &str) -> Result<()> {
            let _ = self.played_tx.send(id.to_string());
            if *self.fail_ids.lock().get(id).unwrap_or(&false) {
                return Err(Error::new("dead track"));
            }
            Ok(())
        }
        fn resume(&self) -> Result<()> {
            self.resume_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn pause(&self) -> Result<()> {
            Ok(())
        }
        fn stop(&self) -> Result<()> {
            self.stop_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn set_volume(&self, _v: f64) -> Result<()> {
            Ok(())
        }
    }

    struct FakeExpander {
        m: HashMap<String, Vec<String>>,
    }
    #[async_trait]
    impl PlaylistExpander for FakeExpander {
        async fn expand_playlist(
            &self,
            id: &str,
            _force: bool,
            _cancel: CancellationToken,
        ) -> Result<Vec<String>> {
            Ok(self.m.get(id).cloned().unwrap_or_default())
        }
    }

    /// Blocks until released, cancelled, or a 2s fallback (Go `blockingExpander`).
    struct BlockingExpander {
        release: Mutex<Option<stdmpsc::Receiver<()>>>,
        ids: Vec<String>,
    }
    #[async_trait]
    impl PlaylistExpander for BlockingExpander {
        async fn expand_playlist(
            &self,
            _id: &str,
            _force: bool,
            cancel: CancellationToken,
        ) -> Result<Vec<String>> {
            // Poll the std release channel without holding state, honouring cancel.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if cancel.is_cancelled() {
                    return Err(Error::new("cancelled"));
                }
                {
                    let guard = self.release.lock();
                    if let Some(rx) = guard.as_ref() {
                        if rx.try_recv().is_ok() {
                            return Ok(self.ids.clone());
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return Ok(self.ids.clone());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn noop_emit() -> EmitFn {
        Arc::new(|_ev| {})
    }

    fn vid(id: &str) -> StationItem {
        StationItem { kind: StationItemKind::Video, id: id.into(), raw: String::new() }
    }
    fn pl(id: &str) -> StationItem {
        StationItem { kind: StationItemKind::Playlist, id: id.into(), raw: String::new() }
    }
    fn station(name: &str, items: Vec<StationItem>, shuffle: bool) -> StationConfig {
        StationConfig { name: name.into(), items, shuffle }
    }

    fn wait_queue_len(s: &Arc<StationPlayer>, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if s.state.lock().queue.len() == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("queue never reached length {want}");
    }

    // ── pure advance/retreat (no runtime) ──

    #[test]
    fn advance_locked_loops() {
        let mut st = StationState::new();
        st.queue = vec!["a".into(), "b".into(), "c".into()];
        let want = ["b", "c", "a", "b"];
        for w in want {
            assert_eq!(st.advance_locked().as_deref(), Some(w));
        }
    }

    #[test]
    fn advance_shuffle_no_immediate_repeat() {
        let mut st = StationState::new();
        st.queue = vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()];
        st.shuffle = true;
        for _ in 0..200 {
            let prev = st.cur;
            let id = st.advance_locked().expect("ok on non-empty queue");
            assert_ne!(st.cur, prev, "shuffle advance repeated index {prev}");
            assert_eq!(id, st.queue[st.cur]);
        }
    }

    // ── full-player tests (need a runtime) ──

    #[test]
    fn build_sequential_queue() {
        let r = rt();
        let fc = FakeController::new();
        let fe = Arc::new(FakeExpander {
            m: HashMap::from([("P".to_string(), vec!["p1".into(), "p2".into()])]),
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        s.set_stations(vec![station("S", vec![vid("a"), pl("P"), vid("b")], false)]);

        s.play(0).unwrap();
        assert_eq!(fc.next_played(), "a");
        wait_queue_len(&s, 4);
        assert_eq!(s.state.lock().queue, vec!["a", "p1", "p2", "b"]);
    }

    #[test]
    fn ended_advances_and_forwards() {
        let r = rt();
        let fc = FakeController::new();
        let events = Arc::new(Mutex::new(Vec::<State>::new()));
        let ev2 = events.clone();
        let emit: EmitFn = Arc::new(move |ev: Event| ev2.lock().push(ev.state));
        let s = StationPlayer::new(fc.clone(), Arc::new(FakeExpander { m: HashMap::new() }), emit, r.handle().clone());
        {
            let mut st = s.state.lock();
            st.queue = vec!["a".into(), "b".into()];
            st.cur = 0;
        }
        s.on_audio_event(Event::with_video(State::Ended, "a"));
        assert_eq!(fc.next_played(), "b");
        assert_eq!(events.lock().first().copied(), Some(State::Ended));
    }

    #[test]
    fn ended_wraps_to_start_and_loops() {
        let r = rt();
        let fc = FakeController::new();
        let s = StationPlayer::new(fc.clone(), Arc::new(FakeExpander { m: HashMap::new() }), noop_emit(), r.handle().clone());
        {
            let mut st = s.state.lock();
            st.queue = vec!["only".into()];
            st.cur = 0;
        }
        s.on_audio_event(Event::with_video(State::Ended, "only"));
        assert_eq!(fc.next_played(), "only");
    }

    #[test]
    fn stale_ended_ignored() {
        let r = rt();
        let fc = FakeController::new();
        let s = StationPlayer::new(fc.clone(), Arc::new(FakeExpander { m: HashMap::new() }), noop_emit(), r.handle().clone());
        {
            let mut st = s.state.lock();
            st.queue = vec!["a".into(), "b".into()];
            st.cur = 1; // currently on "b"
        }
        // Late StateEnded for the already-passed "a" must not advance.
        s.on_audio_event(Event::with_video(State::Ended, "a"));
        assert!(fc.try_next_played(Duration::from_millis(100)).is_none());
    }

    #[test]
    fn skip_on_failure() {
        let r = rt();
        let fc = FakeController::new();
        let s = StationPlayer::new(fc.clone(), Arc::new(FakeExpander { m: HashMap::new() }), noop_emit(), r.handle().clone());
        {
            let mut st = s.state.lock();
            st.queue = vec!["a".into(), "b".into(), "c".into()];
            st.cur = 0;
        }
        s.on_audio_event(Event::error("a", "dead"));
        assert_eq!(fc.next_played(), "b");
    }

    #[test]
    fn give_up_when_all_dead() {
        let r = rt();
        let fc = FakeController::new();
        let last_err = Arc::new(Mutex::new(String::new()));
        let le = last_err.clone();
        let emit: EmitFn = Arc::new(move |ev: Event| {
            if ev.state == State::Error {
                *le.lock() = ev.err;
            }
        });
        let s = StationPlayer::new(fc.clone(), Arc::new(FakeExpander { m: HashMap::new() }), emit, r.handle().clone());
        {
            let mut st = s.state.lock();
            st.queue = vec!["a".into()]; // single dead track → give up immediately
            st.cur = 0;
        }
        s.on_audio_event(Event::error("a", "dead"));
        assert!(fc.try_next_played(Duration::from_millis(100)).is_none(), "gave up but still played");
        assert!(fc.stop_count.load(Ordering::SeqCst) > 0, "expected Stop on give-up");
        assert_eq!(*last_err.lock(), "station unavailable");
    }

    #[test]
    fn playing_resets_fail_streak() {
        let r = rt();
        let fc = FakeController::new();
        let s = StationPlayer::new(fc.clone(), Arc::new(FakeExpander { m: HashMap::new() }), noop_emit(), r.handle().clone());
        s.state.lock().fail_streak = 5;
        s.on_audio_event(Event::with_video(State::Playing, "a"));
        assert_eq!(s.state.lock().fail_streak, 0);
    }

    #[test]
    fn play_out_of_range() {
        let r = rt();
        let fc = FakeController::new();
        let s = StationPlayer::new(fc, Arc::new(FakeExpander { m: HashMap::new() }), noop_emit(), r.handle().clone());
        s.set_stations(vec![station("only", vec![], false)]);
        assert!(s.play(5).is_err());
    }

    #[test]
    fn epoch_cancels_stale_expansion() {
        let r = rt();
        let fc = FakeController::new();
        let (rel_tx, rel_rx) = stdmpsc::channel::<()>();
        let fe = Arc::new(BlockingExpander {
            release: Mutex::new(Some(rel_rx)),
            ids: vec!["x1".into(), "x2".into()],
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        s.set_stations(vec![
            station("A", vec![pl("PA")], false),
            station("B", vec![vid("b")], false),
        ]);

        s.play(0).unwrap(); // blocking expansion at epoch 1
        s.play(1).unwrap(); // switch before A returns → epoch bumps + cancels
        assert_eq!(fc.next_played(), "b");
        // Let the stale expansion finish; its results must be discarded.
        let _ = rel_tx.send(());
        std::thread::sleep(Duration::from_millis(80));
        let q = s.state.lock().queue.clone();
        assert!(!q.iter().any(|id| id == "x1" || id == "x2"), "stale expansion leaked: {q:?}");
    }

    #[test]
    fn shuffle_start_plays_random_queue_track() {
        let r = rt();
        let fc = FakeController::new();
        let fe = Arc::new(FakeExpander {
            m: HashMap::from([(
                "P".to_string(),
                (1..=10).map(|i| format!("p{i}")).collect(),
            )]),
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        s.set_stations(vec![station("S", vec![pl("P")], true)]);

        s.play(0).unwrap();
        let first = fc.next_played();
        wait_queue_len(&s, 10);
        let st = s.state.lock();
        assert!(st.queue.contains(&first), "first played not in queue");
        assert_eq!(st.queue[st.cur], first, "first played != queue[cur]");
    }

    #[test]
    fn set_shuffle_is_mode_only_toggle() {
        let r = rt();
        let fc = FakeController::new();
        let fe = Arc::new(FakeExpander {
            m: HashMap::from([("P".to_string(), (1..=5).map(|i| format!("p{i}")).collect())]),
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        s.set_stations(vec![station("S", vec![pl("P")], false)]);

        s.play(0).unwrap();
        let first = fc.next_played(); // sequential start → p1
        wait_queue_len(&s, 5);

        // Toggling shuffle must not start/stop/jump playback.
        s.set_shuffle(0, true).unwrap();
        assert!(fc.try_next_played(Duration::from_millis(150)).is_none(), "shuffle toggle triggered playback");
        assert!(s.state.lock().shuffle);

        // The mode still takes effect on the next advance.
        s.next().unwrap();
        assert_ne!(fc.next_played(), first, "shuffled advance replayed the same track");
    }

    #[test]
    fn prev_steps_backward_with_wrap() {
        let r = rt();
        let fc = FakeController::new();
        let fe = Arc::new(FakeExpander {
            m: HashMap::from([("P".to_string(), vec!["p1".into(), "p2".into(), "p3".into()])]),
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        s.set_stations(vec![station("S", vec![pl("P")], false)]);

        s.play(0).unwrap();
        assert_eq!(fc.next_played(), "p1");
        wait_queue_len(&s, 3);

        s.prev().unwrap();
        assert_eq!(fc.next_played(), "p3", "prev from first wraps to end");
        s.prev().unwrap();
        assert_eq!(fc.next_played(), "p2");
    }

    #[test]
    fn play_resumes_via_controller() {
        let r = rt();
        let fc = FakeController::new();
        let fe = Arc::new(FakeExpander {
            m: HashMap::from([("P".to_string(), vec!["p1".into(), "p2".into(), "p3".into()])]),
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        s.set_stations(vec![station("S", vec![pl("P")], false)]);

        s.play(0).unwrap();
        let _ = fc.next_played(); // p1
        wait_queue_len(&s, 3);

        s.pause().unwrap();
        // Re-play the active station: must RESUME, not start a new PlayVideo.
        s.play(0).unwrap();
        assert!(fc.try_next_played(Duration::from_millis(150)).is_none(), "resume should not call play_video");
        assert_eq!(fc.resume_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn legacy_upgrade_on_the_fly() {
        let r = rt();
        let fc = FakeController::new();
        let fe = Arc::new(FakeExpander {
            m: HashMap::from([(
                "PLAbcdEfGhIjKlMnOpQrSt".to_string(),
                vec!["p1".into(), "p2".into()],
            )]),
        });
        let s = StationPlayer::new(fc.clone(), fe, noop_emit(), r.handle().clone());
        // Saved as ItemVideo, but Raw carries a list= playlist parameter.
        s.set_stations(vec![station(
            "Legacy",
            vec![StationItem {
                kind: StationItemKind::Video,
                id: "EWrX250Zhko".into(),
                raw: "https://www.youtube.com/watch?v=EWrX250Zhko&list=PLAbcdEfGhIjKlMnOpQrSt".into(),
            }],
            false,
        )]);

        s.play(0).unwrap();
        wait_queue_len(&s, 2);
        assert_eq!(s.state.lock().queue, vec!["p1", "p2"]);
    }
}
