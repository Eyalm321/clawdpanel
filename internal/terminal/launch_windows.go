//go:build windows

package terminal

import (
	"os"
	"os/exec"
	"path/filepath"
	"syscall"

	"clawdpanel/internal/config"
)

// builtinPresets is ordered by detection preference: Hyperpanes first,
// then Windows Terminal, then PowerShell, then Command Prompt.
func builtinPresets() []Preset {
	hyperpaneExe := filepath.Join(os.Getenv("LOCALAPPDATA"), "Programs", "Hyperpanes", "Hyperpanes.exe")
	if _, err := os.Stat(hyperpaneExe); err != nil {
		sys := filepath.Join("C:\\Program Files\\Hyperpanes", "Hyperpanes.exe")
		if _, err := os.Stat(sys); err == nil {
			hyperpaneExe = sys
		} else {
			hyperpaneExe = "Hyperpanes.exe"
		}
	}

	return []Preset{
		{
			Key:        "hyperpanes",
			Exe:        hyperpaneExe,
			PreColor:   []string{"-c", "{cmd}", "--label", "{title}", "--cwd", "{dir}"},
			ColorArgs:  []string{"--color", "{color}"},
			PostColor:  []string{"--shell", "pwsh"},
			DotInTitle: true,
			Shell:      "pwsh",
			Quote:      quoteNone,
		},
		{
			Key:        "windows-terminal",
			Exe:        "wt.exe",
			PreColor:   []string{"-w", "new", "new-tab", "--suppressApplicationTitle", "--title", "{title}"},
			ColorArgs:  []string{"--tabColor", "{color}"},
			PostColor:  []string{"-d", "{dir}", "pwsh", "-NoExit", "-EncodedCommand", "{cmd}"},
			DotInTitle: true,
			Shell:      "pwsh",
			EncodeCmd:  true,
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
			Key:        "cmd",
			Exe:        "cmd.exe",
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
// the moment `start` returns. GUI presets (Windows Terminal, Hyperpanes) draw their
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
