//go:build darwin && cgo
package audio

/*
#cgo CFLAGS: -x objective-c
#cgo LDFLAGS: -framework Foundation -framework AVFoundation
#import <stdlib.h>

void* createDarwinPlayer(void* goPlayer);
void playDarwinPlayer(void* playerPtr, const char* url);
void resumeDarwinPlayer(void* playerPtr);
void pauseDarwinPlayer(void* playerPtr);
void stopDarwinPlayer(void* playerPtr);
void setVolumeDarwinPlayer(void* playerPtr, float vol);
void freeDarwinPlayer(void* playerPtr);
*/
import "C"
import (
	"runtime/cgo"
	"sync"
	"unsafe"
)

type DarwinPlayer struct {
	mu        sync.Mutex
	emit      func(Event)
	ptr       unsafe.Pointer
	cgoHandle cgo.Handle
}

func New(emit func(Event)) (Player, error) {
	p := &DarwinPlayer{
		emit: emit,
	}
	p.cgoHandle = cgo.NewHandle(p)
	p.ptr = C.createDarwinPlayer(unsafe.Pointer(&p.cgoHandle))
	return p, nil
}

//export goDarwinPlayerCallback
func goDarwinPlayerCallback(goPlayer unsafe.Pointer, stateStr *C.char, errStr *C.char) {
	hPtr := (*cgo.Handle)(goPlayer)
	player := hPtr.Value().(*DarwinPlayer)

	// Copy the C strings into Go strings *before* we leave the C-callable
	// stack frame — they may be invalidated as soon as we return. GoString
	// copies, so the goroutine below holds independent storage.
	state := State(C.GoString(stateStr))
	var err string
	if errStr != nil {
		err = C.GoString(errStr)
	}

	// Dispatch the emit on a fresh goroutine to break any synchronous chain
	// back into Go state. AVFoundation delivers KVO synchronously on the
	// thread that mutated the observed property — so [AVPlayer play], called
	// from Controller.PlayVideo while it holds c.mu, re-enters this callback
	// → handlePlayerEvent → c.mu.Lock() on the SAME thread. On macOS the
	// Wails RPC dispatch is the main thread, so the whole UI hangs while
	// AVPlayer's worker keeps the audio going. Async breaks the chain; the
	// goroutine waits naturally for c.mu instead of deadlocking on itself.
	go player.emit(Event{
		State: state,
		Err:   err,
	})
}

func (p *DarwinPlayer) Play(url string) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	cURL := C.CString(url)
	defer C.free(unsafe.Pointer(cURL))

	C.playDarwinPlayer(p.ptr, cURL)
	return nil
}

func (p *DarwinPlayer) Pause() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	C.pauseDarwinPlayer(p.ptr)
	return nil
}

func (p *DarwinPlayer) Resume() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	C.resumeDarwinPlayer(p.ptr)
	return nil
}

func (p *DarwinPlayer) Stop() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	C.stopDarwinPlayer(p.ptr)
	return nil
}

func (p *DarwinPlayer) SetVolume(v float64) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	C.setVolumeDarwinPlayer(p.ptr, C.float(v))
	return nil
}

// Seek is not yet wired on macOS (TODO: AVPlayer seekToTime). The bar's
// timeline degrades to read-only here.
func (p *DarwinPlayer) Seek(seconds float64) error {
	return nil
}

func (p *DarwinPlayer) Close() error {
	p.mu.Lock()
	defer p.mu.Unlock()

	if p.ptr != nil {
		C.freeDarwinPlayer(p.ptr)
		p.ptr = nil
	}
	p.cgoHandle.Delete()
	return nil
}
