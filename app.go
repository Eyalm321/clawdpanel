package main

import (
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"clawdpanel/internal/audio"
	"clawdpanel/internal/claude"
	"clawdpanel/internal/config"
	"clawdpanel/internal/platform"
	"clawdpanel/internal/radio"
	"clawdpanel/internal/reveal"
	"clawdpanel/internal/station"
	"clawdpanel/internal/tray"

	"context"

	"github.com/wailsapp/wails/v3/pkg/application"
	"github.com/wailsapp/wails/v3/pkg/events"
)

// radioResolverAdapter bridges *radio.Resolver to the audio.StreamResolver
// interface (different ResolvedTrack types keep the audio layer independent of
// the youtube-backed radio package).
type radioResolverAdapter struct{ r *radio.Resolver }

func (a radioResolverAdapter) Resolve(ctx context.Context, videoID string, forceRefresh bool) (audio.ResolvedTrack, error) {
	t, err := a.r.Resolve(ctx, videoID, forceRefresh)
	return audio.ResolvedTrack{URL: t.URL, IsLive: t.IsLive}, err
}

// Version is set via -ldflags "-X main.Version=x.y.z" at build time.
var Version = "dev"

type App struct {
	app      *application.App
	window   *application.WebviewWindow
	cfg      *config.Config
	monitors []platform.MonitorInfo
	hwnd     uintptr
	trayMgr  *tray.Manager
	radio    *radio.Resolver

	// audioCtrl/station are the radio "resource dependency": the native audio
	// engine (a background player process) and the queue that drives it. They
	// exist only while the Radio feature is enabled — created in initAudio,
	// destroyed in teardownAudio when the user toggles Radio off — so disabling
	// Radio frees the resource, not just the UI. audioMu guards the two pointers
	// because they're written from the SaveConfig goroutine but read from the
	// audio event callback and the Radio* binding goroutines.
	audioMu   sync.Mutex
	audioCtrl *audio.Controller
	station   *station.StationPlayer

	// settingsWindow is the reusable popup editor. Created lazily on first use
	// and hidden (not destroyed) on close, so reopening is cheap.
	settingsWindow *application.WebviewWindow

	// menuWindow is the small dropdown anchored under the brand icon
	// (Check for updates / Exit). Created lazily, hidden (not destroyed) on
	// close, and auto-hidden when it loses focus.
	menuWindow  *application.WebviewWindow
	menuVisible bool      // tracks whether the dropdown is currently shown
	menuShownAt time.Time // guards the focus-loss auto-hide against a spurious
	//                        WindowLostFocus during the show/focus transition
	menuHiddenAt time.Time // when it was last hidden, so a toggle click can tell
	//                        a self-inflicted auto-hide from a fresh open request

	// updateWindow is the reusable popup updater dialog overlay.
	updateWindow     *application.WebviewWindow
	lastUpdateResult UpdateCheckResult

	// revealCtrl owns the slide animation + click-through behind the WindowOps
	// seam. The hover watcher below decides *when* to reveal/collapse and drives
	// it via SetExpanded/Expanded (moving the watcher in is issue #3).
	revealCtrl *reveal.Controller

	domReadyOnce bool // guards against WindowRuntimeReady firing twice on Windows WebView2
}

func NewApp() *App {
	// Redirect log output to %APPDATA%\ClawdPanel\debug.log for crash diagnosis.
	logPath := filepath.Join(config.AppDataDir(), "debug.log")
	_ = os.MkdirAll(config.AppDataDir(), 0755)
	if f, err := os.OpenFile(logPath, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0644); err == nil {
		log.SetOutput(f)
		log.SetFlags(log.Ldate | log.Ltime)
	}

	cfg, err := config.Load()
	if err != nil {
		log.Printf("config load error: %v — using defaults", err)
		def := config.Defaults()
		cfg = &def
	}
	// Always launch pinned regardless of last-session state — the user prefers
	// to start with the bar docked, even if they collapsed it last time.
	cfg.Pinned = true
	cfg.AppBarMode = true
	// The bar is always fully opaque — no see-through. (Older configs may carry a
	// translucent value like 0.92; override it.)
	cfg.Opacity = 1.0
	res := radio.New()
	app := &App{cfg: cfg, radio: res}

	// Only stand up the native audio engine when the Radio feature is enabled —
	// when it's off we never spawn the background player process at all.
	if cfg.Features.Radio {
		app.initAudio()
	}

	return app
}

