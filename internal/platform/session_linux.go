//go:build linux

package platform

import (
	"os"
	"strings"
	"sync"
)

// LinuxBackend is the window-management strategy chosen for the session. It
// replaces the old single isWayland() boolean: the three modes need genuinely
// different primitives (X11 struts/xdotool vs wlr-layer-shell vs a plain floating
// window), and picking the wrong one is what left the bar silently invisible.
type LinuxBackend int

const (
	// BackendX11 runs as an X11/XWayland client: wmctrl/xprop struts, xdotool
	// move/map, _NET_WM_* hints. The only fully-working dock + auto-hide path.
	// Default (main.go forces GDK_BACKEND=x11) so even on a Wayland session we
	// get real docking via XWayland when wmctrl can see the surface.
	BackendX11 LinuxBackend = iota
	// BackendLayerShell runs natively on Wayland and uses gtk4-layer-shell
	// (zwlr_layer_shell_v1) for anchoring + exclusive-zone docking. Works on
	// wlroots/KDE/COSMIC, NOT GNOME. Opt in via CLAWDPANEL_NO_XWAYLAND on a
	// compositor that advertises layer-shell.
	BackendLayerShell
	// BackendWaylandPlain runs natively on Wayland with no docking/auto-hide
	// (GNOME/Mutter: no layer-shell, no client positioning). Plain always-on-top
	// floating window; the reveal machine degrades to always-visible. Never
	// silently invisible.
	BackendWaylandPlain
)

func (b LinuxBackend) String() string {
	switch b {
	case BackendX11:
		return "x11"
	case BackendLayerShell:
		return "layer-shell"
	case BackendWaylandPlain:
		return "wayland-plain"
	default:
		return "unknown"
	}
}

// layerShellSupported probes whether the running compositor supports
// wlr-layer-shell via gtk4-layer-shell. It MUST only be called after gtk_init
// (gtk_layer_is_supported requires it), which is why the full backend decision
// is resolved lazily, post-init — see Backend(). The default is a stub (no
// layer-shell); layershell_linux.go replaces it with the real cgo probe.
var layerShellSupported = func() bool { return false }

// backendOverride parses CLAWDPANEL_BACKEND. ok=false means no/invalid override.
func backendOverride(env func(string) string) (LinuxBackend, bool) {
	switch strings.ToLower(strings.TrimSpace(env("CLAWDPANEL_BACKEND"))) {
	case "x11":
		return BackendX11, true
	case "layer-shell", "layershell":
		return BackendLayerShell, true
	case "wayland-plain", "wayland", "plain":
		return BackendWaylandPlain, true
	}
	return 0, false
}

// isX11Backend is the env-only half of the decision: would we run as an X11
// client (and thus force GDK_BACKEND=x11)? Pure so it can run in main() before
// GTK init without touching the layer-shell probe.
func isX11Backend(env func(string) string) bool {
	if ov, ok := backendOverride(env); ok {
		return ov == BackendX11
	}
	nativeWayland := env("WAYLAND_DISPLAY") != "" &&
		!strings.EqualFold(strings.TrimSpace(env("XDG_SESSION_TYPE")), "x11")
	if !nativeWayland {
		return true
	}
	// Wayland session: default to XWayland (force x11) unless explicitly opted
	// out, so the existing X11 docking path keeps working where it can.
	return env("CLAWDPANEL_NO_XWAYLAND") == ""
}

// WantsXWayland reports whether to force GDK_BACKEND=x11. Env-only and safe to
// call before GTK init (main.go does, to make the GDK decision).
func WantsXWayland() bool { return isX11Backend(os.Getenv) }

var (
	backendOnce sync.Once
	backend     LinuxBackend
)

// Backend resolves and caches the full backend. The layer-shell probe runs here,
// so the FIRST call must be after gtk_init (all callers are in domReady / window
// ops, which are post-init). main.go uses WantsXWayland() instead, never this.
func Backend() LinuxBackend {
	backendOnce.Do(func() {
		backend = detectBackend(os.Getenv, layerShellSupported)
	})
	return backend
}

// detectBackend is the pure, testable core. env reads the environment and
// layerShellProbe reports compositor layer-shell support.
func detectBackend(env func(string) string, layerShellProbe func() bool) LinuxBackend {
	if ov, ok := backendOverride(env); ok {
		return ov
	}
	if isX11Backend(env) {
		return BackendX11
	}
	// Native Wayland: layer-shell where the compositor supports it, else plain.
	if layerShellProbe() {
		return BackendLayerShell
	}
	return BackendWaylandPlain
}
