# Media slice → Rust/Slint (audio + radio + station + reveal)

1:1 rewrite design. Preserves behavior + looks of today's Wails/Go media subsystem.

## Scope

Existing files/symbols this slice covers:

- `internal/audio/audio.go` — `Player` iface, `Event`, `State` enum (idle/loading/playing/paused/error/ended).
- `internal/audio/controller.go` — `Controller`, `StreamResolver` iface, `ResolvedTrack`, retry-once, progress throttle, event spine.
- `internal/audio/audio_darwin.go` + `.m` — AVPlayer via CGo/ObjC (KVO status+rate, EOS notif).
- `internal/audio/audio_linux.go` — GStreamer `playbin` via CGo (preroll/buffer dance, bus loop, `playbackPhase`/`trackState`).
- `internal/audio/audio_windows.go` + `.c` — PowerShell+WinRT `MediaPlayer` subprocess; `.c` obsolete.
- `internal/audio/audio_stub.go` — `ErrUnsupported`.
- `internal/radio/radio.go` — `Resolver` (youtube extraction), local HTTP proxy (`ServeHTTP`), VOD byte-cache (`videoCache`, parallel segment dl), live DASH staticizer (`staticizeLiveMPD*`, `serveLiveManifest`), `ExpandPlaylist`, `pickAudioFormat`, `probeSize`/`parseRange`.
- `internal/station/parse.go` — `ParseItem`, `HasMultipleTracks`, URL/ID regexes.
- `internal/station/player.go` — `StationPlayer` (queue, shuffle, auto-advance/loop, skip-on-fail, epoch cancel, Next/Prev/Pause/Stop).
- `internal/reveal/reveal.go` — `Controller` auto-hide state machine, `WindowOps` seam, slide anim, cursor poll, grace collapse, precedence rules, click-through.
- Wiring: `app.go` `initAudio`/`teardownAudio`/`getStation`, `radioResolverAdapter`, `Radio*` bindings, reveal setup (`revealCtrl.Run`).
- Frontend it feeds: `frontend/src/main.js:380-739` (radio segment UI, `radio:state` handler, seek timeline).
- Config shapes (owned by config slice): `config.StationItem{Kind,ID,Raw}`, `StationConfig{Name,Items,Shuffle}`, `RadioVolume`, `Features.Radio`, `ActiveStation`.

## Current behavior

### audio — what it controls
NOT system volume. It is an **in-app single-track media player** that streams an HTTP/HLS/DASH URL. `SetVolume(0..1)` sets the player's own output volume (playbin `volume` / `AVPlayer.volume` / `MediaPlayer.Volume`). UI shows 0–200% but JS sends `vol/100`, backend clamps to 1.0 → anything >100% is clamped (effective no-op above 100%).

`Player` iface: `Play(url)`, `Resume`, `Pause`, `Stop`, `SetVolume`, `Seek(sec)`, `Close`. Optional `SetLiveHint(bool)` via type-assert. `Resume` ≠ `Play` (resume continues paused position; play loads fresh from 0).

