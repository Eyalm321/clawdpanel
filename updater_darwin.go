//go:build darwin

package main

import "fmt"

// selectUpdateAsset returns no asset on macOS: silent .pkg installation needs
// admin elevation we don't implement yet, so the update window offers the
// releases page instead of a doomed in-place install.
func selectUpdateAsset(assets []releaseAsset) string {
	return ""
}

func resolveRelaunchPath(currentPath string) string {
	return currentPath
}

func runSilentInstaller(installerPath, appPath string) error {
	return fmt.Errorf("self-update is not supported on this platform")
}