// initAudio builds the native audio controller and the station player that
// drives it, wiring controller events through the station (auto-advance/loop)
// and on to the frontend. Idempotent: a no-op if the engine is already up. Safe
// to call at runtime when the user re-enables Radio.
func (a *App) initAudio() {
	a.audioMu.Lock()
	if a.audioCtrl != nil {
		a.audioMu.Unlock()
		return
	}
	a.audioMu.Unlock()

	ctrl, err := audio.NewController(radioResolverAdapter{a.radio}, func(ev audio.Event) {
		// Route every controller event through the station player, which
		// auto-advances/loops and then forwards (enriched) to the frontend.
		st := a.getStation()
		if st != nil {
			st.OnAudioEvent(ev)
		} else if a.app != nil {
			a.app.Event.Emit("radio:state", ev)
		}
	})
	if err != nil {
		log.Printf("[audio] Failed to initialize native audio controller: %v", err)
		return
	}
	st := station.New(ctrl, a.radio, func(ev audio.Event) {
		if a.app != nil {
			a.app.Event.Emit("radio:state", ev)
		}
	})
	st.SetStations(a.cfg.Stations)

	a.audioMu.Lock()
	a.audioCtrl = ctrl
	a.station = st
	a.audioMu.Unlock()

	// Re-apply the persisted volume so a runtime re-enable matches the bar.
	if a.cfg.RadioVolume > 0 {
		_ = st.SetVolume(a.cfg.RadioVolume)
	}
}

// teardownAudio stops playback and shuts the native audio engine down, releasing
// the background player process. Called when the user disables Radio. Emits an
// idle state so the bar's radio segment (if still shown) resets to [OFF].
func (a *App) teardownAudio() {
	a.audioMu.Lock()
	st := a.station
	ctrl := a.audioCtrl
	a.station = nil
	a.audioCtrl = nil
	a.audioMu.Unlock()

	if st != nil {
		_ = st.Stop()
	}
	if ctrl != nil {
		_ = ctrl.Close()
	}
	if a.app != nil {
		a.app.Event.Emit("radio:state", audio.Event{State: audio.StateIdle})
	}
}

// getStation returns the current station player under the lock (nil while Radio
// is disabled). Callers invoke methods on the returned pointer outside the lock
// — StationPlayer has its own internal synchronization.
func (a *App) getStation() *station.StationPlayer {
	a.audioMu.Lock()
	defer a.audioMu.Unlock()
	return a.station
}

func (a *App) startup(app *application.App, window *application.WebviewWindow) {
	a.app = app
	a.window = window
}

func (a *App) domReady(app *application.App, window *application.WebviewWindow) {
	// WindowRuntimeReady can fire more than once on Windows WebView2 (initial
	// about:blank + the real asset load, or any in-app navigation). Without this
	// guard each fire would build a second tray icon and spawn a second
	// hover-watcher goroutine.
	if a.domReadyOnce {
		return
	}
	a.domReadyOnce = true

	time.Sleep(300 * time.Millisecond)

	hwnd := uintptr(window.NativeWindow())
	a.hwnd = hwnd
	a.revealCtrl = reveal.New(hwnd)
	platform.ApplyBarStyles(hwnd)

	a.monitors = platform.GetMonitors()
	if a.cfg.Monitor >= len(a.monitors) {
		a.cfg.Monitor = 0
	}

	if a.hwnd != 0 && len(a.monitors) > 0 {
		platform.DockToMonitor(a.hwnd, a.monitors[a.cfg.Monitor], a.cfg.BarHeight, a.cfg.AppBarMode)
		if a.cfg.AppBarMode && a.cfg.Pinned {
			go func() {
				if err := platform.PushdownEnable(a.monitors[a.cfg.Monitor], a.cfg.BarHeight); err != nil {
					log.Printf("[pushdown] Enable failed: %v", err)
				}
			}()
		}
		platform.SetOpacity(a.hwnd, a.cfg.Opacity)
		a.revealCtrl.Configure(a.monitors[a.cfg.Monitor], a.cfg.BarHeight, a.cfg.Pinned, a.cfg.ClickThrough)
		// Init sets the initial visual state (pinned ⇒ expanded, else follow the
		// cursor) without animating, snapping the window off-screen + hiding it
		// if starting collapsed so nothing flashes on launch.
		a.revealCtrl.Init()
	}

	a.runTray()
	if runtime.GOOS == "linux" {
		// Linux v1: the window starts Hidden and the reveal controller's
		// hide/show + cursor-poll primitives are no-ops here, so nothing
		// would ever surface the bar (and GNOME needs an extension to show
		// the tray icon). Show it unconditionally; auto-hide stays
		// Windows-only.
		window.Show()
	}
	// The reveal controller owns the cursor poll loop and the whole auto-hide
	// state machine now; App just starts it.
	go a.revealCtrl.Run(a.app.Context())
	// Start the Claude status watcher poller
	go a.watchClaudeStatus(a.app.Context())
}

