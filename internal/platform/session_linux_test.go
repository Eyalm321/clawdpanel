//go:build linux

package platform

import "testing"

func TestDetectBackend(t *testing.T) {
	cases := []struct {
		name       string
		env        map[string]string
		layerShell bool
		want       LinuxBackend
		wantX11    bool // WantsXWayland / isX11Backend expectation
	}{
		{"plain x11 session", map[string]string{"XDG_SESSION_TYPE": "x11"}, false, BackendX11, true},
		{"no wayland display", map[string]string{}, false, BackendX11, true},
		{"wayland session, xwayland default", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland"}, true, BackendX11, true},
		{"wayland session reports x11 type", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "x11"}, true, BackendX11, true},
		{"native wayland gnome (no layer-shell)", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland", "CLAWDPANEL_NO_XWAYLAND": "1"}, false, BackendWaylandPlain, false},
		{"native wayland wlroots (layer-shell)", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland", "CLAWDPANEL_NO_XWAYLAND": "1"}, true, BackendLayerShell, false},
		{"override x11 beats no-xwayland", map[string]string{"CLAWDPANEL_BACKEND": "x11", "WAYLAND_DISPLAY": "wayland-0", "CLAWDPANEL_NO_XWAYLAND": "1"}, true, BackendX11, true},
		{"override layer-shell (no probe needed)", map[string]string{"CLAWDPANEL_BACKEND": "layer-shell"}, false, BackendLayerShell, false},
		{"override wayland-plain", map[string]string{"CLAWDPANEL_BACKEND": "wayland-plain", "WAYLAND_DISPLAY": "wayland-0"}, true, BackendWaylandPlain, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			env := func(k string) string { return c.env[k] }
			if got := detectBackend(env, func() bool { return c.layerShell }); got != c.want {
				t.Errorf("detectBackend = %v, want %v", got, c.want)
			}
			if got := isX11Backend(env); got != c.wantX11 {
				t.Errorf("isX11Backend = %v, want %v (gates GDK_BACKEND=x11)", got, c.wantX11)
			}
		})
	}
}
