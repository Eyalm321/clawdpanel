//go:build linux

package platform

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Linux v1: Wails handles frameless + always-on-top via its built-in options
// on supported compositors. We shell out to wmctrl/xprop where available for
// the extras (opacity, click-through, dock hint). Wayland support is partial
// and compositor-dependent — see README "Known limitations".
//
// Click-through is not implementable via wmctrl/xprop alone (requires XShape
// extension calls), so it's a no-op at v1.

var (
	winIDOnce sync.Once
	winID     uint32
)

func isWayland() bool {
	// GDK_BACKEND=x11 means we're an X11/XWayland client even when the
	// session is Wayland (main.go forces this on Linux) — the X11 tooling
	// (wmctrl/xprop) then operates on our window normally.
	if strings.Contains(os.Getenv("GDK_BACKEND"), "x11") {
		return false
	}
	return os.Getenv("WAYLAND_DISPLAY") != ""
}

// findWindowID locates our X11 window ID via `wmctrl -lp` matching our PID.
// The window must already be mapped (shown) — wmctrl doesn't list hidden
// windows — so we retry briefly to ride out the show → map round-trip.
func findWindowID() uint32 {
	winIDOnce.Do(func() {
		pid := strconv.Itoa(os.Getpid())
		for attempt := 0; attempt < 10; attempt++ {
			out, err := exec.Command("wmctrl", "-lp").Output()
			if err != nil {
				log.Printf("platform: wmctrl unavailable (%v); window-specific ops disabled", err)
				return
			}
			for _, line := range strings.Split(string(out), "\n") {
				fields := strings.Fields(line)
				if len(fields) < 5 {
					continue
				}
				// fields: 0x<id> desktop pid host title...
				if fields[2] == pid {
					if id, err := strconv.ParseUint(strings.TrimPrefix(fields[0], "0x"), 16, 32); err == nil {
						winID = uint32(id)
						return
					}
				}
			}
			time.Sleep(200 * time.Millisecond)
		}
		log.Printf("platform: X11 window for PID %s not found after retries", pid)
	})
	return winID
}

func FindWindowByPID() (uintptr, error) {
	id := findWindowID()
	if id == 0 {
		return 0, fmt.Errorf("X11 window for PID %d not found (wmctrl missing or Wayland-only?)", os.Getpid())
	}
	return uintptr(id), nil
}

func ApplyBarStyles(hwnd uintptr) {
	if hwnd == 0 || isWayland() {
		return
	}
	id := fmt.Sprintf("0x%08x", uint32(hwnd))
	// Mark as a dock window so EWMH-compliant compositors keep it above
	// other windows. Some compositors (GNOME/Mutter) ignore this.
	_ = exec.Command("xprop", "-id", id, "-f", "_NET_WM_WINDOW_TYPE", "32a",
		"-set", "_NET_WM_WINDOW_TYPE", "_NET_WM_WINDOW_TYPE_DOCK").Run()
	_ = exec.Command("wmctrl", "-i", "-r", id, "-b", "add,above").Run()
}

func DockToMonitor(hwnd uintptr, mon MonitorInfo, barHeight int, appBarMode bool) {
	if hwnd == 0 {
		return
	}
	id := fmt.Sprintf("0x%08x", uint32(hwnd))
	width := mon.PhysWidth
	if width == 0 {
		width = mon.Width
	}
	// The bar rests below any compositor chrome (GNOME panel) on this
	// monitor — same convention as the darwin port (mon.Top + WorkTopOffset).
	top := int(mon.Top) + mon.WorkTopOffset
	// wmctrl -e gravity,x,y,w,h. Gravity 0 = default.
	geom := fmt.Sprintf("0,%d,%d,%d,%d", mon.Left, top, width, barHeight)
	_ = exec.Command("wmctrl", "-i", "-r", id, "-e", geom).Run()
	if appBarMode && !isWayland() && top == 0 {
		// _NET_WM_STRUT_PARTIAL reserves space from the ROOT screen edge, so a
		// top strut is only meaningful when the bar actually rests on the root
		// top edge. On a monitor below another (or below the GNOME panel) the
		// strut would carve space out of whatever sits above instead — skip it;
		// the bar stays above the stack but maximized windows will extend
		// under it (X11 offers no mid-screen reservation).
		strut := fmt.Sprintf("0,0,%d,0,0,0,0,0,%d,%d,0,0",
			barHeight, mon.Left, mon.Left+int32(width))
		_ = exec.Command("xprop", "-id", id, "-f", "_NET_WM_STRUT_PARTIAL", "32c",
			"-set", "_NET_WM_STRUT_PARTIAL", strut).Run()
	} else {
		_ = exec.Command("xprop", "-id", id, "-remove", "_NET_WM_STRUT_PARTIAL").Run()
	}
}

