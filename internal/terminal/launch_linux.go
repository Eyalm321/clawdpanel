//go:build linux

package terminal

import (
	"os"
	"os/exec"
	"path/filepath"
	"syscall"

	"clawdpanel/internal/config"
)

// builtinPresets for Linux, ordered by detection preference. Terminals with a
// native title flag use it; the rest (GNOME Terminal, WezTerm,
// x-terminal-emulator) emit an OSC title escape from inside the shell. Every
// preset re-execs `bash -lc` and appends `; exec bash` so the window stays
// open after the command exits.
func builtinPresets() []Preset {
	return []Preset{
		{
			Key:        "ghostty",
			Exe:        "ghostty",
			PreColor:   []string{"--title={title}", "--working-directory={dir}", "-e", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
		{
			Key:        "wezterm",
			Exe:        "wezterm",
			PreColor:   []string{"start", "--cwd", "{dir}", "--", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			NeedsOSC:   true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
		{
			Key:        "kitty",
			Exe:        "kitty",
			PreColor:   []string{"--title", "{title}", "--directory", "{dir}", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
		{
			Key:        "konsole",
			Exe:        "konsole",
			PreColor:   []string{"-p", "tabtitle={title}", "--workdir", "{dir}", "-e", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
		{
			Key:        "gnome-terminal",
			Exe:        "gnome-terminal",
			PreColor:   []string{"--working-directory={dir}", "--", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			NeedsOSC:   true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
		{
			Key:        "xterm",
			Exe:        "xterm",
			PreColor:   []string{"-T", "{title}", "-e", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			DirInShell: true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
		{
			Key:        "x-terminal-emulator",
			Exe:        "x-terminal-emulator",
			PreColor:   []string{"-e", "bash", "-lc", "{cmd}"},
			DotInTitle: true,
			NeedsOSC:   true,
			DirInShell: true,
			Shell:      "bash",
			Quote:      quoteNone,
		},
	}
}

// DetectDefault honours $TERMINAL first (mapped to a builtin when its basename
// matches a known exe), then scans the preset list in preference order.
func DetectDefault() config.LauncherConfig {
	if t := os.Getenv("TERMINAL"); t != "" {
		if _, err := exec.LookPath(t); err == nil {
			base := filepath.Base(t)
			for _, p := range builtinPresets() {
				if p.Exe == base {
					return config.LauncherConfig{Preset: p.Key}
				}
			}
		}
	}
	for _, p := range builtinPresets() {
		if _, err := exec.LookPath(p.Exe); err == nil {
			return config.LauncherConfig{Preset: p.Key}
		}
	}
	return config.LauncherConfig{Preset: "xterm"}
}

// detachAttrs: Setsid puts the terminal in its own session so it survives
// ClawdPanel exiting and isn't tied to our (absent) controlling terminal.
func detachAttrs() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{Setsid: true}
}

// wrapConsoleLaunch is a no-op on Linux: terminals here own their windows and
// the console-stdin problem is Windows-specific (see the Windows variant).
func wrapConsoleLaunch(exe string, args []string, _ bool) (string, []string, *syscall.SysProcAttr) {
	return exe, args, detachAttrs()
}



