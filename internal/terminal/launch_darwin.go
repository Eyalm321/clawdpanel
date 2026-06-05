//go:build darwin

package terminal

import (
	"os"
	"os/exec"
	"syscall"

	"clawdpanel/internal/config"
)

// builtinPresets for macOS, ordered by detection preference: Ghostty, iTerm2,
// then the always-present Terminal.app. iTerm2 / Terminal.app are driven via
// osascript `do script`, with the title set by an OSC escape and the directory
// by `cd` inside the spawned login shell (which stays open on its own).
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
			Key:        "iterm2",
			Exe:        "osascript",
			PreColor:   []string{"-e", `tell application "iTerm" to create window with default profile command ` + "{cmd}"},
			DotInTitle: true,
			NeedsOSC:   true,
			DirInShell: true,
			Quote:      quoteOsa,
		},
		{
			Key:        "terminal-app",
			Exe:        "osascript",
			PreColor:   []string{"-e", `tell application "Terminal" to do script {cmd}`},
			DotInTitle: true,
			NeedsOSC:   true,
			DirInShell: true,
			Quote:      quoteOsa,
		},
	}
}

// DetectDefault prefers Ghostty (LookPath), then iTerm2 (app bundle present),
// else Terminal.app.
func DetectDefault() config.LauncherConfig {
	if _, err := exec.LookPath("ghostty"); err == nil {
		return config.LauncherConfig{Preset: "ghostty"}
	}
	for _, p := range []string{"/Applications/iTerm.app", expandHome("~/Applications/iTerm.app")} {
		if fi, err := os.Stat(p); err == nil && fi.IsDir() {
			return config.LauncherConfig{Preset: "iterm2"}
		}
	}
	return config.LauncherConfig{Preset: "terminal-app"}
}

// detachAttrs: on macOS Process.Release() alone suffices to detach the GUI
// terminal we launch (osascript / ghostty return immediately), so the zero
// value is correct.
func detachAttrs() *syscall.SysProcAttr {
	return &syscall.SysProcAttr{}
}

// wrapConsoleLaunch is a no-op on macOS: terminals here own their windows and
// the console-stdin problem is Windows-specific (see the Windows variant).
func wrapConsoleLaunch(exe string, args []string, _ bool) (string, []string, *syscall.SysProcAttr) {
	return exe, args, detachAttrs()
}

func GetPreExisting(preset string) map[uintptr]bool {
	return nil
}

func PostLaunch(preset string, entry config.TerminalConfig, title string, preExisting map[uintptr]bool) {}


