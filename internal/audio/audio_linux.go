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

type LinuxPlayer struct {
	mu        sync.Mutex
	emit      func(Event)
	playbin   *C.GstElement
	stopChan  chan struct{}
	wg        sync.WaitGroup
	playing   bool
	live      bool // source gave NO_PREROLL: never manage buffering states
	buffering bool // mid-rebuffer: suppress paused/playing emit flicker
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
	}

	p.wg.Add(1)
	go p.monitorBus()

	return p, nil
}

func (p *LinuxPlayer) Play(url string) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	// Stop previous playback if active
	C.gst_element_set_state(p.playbin, C.GST_STATE_READY)

	cURL := C.CString(url)
	defer C.free(unsafe.Pointer(cURL))

	C.set_playbin_uri(p.playbin, cURL)

	ret := C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING)
	if ret == C.GST_STATE_CHANGE_FAILURE {
		return fmt.Errorf("failed to set GStreamer state to PLAYING")
	}
	// True live sources preroll with NO_PREROLL and must never be paused for
	// buffering; everything else (including HLS) is managed in monitorBus.
	p.live = ret == C.GST_STATE_CHANGE_NO_PREROLL
	p.buffering = false

	p.playing = true
	p.emit(Event{State: StateLoading})
	return nil
}

func (p *LinuxPlayer) Resume() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	// Resume from the paused position: just move back to PLAYING without
	// re-setting the URI (which would restart the track from the beginning).
	ret := C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING)
	if ret == C.GST_STATE_CHANGE_FAILURE {
		return fmt.Errorf("failed to set GStreamer state to PLAYING")
	}

	p.playing = true
	p.emit(Event{State: StatePlaying})
	return nil
}

func (p *LinuxPlayer) Pause() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	if !p.playing {
		return nil
	}

	ret := C.gst_element_set_state(p.playbin, C.GST_STATE_PAUSED)
	if ret == C.GST_STATE_CHANGE_FAILURE {
		return fmt.Errorf("failed to set GStreamer state to PAUSED")
	}

	p.playing = false
	p.emit(Event{State: StatePaused})
	return nil
}

func (p *LinuxPlayer) Stop() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	C.gst_element_set_state(p.playbin, C.GST_STATE_READY)
	p.playing = false
	p.emit(Event{State: StateIdle})
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
				C.GST_MESSAGE_ERROR|C.GST_MESSAGE_EOS|C.GST_MESSAGE_STATE_CHANGED|C.GST_MESSAGE_BUFFERING,
			)
			if msg == nil {
				continue
			}

			msgType := C.get_message_type(msg)
			switch msgType {
			case C.GST_MESSAGE_ERROR:
				var err *C.GError
				var debugInfo *C.gchar
				C.gst_message_parse_error(msg, &err, &debugInfo)
				errStr := C.GoString(err.message)
				C.g_error_free(err)
				C.g_free(C.gpointer(debugInfo))

				p.emit(Event{
					State: StateError,
					Err:   errStr,
				})

			case C.GST_MESSAGE_EOS:
				// End-of-stream: the track played to its natural end. Emit
				// StateEnded (distinct from idle/paused) so the station player
				// can auto-advance. Livestreams never reach EOS.
				p.emit(Event{State: StateEnded})

			case C.GST_MESSAGE_BUFFERING:
				// Network streams must sit in PAUSED until the buffer fills,
				// then resume — without this the pipeline stalls in preroll
				// and playback never starts. gst-launch does this dance for
				// you; applications must do it themselves.
				var percent C.gint
				C.gst_message_parse_buffering(msg, &percent)
				p.mu.Lock()
				live, wantPlaying := p.live, p.playing
				p.buffering = percent < 100
				p.mu.Unlock()
				if !live {
					if percent < 100 {
						C.gst_element_set_state(p.playbin, C.GST_STATE_PAUSED)
					} else if wantPlaying {
						C.gst_element_set_state(p.playbin, C.GST_STATE_PLAYING)
					}
				}

			case C.GST_MESSAGE_STATE_CHANGED:
				var oldState, newState, pendingState C.GstState
				C.gst_message_parse_state_changed(msg, &oldState, &newState, &pendingState)

				// Only process settled state changes of the playbin itself —
				// transitional states (pending != VOID) and buffering-induced
				// pauses would flicker the UI through paused/loading.
				p.mu.Lock()
				buffering := p.buffering
				p.mu.Unlock()
				if C.get_message_src(msg) == (*C.GstObject)(unsafe.Pointer(p.playbin)) &&
					pendingState == C.GST_STATE_VOID_PENDING && !buffering {
					switch newState {
					case C.GST_STATE_PLAYING:
						p.emit(Event{State: StatePlaying})
					case C.GST_STATE_PAUSED:
						p.emit(Event{State: StatePaused})
					case C.GST_STATE_READY, C.GST_STATE_NULL:
						// Play() bounces through READY when (re)starting a
						// track — only a real stop should read as idle.
						p.mu.Lock()
						stopped := !p.playing
						p.mu.Unlock()
						if stopped {
							p.emit(Event{State: StateIdle})
						}
					}
				}
			}

			C.gst_message_unref(msg)
		}
	}
}

func (p *LinuxPlayer) Close() error {
	close(p.stopChan)
	p.wg.Wait()

	p.mu.Lock()
	defer p.mu.Unlock()

	if p.playbin != nil {
		C.gst_element_set_state(p.playbin, C.GST_STATE_NULL)
		C.gst_object_unref(C.gpointer(p.playbin))
		p.playbin = nil
	}

	return nil
}
