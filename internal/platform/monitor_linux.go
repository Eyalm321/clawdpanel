//go:build linux

package platform

import (
	"log"
	"os"
	"os/exec"
	"strconv"
	"strings"
)

// gnomePanelOffset returns the height of GNOME Shell's top panel for the
// primary monitor (where it lives). The panel is compositor chrome: it draws
// over X11 docks regardless of stacking, and as a Wayland-native surface it
// can't be measured via X tooling — so we use its default 1×-scale height.
// CLAWDPANEL_TOP_OFFSET overrides for other scales/shell themes.
func gnomePanelOffset(isPrimary bool) int {
	if v := os.Getenv("CLAWDPANEL_TOP_OFFSET"); v != "" {
		if n, err := strconv.Atoi(v); err == nil {
			return n
		}
	}
	if !isPrimary || !strings.Contains(strings.ToLower(os.Getenv("XDG_CURRENT_DESKTOP")), "gnome") {
		return 0
	}
	return 32
}

// GetMonitors parses `xrandr --listmonitors` output. Example line:
//
//	" 0: +*HDMI-0 1920/598x1080/336+0+0  HDMI-0"
//
// Fields: index, primary marker, name, geometry (logical pixels/physical mm),
// X+Y origin, output name.
func GetMonitors() []MonitorInfo {
	out, err := exec.Command("xrandr", "--listmonitors").Output()
	if err != nil {
		log.Printf("platform: xrandr unavailable (%v); returning single fallback monitor", err)
		return []MonitorInfo{{
			Index: 0, Left: 0, Top: 0,
			Width: 1920, Height: 1080,
			PhysWidth: 1920, DpiScale: 1.0,
			IsPrimary: true, Name: "default",
		}}
	}
	var monitors []MonitorInfo
	for _, line := range strings.Split(string(out), "\n") {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "Monitors:") {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) < 4 {
			continue
		}
		// fields[0] = "0:" — index
		idxStr := strings.TrimSuffix(fields[0], ":")
		idx, err := strconv.Atoi(idxStr)
		if err != nil {
			continue
		}
		nameMarker := fields[1]
		isPrimary := strings.HasPrefix(nameMarker, "+*") || strings.HasPrefix(nameMarker, "*+")
		name := strings.TrimLeft(nameMarker, "+*")
		geom := fields[2] // e.g. "1920/598x1080/336+0+0"
		w, h, x, y, ok := parseXrandrGeometry(geom)
		if !ok {
			continue
		}
		monitors = append(monitors, MonitorInfo{
			Index:         idx,
			Left:          int32(x),
			Top:           int32(y),
			Width:         w,
			Height:        h,
			PhysWidth:     w,
			DpiScale:      1.0,
			IsPrimary:     isPrimary,
			Name:          name,
			WorkTopOffset: gnomePanelOffset(isPrimary),
		})
	}
	if len(monitors) == 0 {
		return []MonitorInfo{{
			Index: 0, Width: 1920, Height: 1080, PhysWidth: 1920,
			DpiScale: 1.0, IsPrimary: true, Name: "default",
		}}
	}
	for i := range monitors {
		monitors[i].DockEdge = pickDockEdge(monitors[i], monitors)
	}
	lastMonitors = monitors
	return monitors
}

// lastMonitors caches the most recent layout so DockToMonitor can re-check
// edge reservability without another xrandr round-trip.
var lastMonitors []MonitorInfo

func xOverlap(a, b MonitorInfo) bool {
	return int(a.Left) < int(b.Left)+widthPx(b) && int(b.Left) < int(a.Left)+widthPx(a)
}

func widthPx(m MonitorInfo) int {
	if m.PhysWidth != 0 {
		return m.PhysWidth
	}
	return m.Width
}

// edgeReservable reports whether a strut along the given edge of mon would
// reserve only mon's own space. Struts are measured from the ROOT screen
// edge, so any other monitor occupying the band between that root edge and
// mon (within mon's x-range) would be carved up instead.
func edgeReservable(mon MonitorInfo, all []MonitorInfo, edge string) bool {
	for _, o := range all {
		if o.Index == mon.Index || !xOverlap(mon, o) {
			continue
		}
		if edge == "bottom" {
			if int(o.Top)+o.Height > int(mon.Top)+mon.Height {
				return false
			}
		} else {
			if int(o.Top) < int(mon.Top) {
				return false
			}
		}
	}
	return true
}

// pickDockEdge prefers the top edge, falling back to the bottom when only the
// bottom can actually reserve space (e.g. a center monitor with another above
// it). If neither edge is reservable the bar still docks top — visible and
// above the stack, just without a strut.
func pickDockEdge(mon MonitorInfo, all []MonitorInfo) string {
	if edgeReservable(mon, all, "top") {
		return "top"
	}
	if edgeReservable(mon, all, "bottom") {
		return "bottom"
	}
	return "top"
}

// parseXrandrGeometry parses "1920/598x1080/336+0+0" → 1920, 1080, 0, 0.
// We discard the physical-mm size; DPI scale is set to 1.0 (HiDPI scaling on
// Linux is compositor-dependent and not derivable from xrandr alone).
func parseXrandrGeometry(s string) (w, h, x, y int, ok bool) {
	// Split on 'x' first: "1920/598" and "1080/336+0+0"
	parts := strings.SplitN(s, "x", 2)
	if len(parts) != 2 {
		return
	}
	wStr := strings.SplitN(parts[0], "/", 2)[0]
	w, err := strconv.Atoi(wStr)
	if err != nil {
		return
	}
	rest := parts[1] // "1080/336+0+0"
	// Find the first '+' or '-' after the height
	hEnd := 0
	for i := 0; i < len(rest); i++ {
		if rest[i] == '+' || rest[i] == '-' {
			hEnd = i
			break
		}
	}
	if hEnd == 0 {
		return
	}
	hStr := strings.SplitN(rest[:hEnd], "/", 2)[0]
	h, err = strconv.Atoi(hStr)
	if err != nil {
		return
	}
	// rest[hEnd:] is "+0+0" or similar — two signed ints
	off := rest[hEnd:]
	// Find second '+' or '-'
	splitIdx := 0
	for i := 1; i < len(off); i++ {
		if off[i] == '+' || off[i] == '-' {
			splitIdx = i
			break
		}
	}
	if splitIdx == 0 {
		return
	}
	x, err = strconv.Atoi(off[:splitIdx])
	if err != nil {
		return
	}
	y, err = strconv.Atoi(off[splitIdx:])
	if err != nil {
		return
	}
	return w, h, x, y, true
}
