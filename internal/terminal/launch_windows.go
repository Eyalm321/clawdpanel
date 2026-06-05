//go:build windows

package terminal

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"
	"unsafe"

	"clawdpanel/internal/config"
)

var (
	user32                         = syscall.NewLazyDLL("user32.dll")
	procEnumWindows                = user32.NewProc("EnumWindows")
	procIsWindowVisible            = user32.NewProc("IsWindowVisible")
	procGetWindowTextW             = user32.NewProc("GetWindowTextW")
	procGetClassNameW              = user32.NewProc("GetClassNameW")
	procIsWindow                   = user32.NewProc("IsWindow")
	procSetWindowTextW             = user32.NewProc("SetWindowTextW")
	procShowWindow                 = user32.NewProc("ShowWindow")
	procSetWindowPos               = user32.NewProc("SetWindowPos")
	procGetWindowThreadProcessId   = user32.NewProc("GetWindowThreadProcessId")

	kernel32                       = syscall.NewLazyDLL("kernel32.dll")
	procOpenProcess                = kernel32.NewProc("OpenProcess")
	procCloseHandle                = kernel32.NewProc("CloseHandle")
	procQueryFullProcessImageNameW = kernel32.NewProc("QueryFullProcessImageNameW")

	dwmapi                    = syscall.NewLazyDLL("dwmapi.dll")
	procDwmSetWindowAttribute = dwmapi.NewProc("DwmSetWindowAttribute")
)

const (
	dwmwaBorderColor = uintptr(34)
	swRestore        = 9
	swpNoMove        = 0x0002
	swpNoZOrder      = 0x0004
)

func getWindowText(hwnd uintptr) string {
	var buf [512]uint16
	procGetWindowTextW.Call(hwnd, uintptr(unsafe.Pointer(&buf[0])), uintptr(len(buf)))
	return syscall.UTF16ToString(buf[:])
}

func getClassName(hwnd uintptr) string {
	var buf [256]uint16
	procGetClassNameW.Call(hwnd, uintptr(unsafe.Pointer(&buf[0])), uintptr(len(buf)))
	return syscall.UTF16ToString(buf[:])
}

// getWindowProcessName queries the executable filename of the process owning the window handle.
func getWindowProcessName(hwnd uintptr) string {
	var pid uint32
	procGetWindowThreadProcessId.Call(hwnd, uintptr(unsafe.Pointer(&pid)))
	if pid == 0 {
		return ""
	}

	// PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
	hProcess, _, _ := procOpenProcess.Call(0x1000, 0, uintptr(pid))
	if hProcess == 0 {
		return ""
	}
	defer procCloseHandle.Call(hProcess)

	var buf [1024]uint16
	size := uint32(len(buf))
	res, _, _ := procQueryFullProcessImageNameW.Call(hProcess, 0, uintptr(unsafe.Pointer(&buf[0])), uintptr(unsafe.Pointer(&size)))
	if res == 0 {
		return ""
	}

	path := syscall.UTF16ToString(buf[:size])
	return filepath.Base(path)
}

// getActiveHyperWindows returns a map of all currently visible Hyper window handles.
func getActiveHyperWindows() map[uintptr]bool {
	active := make(map[uintptr]bool)
	cb := syscall.NewCallback(func(hwnd uintptr, _ uintptr) uintptr {
		if vis, _, _ := procIsWindowVisible.Call(hwnd); vis != 0 {
			exeName := getWindowProcessName(hwnd)
			if strings.EqualFold(exeName, "Hyper.exe") {
				active[hwnd] = true
			}
		}
		return 1 // Continue enumerating
	})
	procEnumWindows.Call(cb, 0)
	return active
}

// GetPreExisting returns pre-existing Hyper windows.
func GetPreExisting(preset string) map[uintptr]bool {
	if preset == "hyper" {
		return getActiveHyperWindows()
	}
	return nil
}

// findHyperWindow searches for the Hyper terminal window by matching its process name,
// skipping any pre-existing windows.
func findHyperWindow(entryName string, preExisting map[uintptr]bool) uintptr {
	var found uintptr
	cb := syscall.NewCallback(func(hwnd uintptr, _ uintptr) uintptr {
		if vis, _, _ := procIsWindowVisible.Call(hwnd); vis != 0 {
			if preExisting != nil && preExisting[hwnd] {
				return 1 // Continue enumerating (skip pre-existing)
			}
			exeName := getWindowProcessName(hwnd)
			if strings.EqualFold(exeName, "Hyper.exe") {
				found = hwnd
				return 0 // Stop enumerating
			}
		}
		return 1 // Continue enumerating
	})
	procEnumWindows.Call(cb, 0)
	return found
}

