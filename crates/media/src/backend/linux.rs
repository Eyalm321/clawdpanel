//! Linux audio backend — a 1:1 port of Go's `internal/audio/audio_linux.go`
//! onto `gstreamer-rs`. Classic `playbin` (not playbin3 — we need `GstPlayFlags`)
//! with audio-only flags, the PAUSED-preroll → `ASYNC_DONE` → buffer-fill →
//! PLAYING promotion dance (the DASH "playing but silent" fix), a bus-polling
//! thread, and a 100ms position poll. Live radio rides this exact path: the
//! resolver staticizes the dynamic live MPD (see [`crate::staticize`]) and serves
//! it as static DASH reported NOT live, so the same managed preroll keeps it from
//! scheduling audio hours ahead (silence). VOD uses it too.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::collections::HashSet;
use once_cell::sync::Lazy;

use gstreamer as gst;
use gst::prelude::*;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::event::{Event, State};
use crate::player::Player;

/// The user-intent half of the player's state for the current track; gates which
/// pipeline state changes reach the UI. (Go `playbackPhase`.)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Prerolling,
    Playing,
    Paused,
}
impl Phase {
    /// Whether the intent is "audio should be (or become) audible".
    fn wants_playback(self) -> bool {
        matches!(self, Phase::Prerolling | Phase::Playing)
    }
}

/// How long a "playing" pipeline may run with zero advancing playback position
/// (past the initial buffer fill) before we declare it wedged. The trigger case:
/// a broken / empty live HLS playlist that `hlsdemux` retries every
/// target-duration forever, spamming `assertion 'streams != NULL' failed` and
/// never posting a bus error — so neither GStreamer nor the controller ever
/// gives up. On timeout we tear the pipeline down (stopping the hot-loop) and
/// post a terminal `StateError`, which drives the controller's retry-once and
/// the station's fail-limit give-up. Generous so a slow-but-real start (network
/// jitter, a heavy preroll) never trips it; only a stream that produces no audio
/// at all does. Go never hit this (kkdai returns working manifests); the
/// `rusty_ytdl` extraction gap makes it reachable, so the Rust player guards it.
const STALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-track player state, reset by `play`. The initial-fill fields are
/// deliberately orthogonal to `phase` — the buffering hold must survive phase
/// changes. (Go `trackState`.)
struct Track {
    phase: Phase,
    live: bool,
    prerolled: bool,
    fill_done: bool,
    low_buffer: bool,
    confirmed: bool,
    /// Stops the position-poll ticks after EOS until the next `play` (so a stale
    /// tick can't re-flip the UI to playing after the station has advanced).
    ended: bool,
    last_pos: i64, // ns; -1 = none yet
    /// True once the playback position has actually advanced — distinguishes
    /// "really producing audio" from a pipeline that merely reports PLAYING
    /// (a wedged live demux does the latter). Gates the stall watchdog off once
    /// any real progress is seen.
    progressed: bool,
    /// The stall watchdog fires at most once per track.
    watchdog_fired: bool,
    /// When this track's pipeline was (re)started, for the stall watchdog.
    started_at: Option<Instant>,
}

impl Track {
    fn reset(live: bool) -> Self {
        Track {
            phase: if live { Phase::Playing } else { Phase::Prerolling },
            live,
            prerolled: false,
            fill_done: live, // live tracks have no managed fill
            low_buffer: false,
            confirmed: false,
            ended: false,
            last_pos: -1,
            progressed: false,
            watchdog_fired: false,
            started_at: Some(Instant::now()),
        }
    }
}

/// Shared backend state (the bus thread + the `Player` methods both touch it).
struct Shared {
    playbin: gst::Element,
    name: String,
    tx: mpsc::Sender<Event>,
    track: Mutex<Track>,
    live_hint: AtomicBool,
    stop: AtomicBool,
}

impl Shared {
    /// Queues an event for the controller; drops if saturated (Go `send`).
    fn send(&self, ev: Event) {
        if self.tx.try_send(ev.clone()).is_err() {
            log::warn!("[audio] event queue full, dropping {}", ev.state.as_str());
        }
    }

    fn is_from_playbin(&self, msg: &gst::Message) -> bool {
        msg.src().map(|s| s.name()) == Some(self.name.as_str().into())
    }
}

/// The GStreamer `playbin` player.
pub struct LinuxPlayer {
    shared: Arc<Shared>,
    bus_thread: Mutex<Option<JoinHandle<()>>>,
}

static GST_INIT: std::sync::Once = std::sync::Once::new();

