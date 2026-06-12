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
	// wmctrl -e gravity,x,y,w,h. Gravity 0 = default.
	geom := fmt.Sprintf("0,%d,%d,%d,%d", mon.Left, mon.Top, width, barHeight)
	_ = exec.Command("wmctrl", "-i", "-r", id, "-e", geom).Run()
	if appBarMode && !isWayland() {
		// _NET_WM_STRUT_PARTIAL: left, right, top, bottom, then four ranges.
		// We reserve "top" = barHeight, range = full mon.Left .. mon.Left+width.
		strut := fmt.Sprintf("0,0,%d,0,0,0,0,0,%d,%d,0,0",
			barHeight, mon.Left, mon.Left+int32(width))
		_ = exec.Command("xprop", "-id", id, "-f", "_NET_WM_STRUT_PARTIAL", "32c",
			"-set", "_NET_WM_STRUT_PARTIAL", strut).Run()
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

// AutoHideSupported is false on Linux — the slide-up animation primitives
// aren't wired up at v1.
func AutoHideSupported() bool { return false }

// GetCursorPos is a stub on Linux; the hover-watcher is Windows-only for v1.
func GetCursorPos() (int, int) { return -1, -1 }

// ResetDwmFrame is a Windows-only concept; no-op elsewhere.
func ResetDwmFrame(hwnd uintptr) {}

// HideWindow / ShowWindow no-op on Linux for now (auto-hide is Windows-only v1).
func HideWindow(hwnd uintptr) {}
func ShowWindow(hwnd uintptr) {}

// MoveWindow no-op on Linux for now (slide animation is Windows-only v1).
func MoveWindow(hwnd uintptr, x, y int) {}

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
