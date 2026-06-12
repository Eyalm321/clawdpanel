//go:build linux && cgo

package audio

/*
#cgo pkg-config: gstreamer-1.0
#include <gst/gst.h>
#include <stdlib.h>

static GstMessageType get_message_type(GstMessage* msg) {
	return GST_MESSAGE_TYPE(msg);
}

static GstObject* get_message_src(GstMessage* msg) {
	return GST_MESSAGE_SRC(msg);
}

static void set_playbin_uri(GstElement* playbin, const char* uri) {
	g_object_set(playbin, "uri", uri, NULL);
}

static void set_playbin_volume(GstElement* playbin, double volume) {
	g_object_set(playbin, "volume", volume, NULL);
}

// Audio-only: GST_PLAY_FLAG_AUDIO (1<<1) | GST_PLAY_FLAG_SOFT_VOLUME (1<<4).
// The bar never renders video, and decoding the stream's video track would
// also require codecs Fedora doesn't ship (H.264).
static void set_playbin_audio_only(GstElement* playbin) {
	g_object_set(playbin, "flags", (1 << 1) | (1 << 4), NULL);
	// NOTE: do not set "buffer-duration" here. On live streams only ~one
	// segment of content exists ahead of the play position, so a large
	// fixed buffering target can never be reached and the pipeline sits
	// in buffering limbo forever.
}
*/
import "C"
import (
	"fmt"
	"log"
	"sync"
	"unsafe"
)

var gstInitOnce sync.Once

func initGStreamer() {
	gstInitOnce.Do(func() {
		// Initialize GStreamer with no arguments
		C.gst_init(nil, nil)
	})
}

// playbackPhase is the player's lifecycle position for the current track.
// All transitions happen under LinuxPlayer.mu:
//
//	Play ──────────────► prerolling ──(ASYNC_DONE + buffer 100%)──► playing
//	  │ (live source)                                                │  ▲
//	  └────────────────────────────────────────────────────────────► │  │
//	Pause: {prerolling,playing} ► paused          Resume: paused ────┘  │
//	Stop:  any ► idle                                                   │
//	(position-confirmation / settled PLAYING state both land here ─────┘
type playbackPhase int

const (
	phaseIdle       playbackPhase = iota // no track, or stopped
	phasePrerolling                      // held in PAUSED until preroll settles and the initial buffer fills
	phasePlaying                         // user wants playback and the pipeline is (moving to) PLAYING
	phasePaused                          // user-requested pause
)

// wantsPlayback reports whether the user intent behind this phase is "audio
// should be (or become) audible".
func (ph playbackPhase) wantsPlayback() bool {
	return ph == phasePrerolling || ph == phasePlaying
}

// trackState is the per-track progress through the preroll dance. Reset by
// Play; guarded by LinuxPlayer.mu.
type trackState struct {
	phase     playbackPhase
	live      bool     // NO_PREROLL source or resolver live-hint: skip buffering management entirely
	prerolled bool     // ASYNC_DONE received: the demuxer's segment mapping is settled
	buffered  bool     // initial buffer at 100% (true until the first <100% report)
	confirmed bool     // StatePlaying already reported to the UI for this track
	lastPos   C.gint64 // last queried position, for advance detection (-1 = none yet)
}

type LinuxPlayer struct {
	mu       sync.Mutex
	emit     func(Event)
	playbin  *C.GstElement
	stopChan chan struct{}
	wg       sync.WaitGroup
	liveHint bool // resolver says the next track is a livestream
	track    trackState

	// events decouples emit from the caller's stack — the player-side half of
	// the event spine's threading contract: the player NEVER emits on a
	// caller's stack. The Controller invokes Play/Pause/... while holding its
	// own mutex, and its event handler takes that same mutex — a synchronous
	// emit from inside those calls deadlocks the controller against itself.
	// A dispatcher goroutine (started in New) delivers events in order
	// instead, matching the async-emit contract of the Windows/macOS players.
	events chan Event
}

// SetLiveHint marks the NEXT Play as a live stream (known from the resolver).
// Live playback must never be held in PAUSED for buffering: starting behind
// the live edge starves the demuxer periodically — audible stutter for the
// stream's whole lifetime.
func (p *LinuxPlayer) SetLiveHint(live bool) {
	p.mu.Lock()
	p.liveHint = live
	p.mu.Unlock()
}

