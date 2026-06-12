//go:build linux

package main

import "testing"

// v2.2.1's actual asset list.
var releaseAssets = []releaseAsset{
	{Name: "clawdpanel-2.2.1-1.x86_64.rpm", BrowserDownloadURL: "https://example/rpm"},
	{Name: "ClawdPanel-2.2.1-macos-universal.pkg", BrowserDownloadURL: "https://example/pkg"},
	{Name: "ClawdPanel-2.2.1-windows-amd64-setup.exe", BrowserDownloadURL: "https://example/exe"},
	{Name: "ClawdPanel-x86_64.AppImage", BrowserDownloadURL: "https://example/appimage"},
	{Name: "clawdpanel_2.2.1_amd64.deb", BrowserDownloadURL: "https://example/deb"},
}

func TestSelectUpdateAssetFor(t *testing.T) {
	cases := []struct {
		flavor string
		want   string
	}{
		{"appimage", "https://example/appimage"},
		{"rpm", "https://example/rpm"},
		{"deb", "https://example/deb"},
		{"", ""}, // manual install / dev build: no in-place update
	}
	for _, c := range cases {
		if got := selectUpdateAssetFor(c.flavor, releaseAssets); got != c.want {
			t.Errorf("flavor %q: got %q, want %q", c.flavor, got, c.want)
		}
	}
}

// A test binary is neither an AppImage nor package-owned: the flavor must be
// "manual" so the updater never tries (and fails) an in-place install — the
// regression behind "UPDATE FAILED: self-update is not supported on this
// platform" on hand-installed binaries.
func TestInstallFlavorManualBinary(t *testing.T) {
	t.Setenv("APPIMAGE", "")
	if got := installFlavor(); got != "" {
		t.Errorf("installFlavor() = %q, want \"\" for a non-packaged binary", got)
	}
}

func TestInstallFlavorAppImage(t *testing.T) {
	t.Setenv("APPIMAGE", "/home/user/Apps/ClawdPanel.AppImage")
	if got := installFlavor(); got != "appimage" {
		t.Errorf("installFlavor() = %q, want appimage", got)
	}
}