pub fn new_player(events: mpsc::Sender<Event>) -> Result<Box<dyn Player>> {
    let mut init_err: Option<String> = None;
    GST_INIT.call_once(|| {
        if let Err(e) = gst::init() {
            init_err = Some(e.to_string());
        }
    });
    if let Some(e) = init_err {
        return Err(Error::new(format!("gstreamer init: {e}")));
    }

    // Classic playbin (playbin3 dropped the GstPlayFlags "flags" property we rely
    // on for audio-only playback).
    let playbin = gst::ElementFactory::make("playbin")
        .name("radio-playbin")
        .build()
        .map_err(|e| Error::new(format!("create playbin (gstreamer1.0-plugins-base?): {e}")))?;

    // Audio-only: GST_PLAY_FLAG_AUDIO | GST_PLAY_FLAG_SOFT_VOLUME. The bar never
    // renders video, and decoding the stream's video track would need codecs not
    // always shipped. NOTE: no "buffer-duration" — on live streams a fixed large
    // target is never reached and the pipeline sits in buffering limbo forever.
    playbin.set_property_from_str("flags", "audio+soft-volume");

    // Recursively inject User-Agent to all HTTP source elements created in the bin hierarchy
    CONFIGURED_ELEMENTS.lock().clear();
    setup_user_agent_injection(&playbin);

    let shared = Arc::new(Shared {
        name: "radio-playbin".to_string(),
        playbin,
        tx: events,
        track: Mutex::new(Track::reset(false)),
        live_hint: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    {
        let mut t = shared.track.lock();
        t.phase = Phase::Idle;
    }

    let bus_shared = shared.clone();
    let bus_thread = std::thread::Builder::new()
        .name("radio-gst-bus".into())
        .spawn(move || monitor_bus(bus_shared))
        .map_err(|e| Error::new(format!("spawn bus thread: {e}")))?;

    Ok(Box::new(LinuxPlayer {
        shared,
        bus_thread: Mutex::new(Some(bus_thread)),
    }))
}

static CONFIGURED_ELEMENTS: Lazy<Mutex<HashSet<usize>>> = Lazy::new(|| Mutex::new(HashSet::new()));

fn setup_user_agent_injection(element: &gst::Element) {
    use gst::glib::translate::ToGlibPtr;
    let ptr = ToGlibPtr::<*mut gst::ffi::GstElement>::to_glib_none(element).0 as usize;
    {
        let mut set = CONFIGURED_ELEMENTS.lock();
        if !set.insert(ptr) {
            return;
        }
    }

    let name = element.name();
    println!("[UserAgentInject] Inspecting element: {}", name);
    if element.has_property("user-agent", None) {
        println!("[UserAgentInject] Setting user-agent and extra-headers on: {}", name);
        element.set_property("user-agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36");
        
        // Authenticate the DIRECT segment fetches with the user's YouTube (Premium)
        // cookies. This is the throttle-critical path: live DASH segments are pulled
        // straight from googlevideo by souphttpsrc, and an authenticated session is
        // tied to the account, not just the IP — so YouTube stops the IP bot-throttle
        // that 403s these requests.
        let mut header_builder = gst::Structure::builder("headers")
            .field("Referer", "https://www.youtube.com/")
            .field("Origin", "https://www.youtube.com");
        if let Some(cookie) = crate::ytdl::youtube_cookies() {
            println!("[UserAgentInject] attaching YouTube Cookie header ({} bytes) to {}", cookie.len(), name);
            header_builder = header_builder.field("Cookie", cookie);
        } else {
            println!("[UserAgentInject] NO YouTube cookie — segment fetches UNAUTHENTICATED on {}", name);
        }
        let extra_headers = header_builder.build();
        element.set_property("extra-headers", &extra_headers);
    }
    if let Ok(bin) = element.clone().dynamic_cast::<gst::Bin>() {
        println!("[UserAgentInject] Inspecting bin children and connecting element-added signal on bin: {}", name);
        // Recurse on existing children first
        for child in bin.children() {
            setup_user_agent_injection(&child);
        }
        // Connect signal for future children
        bin.connect("element-added", false, move |values| {
            if let Some(sub_element) = values.get(1).and_then(|val| val.get::<gst::Element>().ok()) {
                println!("[UserAgentInject] Bin '{}' added element: {}", name, sub_element.name());
                setup_user_agent_injection(&sub_element);
            }
            None
        });
    }
}

impl Player for LinuxPlayer {
    fn play(&self, url: &str) -> Result<()> {
        let s = &self.shared;
        println!("[play] (re)start pipeline → {url}");
        // Stop previous playback.
        let _ = s.playbin.set_state(gst::State::Ready);
        s.playbin.set_property("uri", url);

        // Preroll in PAUSED first (the gst-launch dance): going straight to
        // PLAYING races the demuxer's initial segment event — DASH segments keep
        // their live timestamps and without the segment mapping settled the sink
        // schedules them hours ahead → position advances, pure silence. The bus
        // thread promotes to PLAYING once preroll settles and the buffer fills.
        let ret = s
            .playbin
            .set_state(gst::State::Paused)
            .map_err(|_| Error::new("failed to set GStreamer state to PAUSED for preroll"))?;

        // True live sources preroll with NO_PREROLL and must never be paused for
        // buffering; they go to PLAYING immediately.
        let live = ret == gst::StateChangeSuccess::NoPreroll || s.live_hint.load(Ordering::SeqCst);
        if live {
            s.playbin
                .set_state(gst::State::Playing)
                .map_err(|_| Error::new("failed to set GStreamer state to PLAYING"))?;
        }
        *s.track.lock() = Track::reset(live);
        s.send(Event::state(State::Loading));
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        let s = &self.shared;
        s.playbin
            .set_state(gst::State::Playing)
            .map_err(|_| Error::new("failed to set GStreamer state to PLAYING"))?;
        {
            let mut t = s.track.lock();
            t.phase = Phase::Playing;
            t.ended = false;
        }
        s.send(Event::state(State::Playing));
        Ok(())
    }

    fn pause(&self) -> Result<()> {
        let s = &self.shared;
        {
            let t = s.track.lock();
            if !t.phase.wants_playback() {
                return Ok(());
            }
        }
        s.playbin
            .set_state(gst::State::Paused)
            .map_err(|_| Error::new("failed to set GStreamer state to PAUSED"))?;
        s.track.lock().phase = Phase::Paused;
        s.send(Event::state(State::Paused));
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let s = &self.shared;
        let _ = s.playbin.set_state(gst::State::Ready);
        s.track.lock().phase = Phase::Idle;
        s.send(Event::state(State::Idle));
        Ok(())
    }

    fn set_volume(&self, v: f64) -> Result<()> {
        // playbin volume is 0.0..=1.0 (clamped).
        self.shared.playbin.set_property("volume", v);
        Ok(())
    }

    fn seek(&self, seconds: f64) -> Result<()> {
        let pos = gst::ClockTime::from_nseconds((seconds.max(0.0) * 1_000_000_000.0) as u64);
        // Best-effort: livestreams / non-seekable sources simply ignore it.
        let _ = self.shared.playbin.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            pos,
        );
        Ok(())
    }

    fn set_live_hint(&self, live: bool) {
        self.shared.live_hint.store(live, Ordering::SeqCst);
    }

    fn close(&self) -> Result<()> {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.bus_thread.lock().take() {
            let _ = h.join();
        }
        let _ = self.shared.playbin.set_state(gst::State::Null);
        Ok(())
    }
}

impl Drop for LinuxPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.bus_thread.lock().take() {
            let _ = h.join();
        }
        let _ = self.shared.playbin.set_state(gst::State::Null);
    }
}