// send queues an event for the dispatcher; drops if the queue is saturated
// (the dispatcher is wedged) rather than blocking a caller.
func (p *LinuxPlayer) send(ev Event) {
	select {
	case p.events <- ev:
	default:
		log.Printf("[audio] event queue full, dropping %s", ev.State)
	}
}

func New(emit func(Event)) (Player, error) {
	initGStreamer()

	// Classic playbin (not playbin3): playbin3 dropped the GstPlayFlags
	// "flags" property, and we rely on it for audio-only playback.
	cPlaybin := C.CString("playbin")
	cPlaybinName := C.CString("radio-playbin")
	defer C.free(unsafe.Pointer(cPlaybin))
	defer C.free(unsafe.Pointer(cPlaybinName))

	playbin := C.gst_element_factory_make(cPlaybin, cPlaybinName)
	if playbin == nil {
		return nil, fmt.Errorf("failed to create GStreamer playbin element (is gstreamer1.0-plugins-base installed?)")
	}

	C.set_playbin_audio_only(playbin)

	p := &LinuxPlayer{
		emit:     emit,
		playbin:  playbin,
		stopChan: make(chan struct{}),
		events:   make(chan Event, 64),
	}

	p.wg.Add(1)
	go p.monitorBus()
	go func() {
		for ev := range p.events {
			p.emit(ev)
		}
	}()

	return p, nil
}

func (p *LinuxPlayer) Play(url string) error {
	p.mu.Lock()

	// Stop previous playback if active
	C.gst_element_set_state(p.playbin, C.GST_STATE_READY)

	cURL := C.CString(url)
	defer C.free(unsafe.Pointer(cURL))

	C.set_playbin_uri(p.playbin, cURL)

	// Preroll in PAUSED first (the gst-launch dance): going straight to
	// PLAYING races the demuxer's initial segment event — DASH segments keep
	// their original live-stream timestamps, and without the segment mapping
	// settled the sink schedules them hours into the future: position
	// advances, pure silence. monitorBus promotes to PLAYING once both
	// ASYNC_DONE and buffering-100% have landed (maybePromote).
	ret := C.gst_element_set_state(p.playbin, C.GST_STATE_PAUSED)
	if ret == C.GST_STATE_CHANGE_FAILURE {
		p.mu.Unlock()
		return fmt.Errorf("failed to set GStreamer state to PAUSED for preroll")
	}
	// True live sources preroll with NO_PREROLL and must never be paused for
	// buffering; they go to PLAYING immediately.
	live := ret == C.GST_STATE_CHANGE_NO_PREROLL || p.liveHint
	if live {
		if C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING) == C.GST_STATE_CHANGE_FAILURE {
			p.mu.Unlock()
			return fmt.Errorf("failed to set GStreamer state to PLAYING")
		}
	}
	p.track = trackState{
		phase:    phasePrerolling,
		live:     live,
		buffered: true, // until the first <100% buffering report
		lastPos:  -1,
	}
	if live {
		p.track.phase = phasePlaying
	}

	p.mu.Unlock()
	p.send(Event{State: StateLoading})
	return nil
}

func (p *LinuxPlayer) Resume() error {
	p.mu.Lock()

	// Resume from the paused position: just move back to PLAYING without
	// re-setting the URI (which would restart the track from the beginning).
	ret := C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING)
	if ret == C.GST_STATE_CHANGE_FAILURE {
		p.mu.Unlock()
		return fmt.Errorf("failed to set GStreamer state to PLAYING")
	}

	p.track.phase = phasePlaying
	p.mu.Unlock()
	p.send(Event{State: StatePlaying})
	return nil
}

func (p *LinuxPlayer) Pause() error {
	p.mu.Lock()

	if !p.track.phase.wantsPlayback() {
		p.mu.Unlock()
		return nil
	}

	ret := C.gst_element_set_state(p.playbin, C.GST_STATE_PAUSED)
	if ret == C.GST_STATE_CHANGE_FAILURE {
		p.mu.Unlock()
		return fmt.Errorf("failed to set GStreamer state to PAUSED")
	}

	p.track.phase = phasePaused
	p.mu.Unlock()
	p.send(Event{State: StatePaused})
	return nil
}

