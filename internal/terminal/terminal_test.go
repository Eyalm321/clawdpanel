package terminal

import (
	"encoding/base64"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"unicode/utf16"

	"clawdpanel/internal/config"
)

// decodePwsh reverses encodePwshCommand: base64 → UTF-16LE → string.
func decodePwsh(b64 string) string {
	raw, err := base64.StdEncoding.DecodeString(b64)
	if err != nil {
		return ""
	}
	u := make([]uint16, len(raw)/2)
	for i := range u {
		u[i] = uint16(raw[i*2]) | uint16(raw[i*2+1])<<8
	}
	return string(utf16.Decode(u))
}

func contains(args []string, want string) bool {
	for _, a := range args {
		if a == want {
			return true
		}
	}
	return false
}

func indexOf(args []string, want string) int {
	for i, a := range args {
		if a == want {
			return i
		}
	}
	return -1
}

func joined(args []string) string { return strings.Join(args, "\x00") }

func TestNearestDot(t *testing.T) {
	cases := []struct {
		hex  string
		want string
	}{
		{"#FF0000", "🔴"},
		{"#0000FF", "🔵"},
		{"#008000", "🟢"},
		{"#FFFF00", "🟡"},
		{"#000000", "⚫"},
		{"#FFFFFF", "⚪"},
		{"#3B82F6", "🔵"}, // tailwind blue-500 → blue
		{"", ""},
		{"not-a-color", ""},
		{"#12345", ""}, // wrong length
	}
	for _, c := range cases {
		if got := nearestDot(c.hex); got != c.want {
			t.Errorf("nearestDot(%q) = %q, want %q", c.hex, got, c.want)
		}
	}
}

func TestQuoters(t *testing.T) {
	if got := shquote("a'b"); got != `'a'\''b'` {
		t.Errorf("shquote = %q", got)
	}
	if got := pwshquote("a'b"); got != "'a''b'" {
		t.Errorf("pwshquote = %q", got)
	}
	if got := osaquote(`a"b\c`); got != `"a\"b\\c"` {
		t.Errorf("osaquote = %q", got)
	}
}

func TestExpandHome(t *testing.T) {
	home, err := os.UserHomeDir()
	if err != nil {
		t.Skip("no home dir")
	}
	if got := expandHome("~"); got != home {
		t.Errorf("expandHome(~) = %q, want %q", got, home)
	}
	if got := expandHome("~/proj"); got != filepath.Join(home, "proj") {
		t.Errorf("expandHome(~/proj) = %q", got)
	}
	if got := expandHome("/abs/path"); got != "/abs/path" {
		t.Errorf("expandHome left absolute path alone: %q", got)
	}
}

func TestComposeShellCmd(t *testing.T) {
	// OSC title injection + keep-open + cd, with a malicious title/dir that
	// must be single-quoted (data, never executed).
	got := composeShellCmd("claude", "bash", true, true, "/a'b", "🔵 X'Y", "")
	wantCd := `cd '/a'\''b';`
	if !strings.Contains(got, wantCd) {
		t.Errorf("missing/incorrect cd: %q", got)
	}
	if !strings.Contains(got, `printf '\033]0;%s\007' '🔵 X'\''Y'`) {
		t.Errorf("missing/incorrect OSC injection: %q", got)
	}
	if !strings.HasSuffix(got, "; exec bash") {
		t.Errorf("missing keep-open suffix: %q", got)
	}
	if !strings.Contains(got, "claude") {
		t.Errorf("missing command: %q", got)
	}
	// We always own the title, so Claude Code's title rewriting is disabled.
	if !strings.HasPrefix(got, "export CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1'; ") {
		t.Errorf("missing disable-title prefix: %q", got)
	}

	// No OSC, no cd, sh keep-open.
	got = composeShellCmd("claude --resume", "sh", false, false, "/x", "T", "")
	if got != "export CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1'; claude --resume; exec sh" {
		t.Errorf("plain sh compose = %q", got)
	}

	// CLAUDE_CONFIG_DIR export prepended, per-shell syntax, before the command;
	// the disable-title env precedes it.
	bash := composeShellCmd("claude", "bash", false, false, "", "T", "/home/u/.acct")
	if bash != "export CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1'; export CLAUDE_CONFIG_DIR='/home/u/.acct'; claude; exec bash" {
		t.Errorf("bash env compose = %q", bash)
	}
	pwsh := composeShellCmd("claude", "pwsh", false, false, "", "T", `C:\a\.acct`)
	if pwsh != `$env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1'; $env:CLAUDE_CONFIG_DIR='C:\a\.acct'; claude` {
		t.Errorf("pwsh env compose = %q", pwsh)
	}
	cmd := composeShellCmd("claude", "cmd", false, false, "", "T", `C:\a\.acct`)
	if cmd != `set CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1&set CLAUDE_CONFIG_DIR=C:\a\.acct&claude` {
		t.Errorf("cmd env compose = %q", cmd)
	}
}