// reveal surfaces the bar, called when the user launches a second instance (which
// exits immediately under single-instance mode). If the bar was auto-hidden /
// collapsed it slides back on screen; when pinned and already visible it's a
// no-op. Guarded so it does nothing if the first instance is still mid-startup
// (the controller isn't built until the native window handle is known).
func (a *App) reveal() {
	if a.revealCtrl == nil {
		return
	}
	a.revealCtrl.Reveal()
}

func (a *App) shutdown() {
	platform.PushdownDisable()
	if a.hwnd != 0 {
		platform.RemoveAppBar(a.hwnd)
	}
	if a.trayMgr != nil {
		a.trayMgr.Quit()
	}
}

func (a *App) runTray() {
	names := make([]string, len(a.cfg.Accounts))
	for i, acc := range a.cfg.Accounts {
		names[i] = acc.Name
	}
	a.trayMgr = tray.New()
	a.trayMgr.Build(
		a.app,
		a,
		trayIconBytes,
		Version,
		names,
		len(a.monitors),
		a.cfg.StartWithWindows,
		a.cfg.ActiveAccount,
		a.cfg.Monitor,
	)
}

// tray.Controller implementation callbacks

func (a *App) ToggleStartup() {
	a.cfg.StartWithWindows = !a.cfg.StartWithWindows
	exePath, _ := os.Executable()
	_ = config.SetStartOnLogin(a.cfg.StartWithWindows, exePath)
	_ = config.Save(a.cfg)
	if a.trayMgr != nil {
		a.trayMgr.SetStartup(a.cfg.StartWithWindows)
	}
}

// OpenSettings opens the unified settings window (on the Accounts section). The
// window's left-sidebar nav lets the user move to Stations / Bar Options from
// there — replacing the old per-feature tray items.
func (a *App) OpenSettings() { a.openSettings("accounts", 0, "") }

// settingsShowPayload tells the popup which panel to render. Index/Name carry
// extra context for context-specific panels; they're 0/"" otherwise.
type settingsShowPayload struct {
	Panel string `json:"panel"`
	Index int    `json:"index"`
	Name  string `json:"name"`
}

// openSettings shows the reusable settings popup focused on the given panel
// ("accounts", "stations", or "options"). The window is its
// own frameless WebviewWindow (the bar itself is only BarHeight tall, with no
// room for a modal). It is created lazily and hidden — not destroyed — on close,
// so reopening preserves page state and is cheap.
//
// The target panel is delivered two ways for robustness: encoded in the URL on
// first creation (the page can't have registered an event listener yet), and
// re-sent via the "settings:show" event for every subsequent open / panel
// switch on the already-loaded page.
func (a *App) openSettings(panel string, index int, name string) {
	if a.app == nil {
		return
	}
	if a.settingsWindow == nil {
		q := "/settings.html?panel=" + panel
		if index != 0 || name != "" {
			q += "&index=" + strconv.Itoa(index) + "&name=" + url.QueryEscape(name)
		}
		a.settingsWindow = a.app.Window.NewWithOptions(application.WebviewWindowOptions{
			Name:             "settings",
			Title:            "Clawd Panel",
			Width:            660,
			Height:           420,
			MinWidth:         520,
			MinHeight:        260,
			Frameless:        true,
			AlwaysOnTop:      true,
			DisableResize:    true,
			Hidden:           true,
			BackgroundColour: application.NewRGB(0x0B, 0x0C, 0x0E),
			URL:              q,
		})
	}
	a.settingsWindow.Show()
	a.settingsWindow.Center()
	a.settingsWindow.Focus()
	a.app.Event.Emit("settings:show", settingsShowPayload{Panel: panel, Index: index, Name: name})
}

func (a *App) Quit() {
	a.app.Quit()
}