func RemoveAppBar(hwnd uintptr) {
	if hwnd == 0 {
		return
	}
	id := fmt.Sprintf("0x%08x", uint32(hwnd))
	_ = exec.Command("xprop", "-id", id, "-remove", "_NET_WM_STRUT_PARTIAL").Run()
}

func GetWindowSize(hwnd uintptr) (left, top, width, height int) {
	if hwnd == 0 {
		return 0, 0, 0, 0
	}
	id := fmt.Sprintf("0x%08x", uint32(hwnd))
	out, err := exec.Command("xdotool", "getwindowgeometry", "--shell", id).Output()
	if err != nil {
		return 0, 0, 0, 0
	}
	vals := map[string]int{}
	for _, line := range strings.Split(string(out), "\n") {
		if kv := strings.SplitN(line, "=", 2); len(kv) == 2 {
			if v, err := strconv.Atoi(strings.TrimSpace(kv[1])); err == nil {
				vals[strings.TrimSpace(kv[0])] = v
			}
		}
	}
	return vals["X"], vals["Y"], vals["WIDTH"], vals["HEIGHT"]
}

func SetWindowHeight(hwnd uintptr, physHeight int) {
	if hwnd == 0 {
		return
	}
	l, t, w, _ := GetWindowSize(hwnd)
	id := fmt.Sprintf("0x%08x", uint32(hwnd))
	geom := fmt.Sprintf("0,%d,%d,%d,%d", l, t, w, physHeight)
	_ = exec.Command("wmctrl", "-i", "-r", id, "-e", geom).Run()
}

func SetOpacity(hwnd uintptr, opacity float64) {
	if hwnd == 0 || isWayland() {
		return
	}
	if opacity < 0 {
		opacity = 0
	}
	if opacity > 1 {
		opacity = 1
	}
	// _NET_WM_WINDOW_OPACITY is a 32-bit cardinal: 0xFFFFFFFF = opaque.
	alpha := uint32(opacity * float64(0xFFFFFFFF))
	id := fmt.Sprintf("0x%08x", uint32(hwnd))
	_ = exec.Command("xprop", "-id", id, "-f", "_NET_WM_WINDOW_OPACITY", "32c",
		"-set", "_NET_WM_WINDOW_OPACITY", strconv.FormatUint(uint64(alpha), 10)).Run()
}

// IsFullScreenActive: stub on Linux. The mon argument matches the Windows
// signature (which scopes detection to the bar's display) but is unused here.
func IsFullScreenActive(MonitorInfo) bool { return false }

// AutoHideSupported: the slide/hide primitives below are wired up via xdotool,
// so the reveal machine's hover auto-hide and pin toggle work on Linux.
func AutoHideSupported() bool { return true }

// GetCursorPos reads the root-relative cursor position via xdotool. Returns
// (-1, -1) — the reveal machine's "no cursor source" sentinel — on failure.
func GetCursorPos() (int, int) {
	out, err := exec.Command("xdotool", "getmouselocation", "--shell").Output()
	if err != nil {
		return -1, -1
	}
	x, y := -1, -1
	for _, line := range strings.Split(string(out), "\n") {
		if kv := strings.SplitN(line, "=", 2); len(kv) == 2 {
			if v, err := strconv.Atoi(strings.TrimSpace(kv[1])); err == nil {
				switch strings.TrimSpace(kv[0]) {
				case "X":
					x = v
				case "Y":
					y = v
				}
			}
		}
	}
	return x, y
}

// ResetDwmFrame is a Windows-only concept; no-op elsewhere.
func ResetDwmFrame(hwnd uintptr) {}

// HideWindow / ShowWindow unmap/map the X window (collapse/reveal).
func HideWindow(hwnd uintptr) {
	if hwnd == 0 {
		return
	}
	_ = exec.Command("xdotool", "windowunmap", fmt.Sprintf("0x%08x", uint32(hwnd))).Run()
}

func ShowWindow(hwnd uintptr) {
	if hwnd == 0 {
		return
	}
	_ = exec.Command("xdotool", "windowmap", fmt.Sprintf("0x%08x", uint32(hwnd))).Run()
}

// MoveWindow repositions the bar (the reveal machine's slide animation).
func MoveWindow(hwnd uintptr, x, y int) {
	if hwnd == 0 {
		return
	}
	_ = exec.Command("xdotool", "windowmove", fmt.Sprintf("0x%08x", uint32(hwnd)),
		strconv.Itoa(x), strconv.Itoa(y)).Run()
}

// SetWindowClipTop no-op on Linux for now.
func SetWindowClipTop(hwnd uintptr, width, height, topClip int) {}

func SetClickThrough(hwnd uintptr, enabled bool) {
	// Click-through requires the XShape extension and a Go binding for it
	// (jezek/xgb/shape). Deferred to a follow-up; on Linux this is a no-op
	// at v1 and the option in the tray menu will not take effect.
	if enabled {
		log.Printf("platform: click-through not implemented on linux at v1")
	}
}
