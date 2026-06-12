//go:build linux

package main

import (
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"strings"
	"syscall"
)

// installFlavor reports how this binary was installed, which decides both the
// release asset to download and the install mechanism:
//
//	"appimage" — running from an AppImage (APPIMAGE env set by the runtime);
//	            update = replace the .AppImage file and relaunch it.
//	"rpm"/"deb" — the executable belongs to a system package; update = install
//	            the new package via pkexec (polkit auth dialog).
//	""         — anything else (manually copied binary, dev build): no
//	            in-place update; the update window offers the releases page.
func installFlavor() string {
	if os.Getenv("APPIMAGE") != "" {
		return "appimage"
	}
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	if rpm, err := exec.LookPath("rpm"); err == nil {
		if exec.Command(rpm, "-qf", exe).Run() == nil {
			return "rpm"
		}
	}
	if dpkg, err := exec.LookPath("dpkg"); err == nil {
		if exec.Command(dpkg, "-S", exe).Run() == nil {
			return "deb"
		}
	}
	return ""
}

// selectUpdateAsset picks the release asset matching this install flavor.
func selectUpdateAsset(assets []releaseAsset) string {
	return selectUpdateAssetFor(installFlavor(), assets)
}

func selectUpdateAssetFor(flavor string, assets []releaseAsset) string {
	var suffix string
	switch flavor {
	case "appimage":
		suffix = ".appimage"
	case "rpm":
		suffix = ".rpm"
	case "deb":
		suffix = ".deb"
	default:
		return ""
	}
	for _, asset := range assets {
		if strings.HasSuffix(strings.ToLower(asset.Name), suffix) {
			return asset.BrowserDownloadURL
		}
	}
	return ""
}

func resolveRelaunchPath(currentPath string) string {
	// For an AppImage, the running executable is the mounted squashfs payload;
	// the relaunchable file is the .AppImage itself.
	if ai := os.Getenv("APPIMAGE"); ai != "" {
		return ai
	}
	return currentPath
}

func runSilentInstaller(installerPath, appPath string) error {
	switch installFlavor() {
	case "appimage":
		return updateAppImage(installerPath, appPath)
	case "rpm":
		return spawnPackageInstall("pkexec rpm -U --replacepkgs", installerPath, appPath)
	case "deb":
		return spawnPackageInstall("pkexec dpkg -i", installerPath, appPath)
	}
	return fmt.Errorf("self-update is not supported for this install (use the releases page)")
}

// updateAppImage swaps the new image over the running one and relaunches it.
// Write-to-sibling + rename keeps the replacement atomic; the squashfs mount
// of the running instance stays valid until exit.
func updateAppImage(installerPath, appImagePath string) error {
	newPath := appImagePath + ".new"
	src, err := os.Open(installerPath)
	if err != nil {
		return fmt.Errorf("open downloaded image: %w", err)
	}
	defer src.Close()
	dst, err := os.OpenFile(newPath, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o755)
	if err != nil {
		return fmt.Errorf("create replacement image: %w", err)
	}
	if _, err := io.Copy(dst, src); err != nil {
		dst.Close()
		os.Remove(newPath)
		return fmt.Errorf("write replacement image: %w", err)
	}
	if err := dst.Close(); err != nil {
		os.Remove(newPath)
		return fmt.Errorf("write replacement image: %w", err)
	}
	if err := os.Rename(newPath, appImagePath); err != nil {
		os.Remove(newPath)
		return fmt.Errorf("replace AppImage: %w", err)
	}
	os.Remove(installerPath)
	return spawnDetached(fmt.Sprintf(`sleep 1; exec %q`, appImagePath))
}

// spawnPackageInstall runs the package manager (behind a polkit auth dialog)
// after the app exits, then relaunches the installed binary. If the user
// cancels authentication the old version simply relaunches.
func spawnPackageInstall(installCmd, installerPath, appPath string) error {
	script := fmt.Sprintf(`sleep 1; %s %q; rm -f %q; exec %q`,
		installCmd, installerPath, installerPath, appPath)
	return spawnDetached(script)
}

// spawnDetached starts script in its own session so it survives the
// application's imminent os.Exit (mirrors the Windows updater's detached
// PowerShell).
func spawnDetached(script string) error {
	cmd := exec.Command("sh", "-c", script)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("spawn updater: %w", err)
	}
	log.Printf("[Updater] Detached updater started (pid %d)", cmd.Process.Pid)
	return nil
}
