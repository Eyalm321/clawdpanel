//go:build darwin

package config

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
)

const launchAgentLabel = "com.clawdpanel.app"

func launchAgentPath() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(home, "Library", "LaunchAgents", launchAgentLabel+".plist"), nil
}

func SetStartOnLogin(enabled bool, exePath string) error {
	plistPath, err := launchAgentPath()
	if err != nil {
		return err
	}
	if !enabled {
		_ = exec.Command("launchctl", "unload", plistPath).Run()
		if err := os.Remove(plistPath); err != nil && !os.IsNotExist(err) {
			return err
		}
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(plistPath), 0755); err != nil {
		return err
	}
	plist := fmt.Sprintf(`<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>%s</string>
	<key>ProgramArguments</key>
	<array>
		<string>%s</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
</dict>
</plist>
`, launchAgentLabel, exePath)
	if err := os.WriteFile(plistPath, []byte(plist), 0644); err != nil {
		return err
	}
	return exec.Command("launchctl", "load", "-w", plistPath).Run()
}

func IsStartOnLogin() bool {
	plistPath, err := launchAgentPath()
	if err != nil {
		return false
	}
	_, err = os.Stat(plistPath)
	return err == nil
}