/// The single bus-polling loop: bus messages → `handle_message`, quiet ticks →
/// `poll_position`. (Go `monitorBus`.)
fn monitor_bus(shared: Arc<Shared>) {
    let Some(bus) = shared.playbin.bus() else {
        log::warn!("[audio] failed to get GStreamer bus");
        return;
    };
    let filter = [
        gst::MessageType::Error,
        gst::MessageType::Eos,
        gst::MessageType::StateChanged,
        gst::MessageType::Buffering,
        gst::MessageType::AsyncDone,
    ];
    while !shared.stop.load(Ordering::SeqCst) {
        match bus.timed_pop_filtered(gst::ClockTime::from_mseconds(100), &filter) {
            Some(msg) => handle_message(&shared, &msg),
            None => poll_position(&shared),
        }
    }
}

/// Reports StatePlaying from an advancing position (Go `confirmFromPosition`),
/// AND drives the seek timeline by emitting throttled position ticks while
/// playing — the controller down-samples these to ~2.5 progress events/sec.
fn poll_position(shared: &Arc<Shared>) {
    let (wants, ended) = {
        let t = shared.track.lock();
        (t.phase.wants_playback(), t.ended)
    };
    if !wants || ended {
        return;
    }

    // Stall watchdog: past the initial buffer fill, a pipeline that has never
    // produced an advancing position is wedged (a broken live HLS playlist gst
    // retries forever with no bus error). Tear it down to READY — which stops
    // the hlsdemux hot-loop — and post a terminal error so the controller's
    // retry-once and the station's give-up settle it to a single error state.
    if check_stall(shared) {
        return;
    }

    let Some(pos) = shared.playbin.query_position::<gst::ClockTime>() else {
        return;
    };
    let pos_ns = pos.nseconds() as i64;
    if pos_ns <= 0 {
        return;
    }

    let emit = {
        let mut t = shared.track.lock();
        let advanced = t.last_pos >= 0 && pos_ns > t.last_pos;
        t.last_pos = pos_ns;
        if advanced {
            t.progressed = true;
        }
        if !t.confirmed {
            // Confirm playback the moment the position advances (live pipelines
            // can stream audio while the async PLAYING transition never settles).
            if advanced {
                t.confirmed = true;
                t.phase = Phase::Playing;
                true
            } else {
                false
            }
        } else {
            true
        }
    };
    if emit {
        let dur = shared
            .playbin
            .query_duration::<gst::ClockTime>()
            .map(|d| d.nseconds() as f64 / 1e9)
            .unwrap_or(0.0);
        let mut ev = Event::state(State::Playing);
        ev.position = pos_ns as f64 / 1e9;
        ev.duration = dur;
        shared.send(ev);
    }
}