func TestBuildCustom(t *testing.T) {
	entry := config.TerminalConfig{Name: "CRM", Color: "#3B82F6", Dir: "/tmp/x"}
	launcher := config.LauncherConfig{
		Preset: "custom",
		Exe:    "myterm",
		Args:   []string{"--title", "{title}", "--cd", "{dir}", "--", "{cmd}"},
	}
	exe, args, err := build(entry, launcher, LaunchOpts{})
	if err != nil {
		t.Fatal(err)
	}
	if exe != "myterm" {
		t.Errorf("exe = %q", exe)
	}
	want := []string{"--title", "🔵 CRM", "--cd", "/tmp/x", "--", "claude"}
	if joined(args) != joined(want) {
		t.Errorf("custom args = %v, want %v", args, want)
	}
}

func TestBuildSublabelInTitle(t *testing.T) {
	entry := config.TerminalConfig{Name: "CRM", Color: "#3B82F6", Dir: "/tmp/x"}
	launcher := config.LauncherConfig{Preset: "custom", Exe: "t", Args: []string{"{title}"}}
	// Sublabel is appended after the name, inside the dot-prefixed title.
	_, args, err := build(entry, launcher, LaunchOpts{Sublabel: "backend"})
	if err != nil {
		t.Fatal(err)
	}
	if joined(args) != "🔵 CRM · backend" {
		t.Errorf("sublabel title = %v, want \"🔵 CRM · backend\"", args)
	}
	// Whitespace-only sublabel is treated as none.
	_, a2, _ := build(entry, launcher, LaunchOpts{Sublabel: "   "})
	if joined(a2) != "🔵 CRM" {
		t.Errorf("blank sublabel must not add a separator: %v", a2)
	}
}

func TestBuildAccountInTitle(t *testing.T) {
	entry := config.TerminalConfig{Name: "CRM", Color: "#3B82F6", Dir: "/tmp/x"}
	launcher := config.LauncherConfig{Preset: "custom", Exe: "t", Args: []string{"{title}"}}
	// Account is shown as "Name [Account]" after the emoji dot.
	_, args, err := build(entry, launcher, LaunchOpts{Account: "MAIN"})
	if err != nil {
		t.Fatal(err)
	}
	if joined(args) != "🔵 CRM [MAIN]" {
		t.Errorf("account title = %v, want \"🔵 CRM [MAIN]\"", args)
	}
	// Account + sublabel compose as "Name [Account] · sub".
	_, a2, _ := build(entry, launcher, LaunchOpts{Account: "MAIN", Sublabel: "backend"})
	if joined(a2) != "🔵 CRM [MAIN] · backend" {
		t.Errorf("account+sublabel title = %v, want \"🔵 CRM [MAIN] · backend\"", a2)
	}
}

func TestBuildCustomDefaultCommand(t *testing.T) {
	entry := config.TerminalConfig{Name: "X", Dir: "/tmp"}
	launcher := config.LauncherConfig{Preset: "custom", Exe: "t", Args: []string{"{cmd}"}}
	_, args, err := build(entry, launcher, LaunchOpts{})
	if err != nil {
		t.Fatal(err)
	}
	if joined(args) != "claude" {
		t.Errorf("default command not applied: %v", args)
	}
	// No color → no emoji dot prepended to the title.
	entry2 := config.TerminalConfig{Name: "Y", Dir: "/tmp"}
	l2 := config.LauncherConfig{Preset: "custom", Exe: "t", Args: []string{"{title}"}}
	_, a2, _ := build(entry2, l2, LaunchOpts{})
	if joined(a2) != "Y" {
		t.Errorf("uncolored title should be bare name: %v", a2)
	}
}

// ── Windows-only preset assertions ─────────────────────────────────────────────

func TestBuildWindowsTerminal(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("windows-only preset")
	}
	entry := config.TerminalConfig{Name: "CRM", Color: "#3B82F6", Dir: "/tmp/proj"}
	launcher := config.LauncherConfig{Preset: "windows-terminal"}
	exe, args, err := build(entry, launcher, LaunchOpts{})
	if err != nil {
		t.Fatal(err)
	}
	if exe != "wt.exe" {
		t.Errorf("exe = %q", exe)
	}
	// Color shows two ways: the nearest emoji dot in the title, and WT's native
	// --tabColor (which persists through focus changes, unlike a DWM border).
	ti := indexOf(args, "--title")
	if ti < 0 || args[ti+1] != "🔵 CRM" {
		t.Errorf("expected --title \"🔵 CRM\", got args=%v", args)
	}
	tc := indexOf(args, "--tabColor")
	if tc < 0 || args[tc+1] != "#3B82F6" {
		t.Errorf("expected --tabColor #3B82F6, got args=%v", args)
	}
	// Title must be pinned so `claude` can't rename the tab.
	if !contains(args, "--suppressApplicationTitle") {
		t.Errorf("expected --suppressApplicationTitle, got args=%v", args)
	}
	// Command is passed base64-encoded; decode and check it runs claude.
	ci := indexOf(args, "-EncodedCommand")
	if ci < 0 || ci+1 >= len(args) {
		t.Fatalf("no -EncodedCommand arg: %v", args)
	}
	if want := `$env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1'; claude`; decodePwsh(args[ci+1]) != want {
		t.Errorf("decoded command = %q, want %q", decodePwsh(args[ci+1]), want)
	}
}