// ToggleBrandMenu opens the small dropdown anchored under the ClawdPanel brand
// icon, or closes it if it's already open. Like the settings popup it's a
// separate frameless window — the 28-px bar has no room to draw a menu and clips
// its own overflow. Created lazily, hidden (not destroyed) on close, and
// auto-hidden the moment it loses focus so clicking elsewhere dismisses it.
//
// The toggle has to survive a race: clicking the icon while the menu is open
// first defocuses the menu (auto-hide) and only then delivers the click here, so
// a plain "show if hidden" would always reopen. menuVisible answers "is it up
// right now", and menuHiddenAt lets us recognise the auto-hide that this very
// click just triggered and stay closed.
func (a *App) ToggleBrandMenu() {
	if a.app == nil {
		return
	}
	if a.menuVisible {
		a.hideBrandMenu()
		return
	}
	// The click that brought us here may have just auto-closed the menu via focus
	// loss; treat that as "toggle off" rather than immediately reopening.
	if time.Since(a.menuHiddenAt) < 250*time.Millisecond {
		return
	}
	if a.menuWindow == nil {
		a.menuWindow = a.app.Window.NewWithOptions(application.WebviewWindowOptions{
			Name:             "brand-menu",
			Title:            "",
			Width:            188,
			Height:           64,
			Frameless:        true,
			AlwaysOnTop:      true,
			DisableResize:    true,
			Hidden:           true,
			BackgroundColour: application.NewRGB(0x0B, 0x0C, 0x0E),
			URL:              "/menu.html",
		})
		a.menuWindow.OnWindowEvent(events.Common.WindowLostFocus, func(*application.WindowEvent) {
			// Ignore the transient focus loss that can fire while the window is
			// still coming up; only a genuine click-away after it has settled
			// should dismiss it.
			if a.menuVisible && time.Since(a.menuShownAt) > 300*time.Millisecond {
				a.hideBrandMenu()
			}
		})
	}
	a.menuShownAt = time.Now()
	a.menuVisible = true
	a.menuWindow.Show()
	// Anchor just under the brand icon at the bar monitor's top-left. mon.Left/Top
	// are physical pixels but SetPosition takes DIP (it scales to physical
	// internally), so divide by the monitor's scale; the bar height is already in
	// logical units.
	if len(a.monitors) > 0 {
		idx := a.cfg.Monitor
		if idx < 0 || idx >= len(a.monitors) {
			idx = 0
		}
		mon := a.monitors[idx]
		scale := mon.DpiScale
		if scale <= 0 {
			scale = 1
		}
		x := int(float64(mon.Left)/scale) + 6
		y := int(float64(mon.Top)/scale) + a.cfg.BarHeight
		a.menuWindow.SetPosition(x, y)
	}
	a.menuWindow.Focus()
}

// hideBrandMenu hides the dropdown and records when, so a follow-up toggle click
// can tell an auto-hide it caused from a fresh open request.
func (a *App) hideBrandMenu() {
	if a.menuWindow != nil {
		a.menuWindow.Hide()
	}
	a.menuVisible = false
	a.menuHiddenAt = time.Now()
}

// CloseBrandMenu hides the dropdown — called by the menu page after an action so
// the window object is kept for an instant reopen.
func (a *App) CloseBrandMenu() {
	a.hideBrandMenu()
}

// UpdateCheckResult is returned to the brand menu's "Check for updates" item.
// On failure Error carries a short message; otherwise the frontend compares
// Current/Latest and opens URL when UpdateAvailable.
type UpdateCheckResult struct {
	Current         string `json:"current"`
	Latest          string `json:"latest"`
	UpdateAvailable bool   `json:"updateAvailable"`
	URL             string `json:"url"`
	Changelog       string `json:"changelog"`
	DownloadURL     string `json:"downloadUrl"`
	Error           string `json:"error"`
}

const (
	releasesAPIURL  = "https://api.github.com/repos/Eyalm321/clawdpanel/releases/latest"
	releasesPageURL = "https://github.com/Eyalm321/clawdpanel/releases/latest"
)

