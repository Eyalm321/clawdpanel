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

// selectUpdateAsset picks the Windows NSIS installer from the release assets.
func selectUpdateAsset(assets []releaseAsset) string {
	for _, asset := range assets {
		lowerName := strings.ToLower(asset.Name)
		if strings.Contains(lowerName, "windows") && strings.HasSuffix(lowerName, ".exe") {
			return asset.BrowserDownloadURL
		}
	}
	// Fallback to first .exe asset if windows is not explicitly in the name
	for _, asset := range assets {
		if strings.HasSuffix(strings.ToLower(asset.Name), ".exe") {
			return asset.BrowserDownloadURL
		}
	}
	return ""
}

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