func (p *LinuxPlayer) Stop() error {
	p.mu.Lock()
	C.gst_element_set_state(p.playbin, C.GST_STATE_READY)
	p.track.phase = phaseIdle
	p.mu.Unlock()
	p.send(Event{State: StateIdle})
	return nil
}

func (p *LinuxPlayer) SetVolume(v float64) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	// Volume in playbin is 0.0 to 1.0 (clamped)
	C.set_playbin_volume(p.playbin, C.double(v))
	return nil
}

// Seek is not yet wired on Linux (TODO: gst_element_seek_simple with
// GST_FORMAT_TIME). The bar's timeline degrades to read-only here.
func (p *LinuxPlayer) Seek(seconds float64) error {
	return nil
}

// monitorBus is the player's single bus-polling loop. It only routes: bus
// messages go to handleBusMessage, quiet ticks to confirmFromPosition.
func (p *LinuxPlayer) monitorBus() {
	defer p.wg.Done()

	bus := C.gst_element_get_bus(p.playbin)
	if bus == nil {
		log.Println("[audio] Failed to get GStreamer bus")
		return
	}
	defer C.gst_object_unref(C.gpointer(bus))

	for {
		select {
		case <-p.stopChan:
			return
		default:
			// Poll the bus with a 100ms timeout
			msg := C.gst_bus_timed_pop_filtered(
				bus,
				100*C.GST_MSECOND,
				C.GST_MESSAGE_ERROR|C.GST_MESSAGE_EOS|C.GST_MESSAGE_STATE_CHANGED|C.GST_MESSAGE_BUFFERING|C.GST_MESSAGE_ASYNC_DONE,
			)
			if msg == nil {
				p.confirmFromPosition()
				continue
			}
			p.handleBusMessage(msg)
			C.gst_message_unref(msg)
		}
	}
}

// confirmFromPosition reports StatePlaying from an advancing playback
// position. Live pipelines can stream audio without ever posting a settled
// PLAYING state-change (the async transition never completes), so without
// this the UI sits on "loading" despite audible playback.
func (p *LinuxPlayer) confirmFromPosition() {
	p.mu.Lock()
	wantConfirm := p.track.phase.wantsPlayback() && !p.track.confirmed
	p.mu.Unlock()
	if !wantConfirm {
		return
	}
	var pos C.gint64
	if C.gst_element_query_position(p.playbin, C.GST_FORMAT_TIME, &pos) == 0 || pos <= 0 {
		return
	}
	p.mu.Lock()
	advanced := p.track.lastPos >= 0 && pos > p.track.lastPos
	p.track.lastPos = pos
	if advanced {
		p.track.confirmed = true
		p.track.phase = phasePlaying
	}
	p.mu.Unlock()
	if advanced {
		p.send(Event{State: StatePlaying})
	}
}

// handleBusMessage dispatches one GStreamer bus message.
func (p *LinuxPlayer) handleBusMessage(msg *C.GstMessage) {
	fromPlaybin := C.get_message_src(msg) == (*C.GstObject)(unsafe.Pointer(p.playbin))

	switch C.get_message_type(msg) {
	case C.GST_MESSAGE_ERROR:
		var gerr *C.GError
		var debugInfo *C.gchar
		C.gst_message_parse_error(msg, &gerr, &debugInfo)
		errStr := C.GoString(gerr.message)
		C.g_error_free(gerr)
		C.g_free(C.gpointer(debugInfo))
		p.send(Event{State: StateError, Err: errStr})

	case C.GST_MESSAGE_EOS:
		// End-of-stream: the track played to its natural end. Emit
		// StateEnded (distinct from idle/paused) so the station player
		// can auto-advance. Livestreams never reach EOS (the static live
		// window does — its EOS drives the advance into a fresh window).
		p.send(Event{State: StateEnded})

	case C.GST_MESSAGE_ASYNC_DONE:
		if fromPlaybin {
			p.handleAsyncDone()
		}

	case C.GST_MESSAGE_BUFFERING:
		var percent C.gint
		C.gst_message_parse_buffering(msg, &percent)
		p.handleBuffering(int(percent))

	case C.GST_MESSAGE_STATE_CHANGED:
		if fromPlaybin {
			var oldState, newState, pendingState C.GstState
			C.gst_message_parse_state_changed(msg, &oldState, &newState, &pendingState)
			p.handleStateChanged(oldState, newState, pendingState)
		}
	}
}