/// The stall watchdog (see [`STALL_TIMEOUT`]). Returns `true` once it has fired
/// (so the caller skips the rest of the tick). Only arms past the initial buffer
/// fill (`fill_done`) and only when no real progress has been seen, so the
/// managed VOD/DASH preroll-then-fill hold can't trip it; a stream that has ever
/// advanced its position never trips it either. Fires at most once per track.
fn check_stall(shared: &Arc<Shared>) -> bool {
    let fire = {
        let t = shared.track.lock();
        t.started_at.is_some_and(|t0| {
            if t.live && t.confirmed {
                return false;
            }
            stall_should_fire(t.fill_done, t.progressed, t.watchdog_fired, t0.elapsed())
        })
    };
    if !fire {
        return false;
    }
    {
        let mut t = shared.track.lock();
        t.watchdog_fired = true;
        t.phase = Phase::Idle; // stop position ticks until the next play
    }
    log::warn!("[audio] playback stalled (no progress in {STALL_TIMEOUT:?}); stopping pipeline");
    let _ = shared.playbin.set_state(gst::State::Ready);
    shared.send(Event::error("", "playback stalled: no audio"));
    true
}

/// Pure stall-watchdog decision (testable without a live pipeline): fire only
/// past the initial fill, with no real progress ever seen, at most once, after
/// the timeout has elapsed. The `fill_done` gate is load-bearing — it keeps the
/// managed VOD/DASH buffer-fill hold (where the position legitimately hasn't
/// advanced yet) from tripping the watchdog.
fn stall_should_fire(fill_done: bool, progressed: bool, fired: bool, elapsed: Duration) -> bool {
    fill_done && !progressed && !fired && elapsed >= STALL_TIMEOUT
}

/// Dispatches one GStreamer bus message (Go `handleBusMessage`).
fn handle_message(shared: &Arc<Shared>, msg: &gst::Message) {
    use gst::MessageView;
    match msg.view() {
        MessageView::Error(err) => {
            println!("[gst-bus] ERROR: {} | src={:?} | debug={:?}",
                err.error(), msg.src().map(|s| s.name()), err.debug());
            shared.send(Event::error("", err.error().to_string()));
        }
        MessageView::Eos(_) => {
            // Natural end → StateEnded (distinct from idle/paused) so the station
            // can auto-advance. Stop position ticks until the next play.
            shared.track.lock().ended = true;
            shared.send(Event::state(State::Ended));
        }
        MessageView::AsyncDone(_) => {
            if shared.is_from_playbin(msg) {
                handle_async_done(shared);
            }
        }
        MessageView::Buffering(b) => {
            handle_buffering(shared, b.percent());
        }
        MessageView::StateChanged(sc)
            if shared.is_from_playbin(msg) => {
                handle_state_changed(shared, sc.current(), sc.pending());
            }
        _ => {}
    }
}

/// Preroll settled → promote to PLAYING unless held by the initial fill. The
/// set_state runs under the lock so check-and-promote is atomic against a
/// concurrent pause. (Go `handleAsyncDone`.)
fn handle_async_done(shared: &Arc<Shared>) {
    let mut t = shared.track.lock();
    t.prerolled = true;
    let promote = !t.fill_done && !t.low_buffer && t.phase.wants_playback();
    if promote {
        t.fill_done = true;
        let _ = shared.playbin.set_state(gst::State::Playing);
    }
}

/// Manages the PAUSED hold during the initial buffer fill (Go `handleBuffering`).
/// The hold applies ONLY during the initial fill (`!fill_done`): once promoted,
/// re-pausing on every sub-100% dip turns jitter into audible stutter. Keyed on
/// `fill_done`, NOT phase, so a pause/resume mid-fill can't cancel the hold.
fn handle_buffering(shared: &Arc<Shared>, percent: i32) {
    let mut t = shared.track.lock();
    let managed = !t.live && !t.fill_done;
    if managed {
        t.low_buffer = percent < 100;
        if percent < 100 {
            let _ = shared.playbin.set_state(gst::State::Paused);
        } else if t.prerolled && t.phase.wants_playback() {
            t.fill_done = true;
            let _ = shared.playbin.set_state(gst::State::Playing);
        }
    }
}

