//go:build windows

package main

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
)

func resolveRelaunchPath(currentPath string) string {
	isDev := Version == "dev" || strings.Contains(strings.ToLower(currentPath), "claudebar")

	if isDev {
		candidates := []string{
			filepath.Join(os.Getenv("ProgramFiles"), "ClawdPanel", "clawdpanel.exe"),
			filepath.Join(os.Getenv("ProgramFiles(x86)"), "ClawdPanel", "clawdpanel.exe"),
			filepath.Join(os.Getenv("LOCALAPPDATA"), "Programs", "ClawdPanel", "clawdpanel.exe"),
		}

		for _, candidate := range candidates {
			if candidate == "" {
				continue
			}
			if _, err := os.Stat(candidate); err == nil {
				log.Printf("[Updater] Dev mode detected. Resolving relaunch path to official installation: %s", candidate)
				return candidate
			}
		}
	}

	return currentPath
}

func runSilentInstaller(installerPath, appPath string) error {
	psCommand := fmt.Sprintf(
		`Start-Sleep -Seconds 1; Stop-Process -Name "ClawdPanel", "clawdpanel" -Force -ErrorAction SilentlyContinue; Start-Process -FilePath "%s" -ArgumentList "/S" -Verb RunAs -Wait; Start-Process -FilePath "%s"`,
		installerPath, appPath,
	)

	cmd := exec.Command("powershell", "-NoProfile", "-Command", psCommand)
	cmd.SysProcAttr = &syscall.SysProcAttr{
		HideWindow:    true,
		CreationFlags: 0x08000000, // CREATE_NO_WINDOW
	}
	return cmd.Start()
}
