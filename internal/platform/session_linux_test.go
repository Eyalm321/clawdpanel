//go:build linux

package platform

import "testing"

func TestDetectBackend(t *testing.T) {
	cases := []struct {
		name       string
		env        map[string]string
		layerShell bool
		want       LinuxBackend
	}{
		{"plain x11 session", map[string]string{"XDG_SESSION_TYPE": "x11"}, false, BackendX11},
		{"no wayland display", map[string]string{}, false, BackendX11},
		{"wayland session, xwayland default", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland"}, true, BackendX11},
		{"wayland session reports x11 type", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "x11"}, true, BackendX11},
		{"native wayland, gnome (no layer-shell)", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland", "CLAWDPANEL_NO_XWAYLAND": "1"}, false, BackendWaylandPlain},
		{"native wayland, wlroots (layer-shell)", map[string]string{"WAYLAND_DISPLAY": "wayland-0", "XDG_SESSION_TYPE": "wayland", "CLAWDPANEL_NO_XWAYLAND": "1"}, true, BackendLayerShell},
		{"override x11", map[string]string{"CLAWDPANEL_BACKEND": "x11", "WAYLAND_DISPLAY": "wayland-0", "CLAWDPANEL_NO_XWAYLAND": "1"}, true, BackendX11},
		{"override layer-shell wins over no-probe", map[string]string{"CLAWDPANEL_BACKEND": "layer-shell"}, false, BackendLayerShell},
		{"override wayland-plain", map[string]string{"CLAWDPANEL_BACKEND": "wayland-plain", "WAYLAND_DISPLAY": "wayland-0"}, true, BackendWaylandPlain},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			env := func(k string) string { return c.env[k] }
			got := detectBackend(env, func() bool { return c.layerShell })
			if got != c.want {
				t.Errorf("detectBackend = %v, want %v", got, c.want)
			}
		})
	}
}