// handleAsyncDone marks preroll settled and promotes if the buffer is ready.
func (p *LinuxPlayer) handleAsyncDone() {
	p.mu.Lock()
	p.track.prerolled = true
	promote := p.promotablePipeline()
	p.mu.Unlock()
	log.Printf("[audio] gst: ASYNC_DONE (promote=%v)", promote)
	if promote {
		C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING)
	}
}

// handleBuffering manages the PAUSED hold during the initial buffer fill.
//
// Network streams must sit in PAUSED until the buffer fills, then resume —
// without this the pipeline stalls in preroll and playback never starts
// (gst-launch does this dance for you; applications must do it themselves).
// The hold applies ONLY while prerolling: once the track plays, re-pausing on
// every sub-100% dip turns network jitter into audible stutter — the queue
// absorbs dips on its own. Promotion additionally waits for ASYNC_DONE:
// going PLAYING mid-preroll races the demuxer's segment event and the sink
// schedules buffers at their raw media timestamps — silence (see Play).
func (p *LinuxPlayer) handleBuffering(percent int) {
	p.mu.Lock()
	t := &p.track
	managed := !t.live && t.phase == phasePrerolling
	if managed {
		t.buffered = percent == 100
	}
	promote := managed && p.promotablePipeline()
	live, phase, prerolled := t.live, t.phase, t.prerolled
	p.mu.Unlock()

	if percent == 100 || percent%25 == 0 {
		log.Printf("[audio] gst: buffering %d%% (live=%v phase=%d prerolled=%v)", percent, live, phase, prerolled)
	}
	if !managed {
		return
	}
	if percent < 100 {
		C.gst_element_set_state(p.playbin, C.GST_STATE_PAUSED)
	} else if promote {
		C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING)
	}
}

// promotablePipeline reports whether the preroll dance is complete and the
// pipeline should be promoted to PLAYING. Callers must hold p.mu. The phase
// stays phasePrerolling until the PLAYING state-change lands, so a buffer dip
// between the promotion request and that state-change still re-pauses.
func (p *LinuxPlayer) promotablePipeline() bool {
	return p.track.phase == phasePrerolling && p.track.prerolled && p.track.buffered
}

// handleStateChanged turns settled playbin state changes into UI events.
// Phase gates each emit: transitional preroll pauses and the READY bounce in
// Play would otherwise flicker the UI through paused/idle.
func (p *LinuxPlayer) handleStateChanged(oldState, newState, pendingState C.GstState) {
	p.mu.Lock()
	phase := p.track.phase
	p.mu.Unlock()
	log.Printf("[audio] gst: playbin state %d->%d (pending %d, phase=%d)", int(oldState), int(newState), int(pendingState), phase)

	switch newState {
	case C.GST_STATE_PLAYING:
		// PLAYING is reported even mid-transition (pending != VOID): live
		// pipelines can stream audio while the async state change never
		// completes, and the UI would sit on "loading" despite audible
		// playback.
		p.mu.Lock()
		report := p.track.phase.wantsPlayback()
		if report {
			p.track.phase = phasePlaying
			p.track.confirmed = true
		}
		p.mu.Unlock()
		if report {
			p.send(Event{State: StatePlaying})
		}

	case C.GST_STATE_PAUSED:
		if pendingState != C.GST_STATE_VOID_PENDING {
			return // transitional preroll pause — not user-facing
		}
		// Settled PAUSED is user-facing only when the user asked for it;
		// during phasePrerolling it is the preroll hold before promotion.
		if phase == phasePaused {
			p.send(Event{State: StatePaused})
		}

	case C.GST_STATE_READY, C.GST_STATE_NULL:
		if pendingState != C.GST_STATE_VOID_PENDING {
			return
		}
		// Play() bounces through READY when (re)starting a track — only a
		// real stop should read as idle.
		if phase == phaseIdle {
			p.send(Event{State: StateIdle})
		}
	}
}

func (p *LinuxPlayer) Close() error {
	close(p.stopChan)
	p.wg.Wait()
	close(p.events)

	p.mu.Lock()
	defer p.mu.Unlock()

	if p.playbin != nil {
		C.gst_element_set_state(p.playbin, C.GST_STATE_NULL)
		C.gst_object_unref(C.gpointer(p.playbin))
		p.playbin = nil
	}

	return nil
}