/// Turns settled playbin state changes into UI events. Phase gates each emit so
/// transitional preroll pauses + the READY bounce in `play` don't flicker the
/// UI; while a mid-fill dip holds the pipeline, every report is suppressed.
/// (Go `handleStateChanged`.)
fn handle_state_changed(shared: &Arc<Shared>, new_state: gst::State, pending: gst::State) {
    let settled = pending == gst::State::VoidPending;
    let ev = {
        let mut t = shared.track.lock();
        if t.low_buffer {
            None
        } else {
            match new_state {
                gst::State::Playing => {
                    // Reported even mid-transition: live pipelines stream audio
                    // while the async change never completes.
                    if t.phase.wants_playback() {
                        t.phase = Phase::Playing;
                        t.confirmed = true;
                        t.started_at = Some(std::time::Instant::now());
                        Some(Event::state(State::Playing))
                    } else {
                        None
                    }
                }
                gst::State::Paused => {
                    if settled && t.phase == Phase::Paused {
                        Some(Event::state(State::Paused))
                    } else {
                        None
                    }
                }
                gst::State::Ready | gst::State::Null => {
                    if settled && t.phase == Phase::Idle {
                        Some(Event::state(State::Idle))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    };
    if let Some(ev) = ev {
        shared.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Headless validation harness for the live-stream periodic buffering fix.
    // Drives the REAL pipeline config (playbin, audio+soft-volume, managed preroll
    // + fill, post-fill buffering dips ignored) against the live /dash URL into a
    // fakesink sync=true (renders at 1x real-time, no audio device needed). Logs
    // buffering% and detects position stalls.
    //
    // The periodic stall is throughput-bound, so it only reproduces on a connection
    // that can't outpace the adaptivedemux refill burst. To compare on any machine:
    //   CLAWD_PREFETCH_DEPTH=2  ... repro_live_buffering   # shallow (old) → stalls
    //   (defaults)              ... repro_live_buffering   # deep readahead → smooth
    // Knobs: CLAWD_REPRO_SECS (run length), CLAWD_REPRO_VID (live id; rotates),
    // CLAWD_PREFETCH_DEPTH / CLAWD_SEG_TTL_S / CLAWD_SEG_CONCURRENCY (proxy tuning).
    //
    //   cargo test -p clawdpanel-media repro_live_buffering -- --nocapture --ignored
    #[test]
    #[ignore]
    fn repro_live_buffering() {
        use crate::ytdl::YtdlResolver;
        use crate::resolver::StreamResolver;

        let secs: u64 = std::env::var("CLAWD_REPRO_SECS").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(150);
        let vid = std::env::var("CLAWD_REPRO_VID").unwrap_or_else(|_| "X4VbdwhkE10".to_string());

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let resolver = YtdlResolver::new(rt.handle().clone()).unwrap();
        let track = rt.block_on(async { resolver.resolve(&vid, true).await }).unwrap();
        println!("[repro] dash url = {} (is_live={})", track.url, track.is_live);

        gst::init().unwrap();
        let playbin = gst::ElementFactory::make("playbin").name("repro").build().unwrap();
        playbin.set_property_from_str("flags", "audio+soft-volume");
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        sink.set_property("sync", true); // honour timestamps → render at 1x real-time
        playbin.set_property("audio-sink", &sink);
        playbin.set_property("uri", &track.url);

        let ret = playbin.set_state(gst::State::Paused).unwrap();
        let live = ret == gst::StateChangeSuccess::NoPreroll;
        if live { let _ = playbin.set_state(gst::State::Playing); }

        let bus = playbin.bus().unwrap();
        let t0 = Instant::now();
        let mut fill_done = live;
        let mut prerolled = false;
        let mut low_buffer = false;
        let mut last_pos_ns: i64 = -1;
        let mut last_advance = Instant::now();
        let mut stalls = 0u32;
        let mut in_stall = false;
        let mut last_buf = 100i32;
        let filter = [gst::MessageType::Error, gst::MessageType::Eos,
            gst::MessageType::Buffering, gst::MessageType::StateChanged, gst::MessageType::AsyncDone];

        while t0.elapsed() < Duration::from_secs(secs) {
            if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(200), &filter) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        println!("[repro] {:>5.1}s ERROR {}", t0.elapsed().as_secs_f64(), e.error());
                    }
                    gst::MessageView::Eos(_) => {
                        println!("[repro] {:>5.1}s EOS (window end → would re-resolve)", t0.elapsed().as_secs_f64());
                        break;
                    }
                    gst::MessageView::AsyncDone(_) => {
                        prerolled = true;
                        if !fill_done && !low_buffer {
                            fill_done = true;
                            let _ = playbin.set_state(gst::State::Playing);
                            println!("[repro] {:>5.1}s fill_done → PLAYING", t0.elapsed().as_secs_f64());
                        }
                    }
                    gst::MessageView::Buffering(b) => {
                        let p = b.percent();
                        if p != last_buf {
                            println!("[repro] {:>5.1}s BUFFERING {}%{}", t0.elapsed().as_secs_f64(), p,
                                if fill_done { "  (post-fill: IGNORED by backend)" } else { "  (initial fill)" });
                            last_buf = p;
                        }
                        // Mirror real backend handle_buffering: only managed pre-fill.
                        let managed = !fill_done;
                        if managed {
                            low_buffer = p < 100;
                            if p < 100 { let _ = playbin.set_state(gst::State::Paused); }
                            else if prerolled { fill_done = true; let _ = playbin.set_state(gst::State::Playing); }
                        }
                    }
                    _ => {}
                }
            }
            // Position sampling + stall detection.
            if let Some(pos) = playbin.query_position::<gst::ClockTime>() {
                let ns = pos.nseconds() as i64;
                if ns > last_pos_ns {
                    if in_stall {
                        println!("[repro] {:>5.1}s ▶ RESUMED after {:.1}s stall (pos={:.1}s)",
                            t0.elapsed().as_secs_f64(), last_advance.elapsed().as_secs_f64(), ns as f64/1e9);
                        in_stall = false;
                    }
                    last_pos_ns = ns;
                    last_advance = Instant::now();
                } else if fill_done && !in_stall && last_advance.elapsed() > Duration::from_secs(2) {
                    stalls += 1;
                    in_stall = true;
                    println!("[repro] {:>5.1}s ⏸ STALL #{} — position frozen at {:.1}s",
                        t0.elapsed().as_secs_f64(), stalls, last_pos_ns as f64/1e9);
                }
            }
        }
        let _ = playbin.set_state(gst::State::Null);
        println!("[repro] DONE: {} stalls in {}s, final pos={:.1}s", stalls, secs, last_pos_ns as f64/1e9);
    }

    // Long-run harness for the "jumps to live every ~10min, diverges in between"
    // report. Unlike repro_live_buffering (which breaks at the first EOS), this
    // RE-RESOLVES on EOS exactly like the station does, so a 30-min run catches
    // the recurring jump. It measures two things the short harness can't:
    //   * window cadence  = wall seconds between EOS events (the jump period)
    //   * playback lag    = wall_in_window - sink_position; if this grows, the
    //                       sink is playing < 1x (accumulating latency = the
    //                       audible "diverge"); if it stays flat, the only jump
    //                       is the pure EOS/window seam.
    //   CLAWD_REPRO_SECS=1800 CLAWD_REPRO_VID=4xDzrJKXOOY \
    //     cargo test -p clawdpanel-media repro_live_jump -- --nocapture --ignored
    #[test]
    #[ignore]
    fn repro_live_jump() {
        use crate::resolver::StreamResolver;
        use crate::ytdl::YtdlResolver;

        let secs: u64 = std::env::var("CLAWD_REPRO_SECS").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(1800);
        let vid = std::env::var("CLAWD_REPRO_VID").unwrap_or_else(|_| "4xDzrJKXOOY".to_string());

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let resolver = YtdlResolver::new(rt.handle().clone()).unwrap();
        let resolve = |force: bool| -> Option<String> {
            rt.block_on(async { resolver.resolve(&vid, force).await }).ok().map(|t| t.url)
        };

        gst::init().unwrap();
        let playbin = gst::ElementFactory::make("playbin").name("repro-jump").build().unwrap();
        playbin.set_property_from_str("flags", "audio+soft-volume");
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        sink.set_property("sync", true); // honour timestamps → render at 1x real-time
        playbin.set_property("audio-sink", &sink);

        let depth = std::env::var("CLAWD_PREFETCH_DEPTH").unwrap_or_else(|_| "40(default)".into());
        let ttl = std::env::var("CLAWD_SEG_TTL_S").unwrap_or_else(|_| "180(default)".into());
        println!("[jump] vid={vid} run={secs}s  prefetch_depth={depth} seg_ttl={ttl}");

        let start_window = |pb: &gst::Element, url: &str| {
            let _ = pb.set_state(gst::State::Null);
            pb.set_property("uri", url);
            let ret = pb.set_state(gst::State::Paused);
            // static /dash prerolls (not NoPreroll); promoted to PLAYING on AsyncDone.
            if matches!(ret, Ok(gst::StateChangeSuccess::NoPreroll)) {
                let _ = pb.set_state(gst::State::Playing);
            }
        };

        let url = match resolve(true) {
            Some(u) => u,
            None => { println!("[jump] initial resolve FAILED — bad/expired live id?"); return; }
        };
        println!("[jump] window #1 url = {url}");
        start_window(&playbin, &url);

        let bus = playbin.bus().unwrap();
        let t0 = Instant::now();
        let mut window = 1u32;
        let mut window_start = Instant::now();
        let mut last_pos_ns: i64 = -1;
        let mut last_advance = Instant::now();
        let mut stalls = 0u32;
        let mut in_stall = false;
        let mut last_tick = Instant::now();
        let filter = [gst::MessageType::Error, gst::MessageType::Eos, gst::MessageType::AsyncDone];

        while t0.elapsed() < Duration::from_secs(secs) {
            if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(200), &filter) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        println!("[jump] {:>6.1}s ERROR (win#{window}) {} — re-resolving",
                            t0.elapsed().as_secs_f64(), e.error());
                        if let Some(u) = resolve(true) { window += 1; window_start = Instant::now();
                            last_pos_ns = -1; in_stall = false; start_window(&playbin, &u); }
                    }
                    gst::MessageView::AsyncDone(_) => {
                        let _ = playbin.set_state(gst::State::Playing);
                    }
                    gst::MessageView::Eos(_) => {
                        let played = window_start.elapsed().as_secs_f64();
                        println!("[jump] {:>6.1}s ◆ EOS win#{window} — window played {:.1}s (sink pos={:.1}s) → JUMP/re-resolve",
                            t0.elapsed().as_secs_f64(), played, last_pos_ns as f64/1e9);
                        match resolve(true) {
                            Some(u) => {
                                window += 1; window_start = Instant::now();
                                last_pos_ns = -1; last_advance = Instant::now(); in_stall = false;
                                println!("[jump] {:>6.1}s   ↳ window #{window} url = {u}", t0.elapsed().as_secs_f64());
                                start_window(&playbin, &u);
                            }
                            None => { println!("[jump] re-resolve FAILED — stopping"); break; }
                        }
                    }
                    _ => {}
                }
            }

            if let Some(pos) = playbin.query_position::<gst::ClockTime>() {
                let ns = pos.nseconds() as i64;
                if ns > last_pos_ns {
                    if in_stall {
                        println!("[jump] {:>6.1}s ▶ resumed after {:.1}s stall",
                            t0.elapsed().as_secs_f64(), last_advance.elapsed().as_secs_f64());
                        in_stall = false;
                    }
                    last_pos_ns = ns;
                    last_advance = Instant::now();
                } else if !in_stall && last_advance.elapsed() > Duration::from_secs(2) && last_pos_ns > 0 {
                    stalls += 1; in_stall = true;
                    println!("[jump] {:>6.1}s ⏸ STALL #{stalls} (win#{window}) pos frozen at {:.1}s",
                        t0.elapsed().as_secs_f64(), last_pos_ns as f64/1e9);
                }
            }

            // 15s heartbeat: wall-in-window vs sink position. Lag that grows ⇒ <1x ⇒ diverge.
            if last_tick.elapsed() >= Duration::from_secs(15) {
                last_tick = Instant::now();
                let in_win = window_start.elapsed().as_secs_f64();
                let pos = last_pos_ns as f64 / 1e9;
                println!("[jump] {:>6.1}s   win#{window} in_window={:.0}s sink_pos={:.1}s lag={:.1}s stalls={}",
                    t0.elapsed().as_secs_f64(), in_win, pos, (in_win - pos).max(0.0), stalls);
            }
        }
        let _ = playbin.set_state(gst::State::Null);
        println!("[jump] DONE: {window} windows, {stalls} stalls in {secs}s");
    }

    /// Resolver decorator: forwards to the real YtdlResolver but counts/logs every
    /// `resolve` call and its `force` flag. `do_retry` (the controller's
    /// retry-once) is the ONLY caller that passes force=true, so force-count == the
    /// number of app-layer pipeline restarts. The proxy's own internal re-resolve
    /// (`reresolve_dash`) bypasses this seam, so it is NOT counted here.
    struct CountingResolver {
        inner: Arc<crate::ytdl::YtdlResolver>,
        t0: Instant,
        total: std::sync::atomic::AtomicU64,
        forced: std::sync::atomic::AtomicU64,
    }
    #[async_trait::async_trait]
    impl crate::resolver::StreamResolver for CountingResolver {
        async fn resolve(&self, video_id: &str, force: bool) -> Result<crate::event::ResolvedTrack> {
            self.total.fetch_add(1, Ordering::SeqCst);
            if force {
                self.forced.fetch_add(1, Ordering::SeqCst);
            }
            let r = self.inner.resolve(video_id, force).await;
            println!("[ctrl] {:>6.1}s RESOLVE force={force} -> {}",
                self.t0.elapsed().as_secs_f64(),
                match &r { Ok(t) => format!("ok live={} {}", t.is_live, t.url), Err(e) => format!("ERR {e}") });
            r
        }
    }

    // Controller-path harness for "is the ~N-min jump an app-layer play() restart
    // or the inherent proxy re-resolve skip?". Drives the REAL stack — YtdlResolver
    // (wrapped to count force=true resolves) → Controller (retry-once) → LinuxPlayer
    // (bus thread + stall watchdog + autoaudiosink) — exactly as the app does, and
    // re-plays on terminal Error/Ended like the single-track station. Decision:
    //   * forced resolves > 0 at a periodic cadence ⇒ APP-LAYER RESTART (controller
    //     retry on a backend bus error / watchdog). Fix lives in backend/controller.
    //   * forced resolves ≈ 0, only continuous Playing ⇒ INHERENT PROXY SKIP (the
    //     proxy's internal manifest re-resolve hands dashdemux a fresh CDN window;
    //     the skip never reaches the app). Fix lives in the proxy.
    // Position resets (playhead drops toward 0) corroborate a restart.
    //   CLAWD_REPRO_SECS=1800 CLAWD_REPRO_VID=4xDzrJKXOOY \
    //     cargo test -p clawdpanel-media repro_controller_jump -- --nocapture --ignored
    #[test]
    #[ignore]
    fn repro_controller_jump() {
        use crate::controller::Controller;
        use crate::event::{Event, State};
        use crate::station::TrackController;
        use std::sync::atomic::AtomicU64;

        let secs: u64 = std::env::var("CLAWD_REPRO_SECS").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(1800);
        let vid = std::env::var("CLAWD_REPRO_VID").unwrap_or_else(|_| "4xDzrJKXOOY".to_string());

        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let t0 = Instant::now();
        let resolver = Arc::new(CountingResolver {
            inner: crate::ytdl::YtdlResolver::new(rt.handle().clone()).unwrap(),
            t0,
            total: AtomicU64::new(0),
            forced: AtomicU64::new(0),
        });
        let ctrl = Controller::new(resolver.clone(), rt.handle().clone()).unwrap();

        // Stats touched only by the emit closure (serialized by the dispatcher).
        struct Stats { last_pos: f64, max_pos: f64, resets: u32, errors: u32, ended: u32, replays: u32, playing_tx: u32, loadings: u32 }
        let stats = Arc::new(Mutex::new(Stats { last_pos: 0.0, max_pos: 0.0, resets: 0, errors: 0, ended: 0, replays: 0, playing_tx: 0, loadings: 0 }));

        let tc: Arc<dyn TrackController> = ctrl.track_controller();
        let depth = std::env::var("CLAWD_PREFETCH_DEPTH").unwrap_or_else(|_| "40(default)".into());
        println!("[ctrl] vid={vid} run={secs}s prefetch_depth={depth} — driving REAL Controller+LinuxPlayer");

        let st_emit = stats.clone();
        let tc_emit = tc.clone();
        let rt_emit = rt.handle().clone();
        let vid_emit = vid.clone();
        let emit: crate::event::EmitFn = Arc::new(move |ev: Event| {
            let now = t0.elapsed().as_secs_f64();
            let mut s = st_emit.lock();
            if ev.progress {
                // Throttled playhead tick. A drop toward 0 = a fresh pipeline = restart.
                if ev.position + 5.0 < s.last_pos {
                    s.resets += 1;
                    println!("[ctrl] {now:>6.1}s ⟲ POSITION RESET {:.1}s -> {:.1}s (restart #{})", s.last_pos, ev.position, s.resets);
                }
                s.last_pos = ev.position;
                if ev.position > s.max_pos { s.max_pos = ev.position; }
                return;
            }
            // A state transition (not a progress tick).
            match ev.state {
                State::Loading => { s.loadings += 1; }
                State::Playing => { s.playing_tx += 1; }
                State::Error => {
                    s.errors += 1;
                    println!("[ctrl] {now:>6.1}s ✖ ERROR (outward) #{}: {}", s.errors, ev.err);
                }
                State::Ended => { s.ended += 1; }
                _ => {}
            }
            println!("[ctrl] {now:>6.1}s STATE {} vid={} pos={:.1} dur={:.1} err={}",
                ev.state.as_str(), ev.video_id, ev.position, ev.duration, ev.err);
            // Single-track station behavior: a terminal Error or natural Ended re-plays.
            if matches!(ev.state, State::Error | State::Ended) {
                s.replays += 1;
                println!("[ctrl] {now:>6.1}s ↻ re-play (station) #{}", s.replays);
                drop(s);
                let tc2 = tc_emit.clone();
                let v = vid_emit.clone();
                rt_emit.spawn(async move { let _ = tc2.play_video(&v).await; });
            }
        });
        ctrl.set_emit(emit);

        rt.block_on(tc.play_video(&vid)).unwrap();

        let mut last_tick = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_millis(250));
            if last_tick.elapsed() >= Duration::from_secs(30) {
                last_tick = Instant::now();
                let s = stats.lock();
                println!("[ctrl] {:>6.1}s ── pos={:.1}s max={:.1}s | resets={} errors={} ended={} replays={} | resolves total={} forced={}",
                    t0.elapsed().as_secs_f64(), s.last_pos, s.max_pos, s.resets, s.errors, s.ended, s.replays,
                    resolver.total.load(Ordering::SeqCst), resolver.forced.load(Ordering::SeqCst));
            }
        }

        let _ = ctrl.close();
        let s = stats.lock();
        let forced = resolver.forced.load(Ordering::SeqCst);
        println!("[ctrl] DONE in {secs}s: forced_resolves(app-layer restarts)={forced} position_resets={} outward_errors={} ended={} replays={} | total_resolves={}",
            s.resets, s.errors, s.ended, s.replays, resolver.total.load(Ordering::SeqCst));
        println!("[ctrl] VERDICT: {}", if forced == 0 && s.resets == 0 {
            "no app-layer restart — jump is the INHERENT PROXY re-resolve skip (fix in proxy)"
        } else {
            "app-layer RESTART observed (controller retry / position reset) — fix in backend/controller"
        });
    }

    // The stall watchdog's gating (the part that turns a wedged broken-HLS
    // pipeline into a terminal error so the controller/station give up instead
    // of letting gst hot-loop the `streams != NULL` assertion forever).
    #[test]
    fn stall_watchdog_gating() {
        let over = STALL_TIMEOUT + Duration::from_secs(1);
        let under = Duration::from_secs(0);
        // Wedged: filled, never progressed, not yet fired, past the timeout.
        assert!(stall_should_fire(true, false, false, over));
        // Still within the timeout → hold (give a slow-but-real start time).
        assert!(!stall_should_fire(true, false, false, under));
        // During the managed VOD/DASH buffer fill (fill not done) → never fire.
        assert!(!stall_should_fire(false, false, false, over));
        // Real playback progress was seen → never fire.
        assert!(!stall_should_fire(true, true, false, over));
        // Fires at most once per track.
        assert!(!stall_should_fire(true, false, true, over));
    }
}
