//go:build linux

package config

import (
	"fmt"
	"os"
	"path/filepath"
)

const autostartFileName = "clawdpanel.desktop"

func autostartPath() (string, error) {
	base := os.Getenv("XDG_CONFIG_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		base = filepath.Join(home, ".config")
	}
	return filepath.Join(base, "autostart", autostartFileName), nil
}

func SetStartOnLogin(enabled bool, exePath string) error {
	path, err := autostartPath()
	if err != nil {
		return err
	}
	if !enabled {
		if err := os.Remove(path); err != nil && !os.IsNotExist(err) {
			return err
		}
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	entry := fmt.Sprintf(`[Desktop Entry]
Type=Application
Name=ClawdPanel
Exec=%s
X-GNOME-Autostart-enabled=true
NoDisplay=false
Hidden=false
Terminal=false
`, exePath)
	return os.WriteFile(path, []byte(entry), 0644)
}

func IsStartOnLogin() bool {
	path, err := autostartPath()
	if err != nil {
		return false
	}
	_, err = os.Stat(path)
	return err == nil
}