func TestBuildWindowsTerminalEmptyColorBareTitle(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("windows-only preset")
	}
	entry := config.TerminalConfig{Name: "CRM", Color: "", Dir: "/tmp/proj"}
	launcher := config.LauncherConfig{Preset: "windows-terminal"}
	_, args, err := build(entry, launcher, LaunchOpts{})
	if err != nil {
		t.Fatal(err)
	}
	// No color → no emoji dot, just the bare name; and never a tab-color flag.
	ti := indexOf(args, "--title")
	if ti < 0 || args[ti+1] != "CRM" {
		t.Errorf("expected --title CRM, got args=%v", args)
	}
	if contains(args, "--tabColor") {
		t.Errorf("windows-terminal must not use --tabColor, got args=%v", args)
	}
}

func TestBuildWindowsTerminalEncodesEnvCommand(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("windows-only preset")
	}
	entry := config.TerminalConfig{Name: "CRM", Dir: "/tmp/proj"}
	launcher := config.LauncherConfig{Preset: "windows-terminal"}
	_, args, err := build(entry, launcher, LaunchOpts{Account: "MAIN", ConfigDir: `C:\Users\Admin\.claude`})
	if err != nil {
		t.Fatal(err)
	}
	// Account tag in the title.
	ti := indexOf(args, "--title")
	if ti < 0 || args[ti+1] != "CRM [MAIN]" {
		t.Errorf("expected --title \"CRM [MAIN]\", got args=%v", args)
	}
	// The command is base64-encoded for -EncodedCommand; the payload must carry
	// the CLAUDE_CONFIG_DIR export with a plain `;` (no wt-escaping needed) so
	// pwsh runs `claude` scoped to the account.
	ci := indexOf(args, "-EncodedCommand")
	if ci < 0 || ci+1 >= len(args) {
		t.Fatalf("no -EncodedCommand arg: %v", args)
	}
	want := `$env:CLAUDE_CODE_DISABLE_TERMINAL_TITLE='1'; $env:CLAUDE_CONFIG_DIR='C:\Users\Admin\.claude'; claude`
	if got := decodePwsh(args[ci+1]); got != want {
		t.Errorf("decoded command = %q, want %q", got, want)
	}
	// The encoded payload must be free of characters wt.exe would choke on.
	if strings.ContainsAny(args[ci+1], "; \"'") {
		t.Errorf("encoded command must be plain base64, got %q", args[ci+1])
	}
}




func TestBuildPowershellQuotesData(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("windows-only preset")
	}
	// A name with a single quote must be PowerShell-quoted (doubled) so it is
	// inert data, never executed.
	entry := config.TerminalConfig{Name: "a'b", Color: "#FF0000", Dir: "/tmp/x"}
	launcher := config.LauncherConfig{Preset: "powershell"}
	exe, args, err := build(entry, launcher, LaunchOpts{})
	if err != nil {
		t.Fatal(err)
	}
	if exe != "powershell" {
		t.Errorf("exe = %q", exe)
	}
	if !contains(args, "-NoExit") {
		t.Errorf("missing -NoExit (keep-open): %v", args)
	}
	script := args[len(args)-1]
	if !strings.Contains(script, "'🔴 a''b'") {
		t.Errorf("title not pwsh-quoted with emoji dot: %q", script)
	}
	if !strings.Contains(script, "Set-Location -LiteralPath '") {
		t.Errorf("dir not set via Set-Location: %q", script)
	}
	if !strings.HasSuffix(script, "claude") {
		t.Errorf("command not appended verbatim: %q", script)
	}
}

func TestBuildCmd(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("windows-only preset")
	}
	entry := config.TerminalConfig{Name: "CRM", Color: "#00FF00", Dir: "/tmp/x"}
	launcher := config.LauncherConfig{Preset: "cmd"}
	exe, args, err := build(entry, launcher, LaunchOpts{})
	if err != nil {
		t.Fatal(err)
	}
	if exe != "cmd.exe" {
		t.Errorf("exe = %q", exe)
	}
	want := []string{"/k", "title 🟢 CRM&set CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1&claude"}
	if joined(args) != joined(want) {
		t.Errorf("cmd args = %v, want %v", args, want)
	}
}

func TestBuildUnknownPreset(t *testing.T) {
	_, _, err := build(config.TerminalConfig{Name: "X"}, config.LauncherConfig{Preset: "nope"}, LaunchOpts{})
	if err == nil {
		t.Errorf("expected error for unknown preset")
	}
}

func TestPresetsIncludeCustom(t *testing.T) {
	ps := Presets()
	if len(ps) < 2 {
		t.Fatalf("expected at least 2 presets, got %d", len(ps))
	}
	if ps[len(ps)-1].Key != "custom" {
		t.Errorf("last preset should be custom, got %q", ps[len(ps)-1].Key)
	}
	for _, p := range ps {
		if p.Label == "" || p.OS == "" {
			t.Errorf("preset %q missing label/os: %+v", p.Key, p)
		}
	}
}
