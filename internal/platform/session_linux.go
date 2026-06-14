//go:build linux

package platform

import (
	"os"
	"strings"
	"sync"
)

// LinuxBackend is the window-management strategy chosen once, before GTK init.
// It replaces the old single isWayland() boolean: the three modes need genuinely
// different primitives (X11 struts/xdotool vs wlr-layer-shell vs a plain floating
// window), and picking the wrong one is what left the bar silently invisible.
type LinuxBackend int

const (
	// BackendX11 runs as an X11/XWayland client: wmctrl/xprop struts, xdotool
	// move/map, _NET_WM_* hints. The only fully-working dock + auto-hide path.
	// Selected by default (main.go forces GDK_BACKEND=x11) so even on a Wayland
	// session we get real docking via XWayland when wmctrl can see the surface.
	BackendX11 LinuxBackend = iota
	// BackendLayerShell runs natively on Wayland and uses gtk4-layer-shell
	// (zwlr_layer_shell_v1) for anchoring + exclusive-zone + auto-hide. Works on
	// wlroots/KDE/COSMIC, NOT GNOME. Selected only when XWayland is opted out and
	// the compositor advertises layer-shell.
	BackendLayerShell
	// BackendWaylandPlain runs natively on Wayland with no docking/auto-hide
	// support (GNOME/Mutter: no layer-shell, no client positioning). The bar is a
	// plain always-on-top floating window and the reveal machine degrades to
	// always-visible. Never silently invisible.
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
// wlr-layer-shell via gtk4-layer-shell. The P0 build has no layer-shell wrapper,
// so it reports false (→ BackendWaylandPlain on native Wayland); the P1 cgo
// wrapper replaces this var with a real dlopen + gtk_layer_is_supported probe.
var layerShellSupported = func() bool { return false }

var (
	backendOnce sync.Once
	backend     LinuxBackend
)

// DetectBackend decides the backend once and caches it. Call it before GTK init
// (main.go) so the GDK_BACKEND decision and every later window op agree.
func DetectBackend() LinuxBackend {
	backendOnce.Do(func() {
		backend = detectBackend(os.Getenv, layerShellSupported)
	})
	return backend
}

// Backend returns the detected backend (triggers detection on first use).
func Backend() LinuxBackend { return DetectBackend() }

// detectBackend is the pure, testable core. env reads the environment and
// layerShellProbe reports compositor layer-shell support; both are injected so
// the decision table can be asserted without a real session.
func detectBackend(env func(string) string, layerShellProbe func() bool) LinuxBackend {
	// Explicit override always wins.
	switch strings.ToLower(strings.TrimSpace(env("CLAWDPANEL_BACKEND"))) {
	case "x11":
		return BackendX11
	case "layer-shell", "layershell":
		return BackendLayerShell
	case "wayland-plain", "wayland", "plain":
		return BackendWaylandPlain
	}

	// Not a live Wayland session (or an explicit X11 session) → X11.
	nativeWayland := env("WAYLAND_DISPLAY") != "" &&
		!strings.EqualFold(strings.TrimSpace(env("XDG_SESSION_TYPE")), "x11")
	if !nativeWayland {
		return BackendX11
	}

	// On a Wayland session we still default to XWayland (main.go forces
	// GDK_BACKEND=x11) so the existing X11 docking path keeps working — unless
	// the user opts out of XWayland, in which case we go native.
	if env("CLAWDPANEL_NO_XWAYLAND") == "" {
		return BackendX11
	}

	if layerShellProbe() {
		return BackendLayerShell
	}
	return BackendWaylandPlain
}