func isNewerVersion(latest, current string) bool {
	if current == "dev" {
		return true
	}
	if latest == "" {
		return false
	}
	if latest == current {
		return false
	}

	parseComponents := func(v string) []string {
		v = strings.TrimPrefix(strings.TrimSpace(v), "v")
		v = strings.ReplaceAll(v, "-", ".")
		return strings.Split(v, ".")
	}

	latestParts := parseComponents(latest)
	currentParts := parseComponents(current)

	for i := 0; i < len(latestParts) && i < len(currentParts); i++ {
		lPart := latestParts[i]
		cPart := currentParts[i]

		if lPart == cPart {
			continue
		}

		var lNum, cNum int
		lIsNum := false
		cIsNum := false

		if _, err := fmt.Sscanf(lPart, "%d", &lNum); err == nil {
			lIsNum = true
		}
		if _, err := fmt.Sscanf(cPart, "%d", &cNum); err == nil {
			cIsNum = true
		}

		if lIsNum && cIsNum {
			if lNum != cNum {
				return lNum > cNum
			}
		} else {
			var lRc, cRc int
			lIsRc := false
			cIsRc := false
			if _, err := fmt.Sscanf(strings.TrimPrefix(lPart, "rc"), "%d", &lRc); err == nil {
				lIsRc = true
			}
			if _, err := fmt.Sscanf(strings.TrimPrefix(cPart, "rc"), "%d", &cRc); err == nil {
				cIsRc = true
			}

			if lIsRc && cIsRc {
				if lRc != cRc {
					return lRc > cRc
				}
			} else {
				return lPart > cPart
			}
		}
	}

	return len(latestParts) > len(currentParts)
}