`Event{State, VideoID, Err, StationIdx, Position, Duration, Progress}`. States: idle/loading/playing/paused/error/**ended** (EOS — VOD/static-DASH only; livestreams never emit it). `Progress=true` = throttled playhead tick (same state, advanced pos), not a transition.

**Controller** wraps a `Player` + a `StreamResolver`:
- `PlayVideo(videoID)`: emit loading → resolve URL → `SetLiveHint` → `player.Play`. On resolve/play err → sticky `StateError`.
- **Retry-once**: first `StateError` from player → async re-resolve with `forceRefresh=true`, replay. `retried` flag; second error propagates. (YouTube URLs expire ~6h.)
- **Progress throttle**: `progressInterval=400ms`. Player polls 100ms for EOS detection; controller down-samples ticks to ~2.5/s as `Progress` events. Live → `Position/Duration` zeroed (livestream NaturalDuration is a sentinel → bar shows LIVE).
- **Sticky error**: while `StateError`, a following `StateIdle` is swallowed (station's stop-on-error must not repaint to blank).
- Duplicate-state suppression; `StateLoading` suppressed while already `StatePlaying`.

**Threading contract (load-bearing).** Controller `emit` runs on a dedicated dispatcher goroutine (`events chan Event`, cap 64, drop-on-full). Reason: emits happen under `c.mu`; downstream (station) reacts by calling back into Controller methods that take `c.mu` → synchronous emit self-deadlocks. Same pattern in `LinuxPlayer` (player never emits on caller stack). macOS dispatches each KVO callback on a fresh goroutine for the identical reason (AVFoundation delivers KVO synchronously on the mutating thread = Wails main thread).

### Per-OS audio
- **darwin** (`darwin && cgo`): `AVPlayer` + ObjC observer. KVO `status` (ReadyToPlay→playing, Failed→error), KVO `rate` (0→paused, else playing), `AVPlayerItemDidPlayToEndTime`→ended. `endedFired` swallows the spurious rate==0 after EOS. `Seek` is a TODO no-op (timeline read-only on mac).
- **linux** (`linux && cgo`): GStreamer classic `playbin` (not playbin3 — needs `GstPlayFlags`), audio-only flags `AUDIO|SOFT_VOLUME`. **Preroll dance**: set PAUSED first; promote to PLAYING only after `ASYNC_DONE` (segment mapping settled) AND initial buffer fill 100% (`handleBuffering` holds PAUSED <100% during initial fill only). Going straight to PLAYING races the DASH segment event → sink schedules buffers hours ahead → "position advances, pure silence". Live sources (`NO_PREROLL` or live-hint) skip all buffer management → straight to PLAYING (pausing a live stream starves the demuxer = stutter). `playbackPhase`{idle,prerolling,playing,paused} gates which pipeline state-changes reach UI. `confirmFromPosition` reports playing from an advancing position when the async PLAYING transition never settles (live). Bus polled 100ms. `Seek` TODO no-op.
- **windows**: spawns `powershell -NoProfile` running a WinRT `MediaPlayer` script; commands over stdin (`play/resume/pause/stop/seek/volume/state/exit`), state over stdout (`STATE:<st>|POS:..|DUR:..`). 100ms `state` poll. **EOS detection**: PS can't subscribe to WinRT events, so at `pos>=dur-0.5 && state==Paused` it emits `STATE:Ended` once (`$endedReported`). Invariant-culture float formatting (avoid locale comma). `Seek` works here.
- **stub**: `New` → `ErrUnsupported`.

### radio — resolution + proxy
`Resolver` (uses `github.com/kkdai/youtube/v2`). `Resolve(videoID, forceRefresh)`:
1. `GetVideoContext`. **Live + DASH available** → register upstream DASH URL, return local proxy `http://127.0.0.1:<port>/dash?id=…`, **`IsLive=false`** (static DASH hits EOS → station re-resolves a fresh window). **HLS only** → return HLS URL `IsLive=true` (fallback). **VOD** → `pickAudioFormat` (audio/mp4 itag 140 AAC preferred), decipher stream URL, then route via proxy `/stream?id=…`.
- Caches: track URLs `cacheTTL=1h`, playlists `playlistCacheTTL=6h`.

**Local HTTP proxy** (`ServeHTTP` on `127.0.0.1:0`):
- `/stream` (VOD): on first hit, kick a background full-track download into RAM via **8 parallel ranged GETs** (`downloadInto`, `dlStartDelay=1500ms` head start, `maxCachedTracks=3` LRU). Until complete → `passthrough` single range to googlevideo. Once complete → serve every range (incl. seeks) from immutable RAM buffer. Bypasses 403s, makes seeks instant. Never blocks on not-yet-downloaded bytes (MediaPlayer probes arbitrary offsets → blocking = hang).
- `/dash` (live): `serveLiveManifest` fetches upstream dynamic MPD, runs `staticizeLiveMPD`, caches rewritten body 1.5s (dashdemux refetches every ~2s, each ~1MB). Upstream 6h expiry → re-resolve once + retry.

**DASH staticizer invariants (the "playing but silent" fix — must port exactly):**
- Reject non-`type="dynamic"`.
- Drop video AdaptationSets (~90% of doc; audio-only bar).
- `dynamic`→`static`; strip `minimumUpdatePeriod/timeShiftBufferDepth/availabilityStartTime/mpdRequestTime/mpdResponseTime/earliestMediaSequence`; strip `Period@start`.
- **Assert `timescale="1000"`** and **reject `r=` repeat-compacted timelines** — fail loudly; mis-shifting PTO reproduces the silence bug.
- Trim each SegmentList to freshest `maxStaticWindowMs=30min` (overridable `CLAWDPANEL_LIVE_WINDOW_MS`).
- **Shift `presentationTimeOffset += droppedMs`** (sum of dropped `<S d>` values, not count×nominal) and **`startNumber += droppedCount`**. PTO maps fMP4 `tfdt` (full live media time, hundreds of h in) back to presentation 0; without the shift the sink schedules audio that far ahead → silence.
- Set `mediaPresentationDuration = PT<keptMs/1000>S`.
- Test facts: `staticize_test.go` (PTO=7518965733, segMs=5000; 20s window over 12×5s → keep last 4, drop 8, PTO+=8×5000, start+=8).

`ExpandPlaylist` → ordered video IDs. `parseRange` honors single `bytes=start-end` incl. suffix `-N`. `probeSize` = 1-byte ranged GET, total from `Content-Range/<total>`.

### station — queue
`ParseItem` classifies input → `StationItem`: `list=` wins over `v=` (playlist precedence); youtu.be/shorts/embed/v/live + `?v=`; bare 11-char=video, 13+=playlist. `HasMultipleTracks` (config-only, no network): ≥2 items OR single playlist (re-parses `Raw` to upgrade stale "video" kind).

`StationPlayer` sits ABOVE the single-track `Controller`:
- `Play(idx)`: if same active station + queue exists → `ctrl.Resume()` (keep exact place). Else fresh start: bump `epoch`, cancel prior expansion, emit loading, `go buildAndStart`.
- `buildAndStart`: flatten items (expand playlists via resolver), re-parse `Raw` on the fly. Sequential → append incrementally + start at queue[0] the moment first track known. Shuffle → expand all, start at random index. Empty → `StateError "station has no playable items"`.
- `OnAudioEvent`: `Progress` forwarded straight (stamped `StationIdx`), skips advance/skip machine. `StatePlaying`→reset failStreak+unpause. `StatePaused`→paused. `StateEnded`→advance+loop. `StateError`→skip to next track; `failStreak` up to `failLimitLocked` (=min(queue,25),≥1) then give up → `Stop` + `StateError "station unavailable"`. Skips suppress the raw error (no `[ERR]` flicker).
- `advanceLocked`: shuffle→random ≠ current; else seq wrap-to-0 (loop). `retreatLocked`: seq back, wrap to end.
- `epoch` (uint64) invalidates stale playlist expansions + in-flight `playTrack` goroutines on every (re)start/stop.
- `Next/Prev/Pause/SetShuffle/SetVolume/Stop`. SetShuffle = pure mode toggle (never starts/jumps playback).
- Events forwarded to frontend stamped with `StationIdx` so UI filters to active station (videoID changes per track).

### reveal — auto-hide state machine
Owns the bar's slide/hide. Talks to OS only via `WindowOps` seam (window rect/move/cliptop/show/hide/click-through, cursor pos, fullscreen-active, autohide-supported) — fully fake-able with fake cursor+clock. App owns only the poll loop calling `Tick`.

- Constants: `slide=200ms`, `frame=16ms` (~60fps), `collapseDelay=200ms` grace, `poll=80ms` (WebView2 mouseleave unreliable → poll OS cursor).
- State: `configured, mon (MonitorInfo), barHeight, pinned, userClickThrough, expanded, editorOpen, leftBarAt, animGen (atomic)`.
- `Configure(mon,barHeight,pinned,clickThrough)` snapshot + re-apply click-through. `Init` = no-anim initial paint (pinned⇒expanded; else cursor; collapsed⇒move off-screen+clip+hide so nothing flashes).
- **Precedence (`Tick`)**: editor-open OR pinned ⇒ force expanded; fullscreen-active ⇒ force collapsed (tray stays); else follow cursor, collapse after grace once cursor left.
- `cursorOverBar`: monitor full width × [top edge .. bar bottom]; bottom-dock variant; menu-bar slice above bar (macOS `WorkTopOffset`) counts as on-bar.
- `SetExpanded`: slides the **OS window itself** (dark bg travels with bar). `animGen.Add(1)` supersedes in-flight slide; `Show()` before sliding in. `animateY`: ease-out cubic `1-(1-t)³`, re-clips top each frame to mask multi-monitor spill above `mon.Top` (+1px DPI slop), `Hide()` at off-screen target.
- `ApplyClickThrough`: click-through = user pref OR (autohide-supported && !pinned && !expanded) so a hidden bar can't eat clicks.
- Dock edge top/bottom from `MonitorInfo.DockEdge`; `onScreenY/offScreenY` differ accordingly.

### App wiring & frontend bridge
- `initAudio` (only if `Features.Radio`): build `Controller(radioResolverAdapter, emit)`; emit routes each event through `station.OnAudioEvent` (or directly `Event.Emit("radio:state", ev)` if station nil). `station.New(ctrl, resolver, emit→"radio:state")`. `teardownAudio` stops + frees (Radio toggle = resource up/down), emits idle.
- Bindings (frontend-callable): `RadioPlayStation, RadioPause, RadioNext, RadioPrev, RadioStationHasTracks, RadioSetShuffle, RadioSeek, RadioSetVolume, SetActiveStation, ParseStationItem`.
- Event `radio:state` (JSON of `audio.Event`). Frontend (`main.js`): filters by `stationIdx`; `progress`→`updateTimeline`; `loading`→load, `playing`→on (marquee `NOW PLAYING <name> · …`), `paused`/`idle`→off, `error`→err, `ended`→ignored (transient; keep "playing"). Status drives play/pause icon + color classes. Seek timeline: click title→inline scrubber; `dur<=0`⇒inert "LIVE"; drag→`RadioSeek(frac*dur)`; auto-collapse 3s after cursor leaves. Volume cycles −10% wrapping 0↔200.

## Rust/Slint design

One cargo workspace, logical crates: `media-audio`, `media-radio`, `media-station`, `media-reveal`. Async = tokio (multi-thread runtime). Shared event type `media_audio::Event` (serde-derive for Slint bridging). All depend on the `config` slice's `StationConfig`/`StationItem` (cross-slice dep — coordinate).

### Threading model
Replace Go goroutine+channel event spine with **tokio `mpsc::UnboundedSender<Event>`** (or bounded 64 + `try_send` drop-on-full to mirror the cap). The deadlock the Go code dodges (sync emit under a held lock re-entering the same lock) **does not exist** if the controller holds a `std::sync::Mutex` only for short critical sections and emits via channel send (non-reentrant). Keep the discipline anyway: never run user callbacks while holding the state mutex. Use `parking_lot::Mutex` for the small state structs; spawn resolve/retry/download on tokio tasks.

### media-audio
`trait Player: Send { fn play(&self,url:&str)->Result; fn resume; fn pause; fn stop; fn set_volume(f32); fn seek(f64); fn set_live_hint(bool); fn close; }` — mirrors Go iface (fold optional `SetLiveHint` into the trait with a default no-op).

`Controller` (port of controller.go 1:1): holds `Box<dyn Player>`, `Arc<dyn StreamResolver>`, `mpsc::Sender<Event>`, state (`parking_lot::Mutex<CtrlState>`). Reimplement retry-once, sticky-error, progress throttle (`Instant`/`Duration`), live zeroing — pure logic, direct port.

`trait StreamResolver { async fn resolve(&self, video_id:&str, force:bool)->Result<ResolvedTrack>; }` (async-trait or RPITIT).

**Per-OS backends — recommended: keep the native-per-OS split (Strategy B).** Preserves today's behavior 1:1 and drops the fragile PowerShell subprocess:
- **Linux**: `gstreamer-rs` (`gstreamer`, `gstreamer-app` if needed). Direct port of `audio_linux.go` — same `playbin`, `flags=AUDIO|SOFT_VOLUME`, same PAUSED-preroll→ASYNC_DONE→buffer-fill→PLAYING promotion, same `playbackPhase`/`trackState`, bus watch via `bus.add_watch` or a polling task (keep 100ms poll for `confirmFromPosition` parity). This is the **highest-fidelity** port because the DASH preroll invariants are already expressed in GStreamer terms.
- **macOS**: `objc2` + `objc2-av-foundation` (or `objc2-avf-audio`). Port `audio_darwin.m` — `AVPlayer`, KVO on `status`/`rate`, `AVPlayerItemDidPlayToEndTime` observer, `endedFired` guard. KVO/notification callbacks → channel send (objc2 closures). Wire `seek` here (the Go TODO) for free.
- **Windows**: `windows` crate → `Windows.Media.Playback.MediaPlayer` + `Windows.Media.Core.MediaSource` **directly** (no subprocess). Subscribe to `MediaEnded`/`MediaFailed`/`PlaybackSession.PlaybackStateChanged` events (now possible without the PS poll hack), poll `Position`/`NaturalDuration` for progress. Removes the PowerShell process, stdin/stdout parsing, locale formatting — net simplification with identical behavior.
- **stub**: `cfg`-gated `Err(Unsupported)`.

*Strategy A alternative — GStreamer everywhere* (gstreamer-rs on all 3 OSes): one backend, max reuse, and the static-DASH path is GStreamer-validated on every platform. Cost: heavy native GStreamer runtime bundled into the mac/Windows app (the current build avoids it). Recommend B for fidelity; fall back to A if cross-platform native-player upkeep proves costly. Note: today's `/dash` proxy serves static DASH to **all** platforms — under B, confirm `AVPlayer`(mac)/`MediaPlayer`(win) accept the static MPD or keep HLS fallback for them (current open question, not introduced by the rewrite).

### media-radio
Pure, portable (no platform splits). 
- youtube extraction: **`rusty_ytdl`** (closest to kkdai/youtube — video info, formats, stream URL decipher, playlist expansion, manifest URLs). **Biggest fidelity risk** (signature cipher churn). Port `pickAudioFormat` (audio/mp4 itag 140 first). 
- Proxy: **`axum`** (or raw `hyper`) bound to `127.0.0.1:0`, served on a tokio task. Port `ServeHTTP`/`/stream`/`/dash`. `reqwest` for upstream fetches + ranged segment GETs.
- VOD byte-cache: `videoCache` = `Arc<Mutex<...>>` with `tokio::task::JoinSet` for the 8 parallel ranged downloads; immutable `Bytes` snapshot once complete; LRU cap 3 via `VecDeque` insertion order + `CancellationToken` per entry (port `cancel`). Range serving from `Bytes` slice (`http-range`/manual `parseRange` port).
- DASH staticizer: **direct port** with `regex` + `bytes` — string transforms, no XML DOM needed. **Port every invariant verbatim** incl. `timescale=1000` assert, `r=` rejection, PTO/startNumber shift = dropped-segment-duration sum. Carry over `staticize_test.go` + the captured `testdata/live-dynamic.mpd` fixture as Rust tests (this is the regression guard for the silence bug). Keep `CLAWDPANEL_LIVE_WINDOW_MS` env override.

### media-station
Pure logic, straightforward port of player.go + parse.go.
- `ParseItem`/`HasMultipleTracks`: `regex` (same patterns), returns `config::StationItem`.
- `StationPlayer`: `parking_lot::Mutex<State>` (stations, activeIdx, queue, cur, shuffle, paused, failStreak, epoch). `epoch: u64` + `CancellationToken` for `cancelExpand`. `playTrack`/`buildAndStart` as tokio tasks guarded by epoch check. Shuffle via `rand` (`rand::rng().random_range`). `trait TrackController`/`trait PlaylistExpander` test seams = traits (keep unit tests). Direct 1:1 of advance/skip/give-up machine.

### media-reveal + Slint binding
- `trait WindowOps` (same methods) — production impl drives the Slint window.
- State machine (`Controller`, `Tick`, `SetExpanded`, `animateY`, precedence) = **direct port**; `AtomicU64` animGen, `Instant` clock, ease-out cubic. Animation frames via `slint::Timer` (16ms repeating) or a tokio interval that posts to the UI thread; cursor poll via a 80ms `slint::Timer`.
- **Window ops in Slint**: `slint::Window::set_position(LogicalPosition)` for the slide (move the real OS window — matches "dark bg travels with the bar"). `window.hide()/show()`. Always-on-top + frameless via `slint::WindowAttributes`/backend (winit). Per-OS bits (`SetClickThrough`, `IsFullScreenActive`, global cursor pos, multi-monitor `MonitorInfo`) belong to the **platform slice** — reveal calls them through `WindowOps` exactly as today. 
- Slint UI bridge for the whole media slice: a Slint **global** `RadioBridge` with properties (`status: enum`, `station_name`, `marquee_active`, `shuffle_on`, `track_nav_active`, `pos`, `dur`, `live`, `volume_pct`) and callbacks (`play_pause`, `next_station(dir)`, `track_next`, `track_prev`, `toggle_shuffle`, `seek(frac)`, `cycle_volume`, `set_volume`). Rust side: `media_audio::Event`s arrive on the channel → a consumer task maps state→bridge props via `slint::Weak::upgrade_in_event_loop` (replaces `Events.On('radio:state')`). Callbacks invoke `StationPlayer`/`Controller`. The marquee + seek-timeline reveal/auto-collapse + LIVE/`--:--` rules port into `.slint` markup + a small Rust view-model (replaces `main.js:380-739`).

## Crate picks

- `gstreamer` / `gstreamer-rs` — Linux audio; 1:1 of the existing playbin preroll/buffer logic (and the only backend the static-DASH path is proven against).
- `objc2` + `objc2-av-foundation` — macOS `AVPlayer`/KVO; direct port of the ObjC observer.
- `windows` — Windows `MediaPlayer`/`MediaSource` WinRT directly; eliminates the PowerShell subprocess.
- `rusty_ytdl` — YouTube video/playlist/format/stream-URL + manifest extraction (kkdai/youtube replacement). *Highest risk.*
- `axum` + `hyper` + `reqwest` — local proxy server + upstream/ranged HTTP (VOD cache, DASH fetch, passthrough).
- `bytes` + `regex` — DASH staticizer (verbatim string transforms) + URL/ID parsing + range parsing.
- `tokio` — runtime, tasks, `mpsc` event spine, `JoinSet` parallel downloads, `CancellationToken` (`tokio-util`) epoch/eviction cancel.
- `parking_lot` — short-critical-section mutexes for controller/station/reveal state.
- `rand` — shuffle.
- `slint` (+ `slint-build`) — UI, window move/show/hide (reveal), timers (poll/anim), globals/callbacks bridge.
- `serde` — `Event` (de)serialization across the Slint/Rust boundary if needed.

## 1:1 fidelity risks

1. **YouTube extraction (XL risk).** `rusty_ytdl` ≠ kkdai/youtube feature-for-feature; signature cipher + format selection + DASH/HLS manifest extraction drift with YouTube changes. *Mitigation:* wrap behind `StreamResolver`; keep `pickAudioFormat` semantics; integration test against the same probe IDs (`BGXOYfZMR0w`, `4xDzrJKXOOY`); be ready to vendor/patch.
2. **DASH timing / "playing but silent" (XL if mishandled).** The PTO/startNumber shift + preroll-in-PAUSED + buffer-fill promotion are the whole fix. *Mitigation:* port the staticizer **byte-for-byte** with the `timescale=1000` + `r=`-reject asserts; port `staticize_test.go` + fixture as the regression guard; on Linux keep the exact PAUSED→ASYNC_DONE→fill→PLAYING sequence (gstreamer-rs maps 1:1). If Strategy A, this path is validated on all OSes.
3. **Cross-OS native players (Strategy B).** Three native backends to maintain; `AVPlayer`/WinRT must accept the static-DASH proxy URL (current behavior uncertain) — verify, else gate mac/Win to HLS fallback. *Mitigation:* Strategy A (GStreamer everywhere) removes this at the cost of runtime bundling.
4. **Reveal window slide in Slint.** Per-frame top-clip to mask multi-monitor spill (`ClipTop`) may have no direct Slint equivalent; `set_position` at 60fps must be smooth and not lag the dark bg. *Mitigation:* push `ClipTop`/click-through/fullscreen-detect into the platform slice's native window code (as today via `WindowOps`); validate slide smoothness early; fall back to compositor masking.
5. **Global cursor polling.** Slint/winit don't expose global cursor position; needs OS APIs (platform slice). *Mitigation:* `WindowOps::cursor_pos` stays a platform call, same as Go.
6. **Volume >100%.** Today clamped to 1.0 backend-side (200% is cosmetic). Keep identical clamp to avoid "louder" regression expectations.
7. **macOS KVO threading.** objc2 callbacks fire on the AV worker/main thread — keep the "emit via channel, never call back under lock" discipline (port of the goroutine break).

## Effort

- **media-station** — **M.** Pure logic; direct port + existing tests. No external risk. Depends on config slice + media-audio traits.
- **media-radio** — **L–XL.** Staticizer + proxy + byte-cache are M (mechanical port w/ tests); youtube extraction is the XL unknown.
- **media-audio** — **L.** Controller is M (pure port). Per-OS backends: Linux M (gstreamer-rs 1:1), macOS M (objc2), Windows S–M (native WinRT is simpler than today's subprocess). +cross-OS verification.
- **media-reveal** — **M–L.** State machine M (clean port w/ fakes); Slint window slide + clip + cursor poll is the L part, and depends on the **platform slice** (window handle, cursor, fullscreen, MonitorInfo, click-through).

**Ordering / deps:** config slice first (StationConfig/StationItem). Then `media-radio` (Resolver) + `media-audio` traits in parallel → `media-audio` backends → `media-station` (needs Controller+Resolver). `media-reveal` parallel but blocked on the **platform slice** for real `WindowOps`. Slint bridge (globals/callbacks, `radio:state` consumer) last, after audio+station land. Mobile: GStreamer/AVPlayer exist on Android/iOS but reveal (always-on-top HUD + cursor) is desktop-only — mobile media playback feasible, the HUD model is not; treat as out of scope.
