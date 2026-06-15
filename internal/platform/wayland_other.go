//go:build !linux

package platform

// Non-Linux builds (Windows, macOS) have no X11/Wayland backend concept, but the
// shared app/main/reveal code references these symbols unconditionally. They live
// in *_linux.go, so without these stubs the Windows and macOS builds fail to
// compile (undefined: platform.Backend, etc.).
//
// At runtime none of these are exercised off Linux: the backend switch and the
// motion-controller wiring in app.go are guarded by runtime.GOOS == "linux"
// (and eventMode, which short-circuits on the same check), and main.go's
// WantsXWayland call sits behind the same guard. The one gate that DOES evaluate
// on every platform — `Backend() == BackendX11` before PushdownEnable — must stay
// true off Linux to preserve the pre-Wayland docking behavior, so Backend()
// returns BackendX11 here.

// LinuxBackend mirrors the Linux enum so cross-platform code can name its
// constants; only BackendX11 is ever returned off Linux.
type LinuxBackend int

const (
	BackendX11 LinuxBackend = iota
	BackendLayerShell
	BackendWaylandPlain
)

func (b LinuxBackend) String() string {
	switch b {
	case BackendLayerShell:
		return "layer-shell"
	case BackendWaylandPlain:
		return "wayland-plain"
	default:
		return "x11"
	}
}

// Backend reports BackendX11 so the X11-gated docking/pushdown paths keep running
// exactly as they did before the Wayland work landed.
func Backend() LinuxBackend { return BackendX11 }

// IsWaylandSession / WantsXWayland are Linux-session probes; never true elsewhere.
func IsWaylandSession() bool { return false }
func WantsXWayland() bool    { return false }

// InitLayerShell, RegisterHoverCallback and AttachHoverController are GNOME/Wayland
// reveal primitives with no meaning off Linux — no-ops.
func InitLayerShell(hwnd uintptr)          {}
func RegisterHoverCallback(cb func(bool))  {}
func AttachHoverController(gtkWin uintptr) {}

// SetWindowSize backs the event-mode peek collapse, which only runs on a Linux
// Wayland session, so it's never called here — kept as a no-op for compilation.
func SetWindowSize(hwnd uintptr, width, height int) {}