// CheckForUpdates queries the latest GitHub release and compares its tag to the
// running version. Network/parse failures come back in Error rather than as a Go
// error so the menu can always show a friendly line.
func (a *App) CheckForUpdates() UpdateCheckResult {
	res := UpdateCheckResult{Current: strings.TrimPrefix(strings.TrimSpace(Version), "v"), URL: releasesPageURL}

	req, err := http.NewRequest(http.MethodGet, releasesAPIURL, nil)
	if err != nil {
		res.Error = "could not build request"
		return res
	}
	req.Header.Set("Accept", "application/vnd.github+json")
	req.Header.Set("User-Agent", "clawdpanel")

	client := &http.Client{Timeout: 10 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		res.Error = "network unavailable"
		return res
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		res.Error = fmt.Sprintf("server returned status: %d", resp.StatusCode)
		return res
	}

	var payload struct {
		TagName string `json:"tag_name"`
		Body    string `json:"body"`
		Assets  []struct {
			Name               string `json:"name"`
			BrowserDownloadURL string `json:"browser_download_url"`
		} `json:"assets"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		res.Error = "failed to parse server response"
		return res
	}

	res.Changelog = payload.Body

	// Find Windows Setup installer asset
	for _, asset := range payload.Assets {
		lowerName := strings.ToLower(asset.Name)
		if strings.Contains(lowerName, "windows") && strings.HasSuffix(lowerName, ".exe") {
			res.DownloadURL = asset.BrowserDownloadURL
			break
		}
	}
	// Fallback to first .exe asset if windows is not explicitly in the name
	if res.DownloadURL == "" {
		for _, asset := range payload.Assets {
			if strings.HasSuffix(strings.ToLower(asset.Name), ".exe") {
				res.DownloadURL = asset.BrowserDownloadURL
				break
			}
		}
	}

	res.Latest = strings.TrimPrefix(strings.TrimSpace(payload.TagName), "v")
	cur := strings.TrimPrefix(strings.TrimSpace(Version), "v")
	res.UpdateAvailable = isNewerVersion(res.Latest, cur)

	a.lastUpdateResult = res
	if res.UpdateAvailable {
		go a.OpenUpdateWindow()
	}

	return res
}

func (a *App) GetLastUpdateResult() UpdateCheckResult {
	return a.lastUpdateResult
}

func (a *App) OpenUpdateWindow() {
	if a.app == nil {
		return
	}
	if a.updateWindow == nil {
		a.updateWindow = a.app.Window.NewWithOptions(application.WebviewWindowOptions{
			Name:             "update",
			Title:            "System Update",
			Width:            520,
			Height:           380,
			MinWidth:         400,
			MinHeight:        280,
			Frameless:        true,
			AlwaysOnTop:      true,
			DisableResize:    true,
			Hidden:           true,
			BackgroundColour: application.NewRGB(0x0B, 0x0C, 0x0E),
			URL:              "/update.html",
		})
	}
	a.updateWindow.Show()
	a.updateWindow.Center()
	a.updateWindow.Focus()
}

type progressWriter struct {
	total      int64
	downloaded int64
	onProgress func(percent float64, downloadedMB float64, totalMB float64)
}

func (pw *progressWriter) Write(p []byte) (int, error) {
	n := len(p)
	pw.downloaded += int64(n)

	var pct float64
	if pw.total > 0 {
		pct = float64(pw.downloaded) / float64(pw.total) * 100.0
	}

	pw.onProgress(pct, float64(pw.downloaded)/(1024*1024), float64(pw.total)/(1024*1024))
	return n, nil
}

func (a *App) InstallUpdate(downloadURL string) error {
	log.Printf("[Updater] Starting seamless update download from: %s", downloadURL)

	tempDir := os.TempDir()
	tempInstallerPath := filepath.Join(tempDir, "ClawdPanel-setup-temp.exe")

	resp, err := http.Get(downloadURL)
	if err != nil {
		return fmt.Errorf("download failed: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("server returned status: %d", resp.StatusCode)
	}

	totalSize := resp.ContentLength

	out, err := os.Create(tempInstallerPath)
	if err != nil {
		return fmt.Errorf("failed to create temp file: %w", err)
	}
	defer out.Close()

	pw := &progressWriter{
		total: totalSize,
		onProgress: func(percent float64, downloadedMB float64, totalMB float64) {
			a.app.Event.Emit("update:progress", map[string]interface{}{
				"percent":    percent,
				"downloaded": downloadedMB,
				"total":      totalMB,
			})
		},
	}

	// Copy resp.Body through both progressWriter and the file
	_, err = io.Copy(out, io.TeeReader(resp.Body, pw))
	if err != nil {
		return fmt.Errorf("failed to save download: %w", err)
	}
	out.Close()

	log.Printf("[Updater] Download complete. Spawning silent background updater...")

	appPath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("failed to get executable path: %w", err)
	}

	appPath = resolveRelaunchPath(appPath)

	err = runSilentInstaller(tempInstallerPath, appPath)
	if err != nil {
		return fmt.Errorf("failed to run silent installer: %w", err)
	}

	log.Printf("[Updater] Background updater started. Exiting application...")
	os.Exit(0)
	return nil
}

// ── Wails-exported bindings ──────────────────────────────────────────────────

func (a *App) GetBarData() (*claude.BarData, error) {
	if len(a.cfg.Accounts) == 0 {
		return nil, fmt.Errorf("no accounts configured")
	}

	activeIdx := a.cfg.ActiveAccount
	if activeIdx >= len(a.cfg.Accounts) {
		activeIdx = 0
	}

	acc := a.cfg.Accounts[activeIdx]

	return claude.LoadBarData(acc.Path, acc.Name)
}

func (a *App) GetConfig() config.Config {
	return *a.cfg
}

func (a *App) SaveConfig(cfg config.Config) error {
	prevMonitor := a.cfg.Monitor
	prevAppBar := a.cfg.AppBarMode
	prevRadio := a.cfg.Features.Radio
	a.cfg = &cfg
	if err := config.Save(a.cfg); err != nil {
		return err
	}
	// Bring the radio "resource dependency" up/down to match the toggle: enabling
	// spawns the native audio engine, disabling tears it down and frees the
	// background player process (not just hides the segment).
	if cfg.Features.Radio != prevRadio {
		if cfg.Features.Radio {
			a.initAudio()
		} else {
			a.teardownAudio()
		}
	}
	if st := a.getStation(); st != nil {
		st.SetStations(cfg.Stations)
	}
	if a.hwnd != 0 {
		if cfg.Monitor != prevMonitor || cfg.AppBarMode != prevAppBar {
			if prevAppBar {
				platform.RemoveAppBar(a.hwnd)
			}
			a.monitors = platform.GetMonitors()
			if cfg.Monitor < len(a.monitors) {
				platform.DockToMonitor(a.hwnd, a.monitors[cfg.Monitor], cfg.BarHeight, cfg.AppBarMode)
			}
		}
		platform.SetOpacity(a.hwnd, cfg.Opacity)

		if cfg.AppBarMode && cfg.Pinned {
			go func() {
				if err := platform.PushdownEnable(a.monitors[cfg.Monitor], cfg.BarHeight); err != nil {
					log.Printf("[pushdown] Enable failed: %v", err)
				}
			}()
		} else {
			platform.PushdownDisable()
		}
		// Refresh the reveal controller's snapshot so the slide + click-through
		// pick up any new bar height / monitor / pin / click-through setting.
		if a.revealCtrl != nil && a.cfg.Monitor < len(a.monitors) {
			a.revealCtrl.Configure(a.monitors[a.cfg.Monitor], a.cfg.BarHeight, a.cfg.Pinned, a.cfg.ClickThrough)
		}
	}
	a.app.Event.Emit("config:changed")
	return nil
}

func (a *App) GetMonitors() []platform.MonitorInfo {
	a.monitors = platform.GetMonitors()
	return a.monitors
}

func (a *App) SetActiveAccount(index int) error {
	if index < 0 || index >= len(a.cfg.Accounts) {
		return fmt.Errorf("account index %d out of range", index)
	}
	a.cfg.ActiveAccount = index
	if err := config.Save(a.cfg); err != nil {
		return err
	}
	if a.trayMgr != nil {
		a.trayMgr.SetAccountChecked(index)
	}
	a.app.Event.Emit("account:changed", index)
	return nil
}

func (a *App) SetMonitor(index int) error {
	a.monitors = platform.GetMonitors()
	if index < 0 || index >= len(a.monitors) {
		return fmt.Errorf("monitor index %d out of range", index)
	}
	if a.hwnd != 0 && a.cfg.AppBarMode {
		platform.RemoveAppBar(a.hwnd)
	}
	a.cfg.Monitor = index
	if err := config.Save(a.cfg); err != nil {
		return err
	}
	if a.hwnd != 0 {
		platform.DockToMonitor(a.hwnd, a.monitors[index], a.cfg.BarHeight, a.cfg.AppBarMode)
		platform.PushdownReconfigure(a.monitors[index], a.cfg.BarHeight)
	}
	if a.trayMgr != nil {
		a.trayMgr.SetMonitorChecked(index)
	}
	a.app.Event.Emit("monitor:changed", index)
	return nil
}

func (a *App) ToggleClickThrough() bool {
	a.cfg.ClickThrough = !a.cfg.ClickThrough
	if a.revealCtrl != nil {
		a.revealCtrl.SetUserClickThrough(a.cfg.ClickThrough)
	}
	_ = config.Save(a.cfg)
	return a.cfg.ClickThrough
}

func (a *App) SetOpacity(opacity float64) error {
	a.cfg.Opacity = opacity
	if a.hwnd != 0 {
		platform.SetOpacity(a.hwnd, opacity)
	}
	return config.Save(a.cfg)
}

func (a *App) GetVersion() string {
	return Version
}

// RadioPlayStation starts (or resumes) the configured station at index. The
// station player owns the queue, shuffle, auto-advance and looping; it drives
// the single-track audio controller one track at a time.
func (a *App) RadioPlayStation(index int) error {
	st := a.getStation()
	if st == nil {
		return fmt.Errorf("radio is disabled")
	}
	return st.Play(index)
}

// ParseStationItem classifies a single URL/ID into a StationItem for the
// stations editor (so URL parsing stays authoritative on the Go side).
func (a *App) ParseStationItem(input string) (config.StationItem, error) {
	return station.ParseItem(input)
}

// SetActiveStation persists which station is selected in the bar cycler. It
// does not start playback (use RadioPlayStation for that).
func (a *App) SetActiveStation(index int) error {
	if index < 0 || index >= len(a.cfg.Stations) {
		return fmt.Errorf("station index %d out of range", index)
	}
	a.cfg.ActiveStation = index
	return config.Save(a.cfg)
}

func (a *App) RadioPause() error {
	st := a.getStation()
	if st == nil {
		return fmt.Errorf("radio is disabled")
	}
	return st.Pause()
}

// RadioNext skips to the next track in the active station's queue. It's a no-op
// when the radio is disabled or nothing has been queued yet (e.g. never played).
// The bar's › track button drives this; it's grayed out for single-track and
// livestream stations where there's nothing to step through.
func (a *App) RadioNext() error {
	st := a.getStation()
	if st == nil {
		return fmt.Errorf("radio is disabled")
	}
	return st.Next()
}

// RadioPrev steps back to the previous track in the active station's queue. It's
// a no-op when the radio is disabled or nothing has been queued yet. The bar's ‹
// track button drives this.
func (a *App) RadioPrev() error {
	st := a.getStation()
	if st == nil {
		return fmt.Errorf("radio is disabled")
	}
	return st.Prev()
}

// RadioStationHasTracks reports whether the station at index has more than one
// track to step through, so the bar can enable/gray-out its ‹ › track buttons.
// It's derived from config alone (no playback or network) and recognises
// playlists even when an item's saved kind is a stale "video" hint — e.g. a
// watch?v=…&list=… URL. Out-of-range indexes return false.
func (a *App) RadioStationHasTracks(index int) bool {
	if index < 0 || index >= len(a.cfg.Stations) {
		return false
	}
	return station.HasMultipleTracks(a.cfg.Stations[index])
}

// RadioSetShuffle toggles shuffle mode for the station at index, persists it to
// config, and applies it live to the station engine. It is a pure mode toggle:
// it changes only the random-order setting and never starts or jumps playback,
// so toggling while paused stays paused. The bar's shuffle button drives this
// (the setting was removed from the stations editor).
func (a *App) RadioSetShuffle(index int, on bool) error {
	if index < 0 || index >= len(a.cfg.Stations) {
		return fmt.Errorf("station index %d out of range", index)
	}
	a.cfg.Stations[index].Shuffle = on
	if err := config.Save(a.cfg); err != nil {
		return err
	}
	if st := a.getStation(); st != nil {
		st.SetStations(a.cfg.Stations)
		return st.SetShuffle(index, on)
	}
	return nil
}

// RadioSeek jumps the currently-playing track to the given offset in seconds.
// Driven by the bar's seek timeline. No-op when radio is disabled or nothing is
// loaded; ignored for livestreams by the underlying player.
func (a *App) RadioSeek(seconds float64) error {
	a.audioMu.Lock()
	ctrl := a.audioCtrl
	a.audioMu.Unlock()
	if ctrl == nil {
		return fmt.Errorf("radio is disabled")
	}
	return ctrl.Seek(seconds)
}

func (a *App) RadioSetVolume(v float64) error {
	// Persist the chosen volume so it survives restarts (replaces the prior
	// localStorage-only value), even while the engine is down — so a later
	// re-enable picks it up.
	a.cfg.RadioVolume = v
	_ = config.Save(a.cfg)
	st := a.getStation()
	if st == nil {
		return fmt.Errorf("radio is disabled")
	}
	return st.SetVolume(v)
}

func (a *App) SetPinned(pinned bool) error {
	a.cfg.Pinned = pinned
	a.cfg.AppBarMode = pinned
	if err := config.Save(a.cfg); err != nil {
		return err
	}
	if a.hwnd != 0 && len(a.monitors) > 0 {
		platform.RemoveAppBar(a.hwnd)
		platform.DockToMonitor(a.hwnd, a.monitors[a.cfg.Monitor], a.cfg.BarHeight, pinned)
		if pinned {
			go func() {
				if err := platform.PushdownEnable(a.monitors[a.cfg.Monitor], a.cfg.BarHeight); err != nil {
					log.Printf("[pushdown] Enable failed: %v", err)
				}
			}()
		} else {
			platform.PushdownDisable()
		}
		a.revealCtrl.SetPinned(pinned)
	}
	a.app.Event.Emit("pinned:changed", pinned)
	return nil
}

// SetEditorOpen forces the bar fully expanded while the inline accounts editor
// is shown (the editor is launched from the tray with the cursor off-bar, and
// must stay open until dismissed). On close, the controller re-evaluates based
// on the current cursor position.
func (a *App) SetEditorOpen(open bool) {
	if a.revealCtrl != nil {
		a.revealCtrl.SetEditorOpen(open)
	}
}

// GetPushdownStats returns active diagnostics for macOS window pushdown.
func (a *App) GetPushdownStats() platform.PushdownStats {
	return platform.GetPushdownStats()
}

func (a *App) watchClaudeStatus(ctx context.Context) {
	ticker := time.NewTicker(500 * time.Millisecond)
	defer ticker.Stop()

	var lastStatus string
	var lastPath string

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			// Read the active account config
			a.audioMu.Lock()
			cfg := a.cfg
			a.audioMu.Unlock()

			if cfg == nil || len(cfg.Accounts) == 0 {
				continue
			}

			activeIdx := cfg.ActiveAccount
			if activeIdx < 0 || activeIdx >= len(cfg.Accounts) {
				activeIdx = 0
			}
			acc := cfg.Accounts[activeIdx]

			status := claude.GetStatus(acc.Path)
			if status != lastStatus || acc.Path != lastPath {
				lastStatus = status
				lastPath = acc.Path
				if a.app != nil {
					a.app.Event.Emit("claude:status", status)
				}
			}
		}
	}
}
