package main

import (
	"embed"
	"log"
	"os"
	"runtime"

	"clawdpanel/internal/platform"

	"github.com/wailsapp/wails/v3/pkg/application"
	"github.com/wailsapp/wails/v3/pkg/events"
)

//go:embed all:frontend/dist
var assets embed.FS

// trayIconBytes is defined per-OS in icon_{windows,darwin,linux}.go.

func main() {
	// Pick the Linux window-management backend once, before GTK initializes.
	// Default is XWayland (BackendX11): Wayland gives apps no way to self-position
	// or reserve space, and GNOME/Mutter has no layer-shell for third parties, so
	// the docking path (wmctrl geometry, _NET_WM_WINDOW_TYPE_DOCK, struts) only
	// works as an X11 client. Force GDK_BACKEND=x11 ONLY for that backend; the
	// native-Wayland backends (opt in via CLAWDPANEL_NO_XWAYLAND / CLAWDPANEL_BACKEND)
	// must stay on GTK's Wayland backend.
	if runtime.GOOS == "linux" && platform.WantsXWayland() {
		os.Setenv("GDK_BACKEND", "x11")
	}

	app := NewApp()

	wailsApp := application.New(application.Options{
		Name:        "Clawd Panel",
		Description: "Claude Code Usage Panel",
		// Single-instance: a second launch fails to take the lock, pings the
		// running instance (which re-reveals the bar) and exits immediately, so
		// we never end up with two bars / two tray icons.
		SingleInstance: &application.SingleInstanceOptions{
			UniqueID: "com.clawdpanel.app",
			OnSecondInstanceLaunch: func(application.SecondInstanceData) {
				app.reveal()
			},
		},
		Assets: application.AssetOptions{
			Handler: application.AssetFileServerFS(assets),
		},
		Mac: application.MacOptions{
			ActivationPolicy: application.ActivationPolicyAccessory,
		},
		Services: []application.Service{
			application.NewService(app),
		},
	})

	window := wailsApp.Window.NewWithOptions(application.WebviewWindowOptions{
		Title:            "",
		Width:            1920,
		Height:           app.cfg.BarHeight,
		MinWidth:         400,
		MinHeight:        1,
		MaxHeight:        0,
		Frameless:        true,
		// Remote-debugging escape hatch: CLAWDPANEL_DEVTOOLS=1 enables the
		// WebKit inspector (pair with WEBKIT_INSPECTOR_HTTP_SERVER on Linux).
		DevToolsEnabled:  os.Getenv("CLAWDPANEL_DEVTOOLS") == "1",
		AlwaysOnTop:      true,
		// On Linux fixed-size WM hints would also block our own wmctrl resize
		// when docking the bar to the monitor width; dock-type windows aren't
		// user-resizable anyway.
		DisableResize:    runtime.GOOS != "linux",
		Hidden:           true,
		BackgroundColour: application.NewRGB(0x0B, 0x0C, 0x0E),
	})

	// DOM Ready hook
	window.OnWindowEvent(events.Common.WindowRuntimeReady, func(e *application.WindowEvent) {
		app.domReady(wailsApp, window)
	})

	// Startup hook
	app.startup(wailsApp, window)

	// Run the app
	err := wailsApp.Run()
	if err != nil {
		log.Fatalf("Wails error: %v", err)
	}
}