// builtinPresets is ordered by detection preference: Windows Terminal first,
// then Hyper, then PowerShell, then Command Prompt.
func builtinPresets() []Preset {
	hyperExe := filepath.Join(os.Getenv("LOCALAPPDATA"), "Programs", "Hyper", "Hyper.exe")
	if _, err := os.Stat(hyperExe); err != nil {
		hyperExe = "Hyper.exe"
	}

	return []Preset{
		{
			Key: "windows-terminal",
			Exe: "wt.exe",
			PreColor:   []string{"-w", "new", "new-tab", "--suppressApplicationTitle", "--title", "{title}"},
			ColorArgs:  []string{"--tabColor", "{color}"},
			PostColor:  []string{"-d", "{dir}", "pwsh", "-NoExit", "-EncodedCommand", "{cmd}"},
			DotInTitle: true,
			Shell:      "pwsh",
			EncodeCmd:  true,
			Quote:      quoteNone,
		},
		{
			Key: "hyper",
			Exe: hyperExe,
			// Custom post-launch helper handles the launching Command execution, DWM styling and title locking
			PreColor:   []string{},
			DotInTitle: true,
			Shell:      "pwsh",
			Quote:      quoteNone,
		},
		{
			Key: "powershell",
			Exe: "powershell",
			PreColor: []string{"-NoExit", "-Command",
				"$host.UI.RawUI.WindowTitle = {title}; Set-Location -LiteralPath {dir}; {cmd}"},
			DotInTitle: true,
			Shell:      "pwsh",
			Console:    true,
			Quote:      quotePwsh,
		},
		{
			Key: "cmd",
			Exe: "cmd.exe",
			PreColor:   []string{"/k", "title {title}&{cmd}"},
			DotInTitle: true,
			Shell:      "cmd",
			Console:    true,
			Quote:      quoteNone,
		},
	}
}

// DetectDefault probes for an installed terminal in preference order.
func DetectDefault() config.LauncherConfig {
	for _, p := range builtinPresets() {
		if _, err := exec.LookPath(p.Exe); err == nil {
			return config.LauncherConfig{Preset: p.Key}
		}
	}
	return config.LauncherConfig{Preset: "powershell"}
}

// detachAttrs is the deliberate inverse of internal/audio's hidden helper: NO
// HideWindow / CREATE_NO_WINDOW. CREATE_NEW_PROCESS_GROUP detaches Ctrl-C.
func detachAttrs() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{CreationFlags: 0x00000200} // CREATE_NEW_PROCESS_GROUP
}

// wrapConsoleLaunch adapts the launch for console apps (PowerShell, cmd). The
// problem: ClawdPanel is a GUI process with no console, so when Go spawns a
// console child it wires the child's stdin to NUL (Go always sets
// STARTF_USESTDHANDLES). An interactive shell — `powershell -NoExit`, `cmd /k` —
// runs its command, then its prompt reads that NUL stdin, hits EOF, and exits
// immediately: the window flashes open and closes. Routing through `cmd /c start`
// hands the shell a brand-new console with live std handles instead of NUL, so it
// stays open. The cmd host itself runs windowless (CREATE_NO_WINDOW) and exits
// the moment `start` returns. GUI presets (Hyper, Windows Terminal) draw their
// own windows and never read stdin, so they launch unchanged.
func wrapConsoleLaunch(exe string, args []string, console bool) (string, []string, *syscall.SysProcAttr) {
	if !console {
		return exe, args, detachAttrs()
	}
	// `start "" <exe> <args…>`: the empty "" is the window title, so start treats
	// <exe> as the program to run rather than as a title.
	wrapped := append([]string{"/c", "start", "", exe}, args...)
	return "cmd.exe", wrapped, &syscall.SysProcAttr{CreationFlags: 0x08000000} // CREATE_NO_WINDOW
}

// PostLaunch handles custom window resizing, border coloring, and high-frequency title locking on Windows.
func PostLaunch(preset string, entry config.TerminalConfig, title string, preExisting map[uintptr]bool) {
	if preset != "hyper" {
		return
	}

	go func() {
		// Poll for Hyper window
		deadline := time.Now().Add(15 * time.Second)
		var hwnd uintptr
		for time.Now().Before(deadline) {
			if h := findHyperWindow(entry.Name, preExisting); h != 0 {
				hwnd = h
				break
			}
			time.Sleep(150 * time.Millisecond)
		}

		if hwnd == 0 {
			return
		}

		// Ensure window is in restored state (un-maximize/un-minimize)
		procShowWindow.Call(hwnd, swRestore)

		// Resize window to spacious layout (1200x700) to force full welcome screen with large mascot
		procSetWindowPos.Call(hwnd, 0, 0, 0, 1200, 700, swpNoMove|swpNoZOrder)

		// Apply Premium Custom Border Color via DWM matching entry.Color
		color := strings.TrimSpace(entry.Color)
		if color != "" {
			r, g, b, ok := parseHex(color)
			if ok {
				// COLORREF format (0x00bbggrr)
				colorVal := uint32(r) | (uint32(g) << 8) | (uint32(b) << 16)
				procDwmSetWindowAttribute.Call(hwnd, dwmwaBorderColor, uintptr(unsafe.Pointer(&colorVal)), 4)
			}
		}

		// Engage Active High-Frequency Title Lock
		lockedTitle := title
		if strings.TrimSpace(lockedTitle) == "" {
			lockedTitle = entry.Name
		}
		dot := nearestDot(color)
		if dot != "" && !strings.Contains(lockedTitle, dot) {
			lockedTitle = dot + " " + lockedTitle
		}

		titlePtr := syscall.StringToUTF16Ptr(lockedTitle)
		for {
			if vis, _, _ := procIsWindow.Call(hwnd); vis == 0 {
				break
			}
			// Guard against HWND reuse: when the Hyper window closes, Windows
			// recycles its handle value for an unrelated window (e.g. the
			// editor). IsWindow still reports true for the recycled handle, so
			// without re-verifying ownership the loop would hijack whatever now
			// owns it — retitling/resizing the wrong app. Re-check the owning
			// process before every retitle and stop once it's no longer Hyper.
			if !strings.EqualFold(getWindowProcessName(hwnd), "Hyper.exe") {
				break
			}
			procSetWindowTextW.Call(hwnd, uintptr(unsafe.Pointer(titlePtr)))
			time.Sleep(100 * time.Millisecond)
		}
	}()
}
