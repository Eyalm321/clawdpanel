# Rework slice: shell-platform (App shell + OS integration + build → Rust/Slint)

Owner of: process lifecycle, the 4 windows, OS docking/monitors/tray/autostart,
the auto-updater, asset embedding, the build/packaging pipeline, **and the
top-level cargo workspace map that ties all 4 slices together**.

---

## Scope

Existing files/symbols this slice covers:

- **Bootstrap / binding surface:** `main.go` (Wails options, single-instance,
  XWayland forcing, the one main window), `app.go` (the `App` struct — every
  Wails-exported method + every `Event.Emit` channel; the lazy
  settings/menu/update windows; the brand-menu focus dance).
- **Platform layer** `internal/platform/`:
  - `types.go` — `MonitorInfo`, `PushdownStats`.
  - `window_{windows,linux,darwin}.go` — `ApplyBarStyles`, `DockToMonitor`,
    `RemoveAppBar`, `SetOpacity`, `SetClickThrough`, `MoveWindow`,
    `Show/HideWindow`, `SetWindowClipTop`, `GetCursorPos`, `GetWindowSize`,
    `SetWindowHeight`, `IsFullScreenActive`, `AutoHideSupported`,
    `FindWindowByPID`, `ResetDwmFrame`.
  - `monitor_{windows,linux,darwin}.go` — `GetMonitors` + (linux) strut/dock-edge
    logic (`gnomePanelOffset`, `pickDockEdge`, `edgeReservable`,
    `parseXrandrGeometry`, `rootGeometry`).
  - `pushdown_darwin.go` (~800 lines ObjC) + `pushdown_other.go` — macOS AX
    window pushdown (`PushdownEnable/Disable/Reconfigure`, `AXTrusted`,
    `AXRequestTrust`, `GetPushdownStats`).
  - `fullscreen_darwin.go` — NSWorkspace + `AXFullScreen` watcher.
- **Reveal state machine** `internal/reveal/reveal.go` — slide animation, hover
  hit-test, grace timer, fullscreen/pin/editor precedence, click-through. Already
  OS-decoupled behind the `WindowOps` interface (ports almost verbatim).
- **Tray** `internal/tray/tray.go` (`Manager`, `Controller`).
- **Config + autostart** `internal/config/config.go` (`Config`, `AppDataDir`,
  `Load/Save/Defaults`), `startup_{windows,linux,darwin}.go`
  (`SetStartOnLogin`/`IsStartOnLogin`).
- **Updater** `updater_{windows,linux,darwin}.go` (`selectUpdateAsset`,
  `installFlavor`, `runSilentInstaller`, `resolveRelaunchPath`,
  `spawnDetached`/AppImage swap) + the `App.CheckForUpdates`/`InstallUpdate`/
  `OpenUpdateWindow`/`isNewerVersion`/`progressWriter` glue in `app.go`;
  `frontend/update.html` + `src/update.js` (the update window UI).
- **Icons** `icon_{windows,linux,darwin}.go` (`go:embed` of the tray icon).
- **Build/packaging:** `Taskfile.yml` + `linux/Taskfile.yml` (+ `darwin/`,
  `windows/`, `android/`, `ios/` Taskfiles), `build/config.yml`,
  `build/{darwin,linux,windows}/*`, `build/linux/{nfpm.yaml,build-appimage.sh,
  AppRun,*.desktop}`, `.github/workflows/release.yml`, `docker/Dockerfile.*`.

Out of scope (other slices): bar UI/CSS (`index.html`, `main.js`, `style.css`),
settings panels, claude usage logic, audio/radio engine. The `frontend/menu.html`
and `update.html` content is UI; this slice owns the *windows that host them* and
the Rust side of their callbacks.

---

## Current behavior

### Process model & windows
- Single Wails `application.App`, **accessory/agent app** (no dock icon):
  `Mac.ActivationPolicy = Accessory`, `LSUIElement=true` in `Info.plist`.
- **Single-instance** via `SingleInstanceOptions{UniqueID:"com.clawdpanel.app"}`;
  a 2nd launch fires `OnSecondInstanceLaunch → app.reveal()` (slides the bar back
  on-screen) then the 2nd process exits — never two bars/two trays.
- **4 frameless, always-on-top, opaque (`#0B0C0E`), `Hidden:true` windows**, all
  `DisableResize` (except the main bar on Linux, where fixed-size WM hints would
  block the wmctrl dock resize):
  1. **Main bar** — `1920 × cfg.BarHeight` (default 28), `MinHeight:1`,
     `MaxHeight:0`. Created eagerly in `main.go`.
  2. **Settings** — `660×420` (min `520×260`), URL `/settings.html?panel=…`.
  3. **Brand menu** — `188×64`, URL `/menu.html`, auto-hides on focus loss
     (guarded by `menuShownAt`/`menuHiddenAt` timing to survive the click→defocus
     →click race).
  4. **Update** — `520×380`, URL `/update.html`.
  Windows 2–4 are created **lazily on first use and hidden, not destroyed**, so
  reopening preserves state.
- Startup order (`app.startup` → `domReady`): a 300ms settle, then resolve the
  native handle, build `reveal.Controller`, `ApplyBarStyles`, `GetMonitors`,
  `DockToMonitor` (+ macOS pushdown), `SetOpacity`, `revealCtrl.Configure/Init`,
  `runTray`, start the cursor poll loop + the 500ms claude-status poller.
  `domReadyOnce` guards a double `WindowRuntimeReady` (Windows WebView2).
- Config is **force-overridden on launch** (`NewApp`): `Pinned=true`,
  `AppBarMode=true`, `Opacity=1.0` regardless of saved values (always start docked
  & opaque).

### Binding surface (Wails → frontend)
Exported `App` methods become JS bindings; the app also emits events. Both must be
reproduced as the Slint bridge:

- **Methods:** `GetBarData`, `GetConfig`, `SaveConfig`, `GetMonitors`,
  `SetActiveAccount`, `SetMonitor`, `SetPinned`, `ToggleClickThrough`,
  `SetOpacity`, `SetEditorOpen`, `GetVersion`, `GetPushdownStats`,
  `OpenSettings`, `ToggleBrandMenu`, `CloseBrandMenu`, `Quit`, `ToggleStartup`,
  `CheckForUpdates`, `GetLastUpdateResult`, `OpenUpdateWindow`, `InstallUpdate`,
  and the radio set (`RadioPlayStation/Pause/Next/Prev/Seek/SetVolume/SetShuffle`,
  `RadioStationHasTracks`, `SetActiveStation`, `ParseStationItem`).
- **Events (`a.app.Event.Emit`):** `radio:state`, `settings:show`,
  `update:progress`, `config:changed`, `account:changed`, `monitor:changed`,
  `claude:status`, `pinned:changed`. Consumed in `main.js`, `settings.js`,
  `update.js`.

### Platform splits (the hard part)
`MonitorInfo` is the shared currency (`json` fields cross to JS). Key subtlety:
**Win/Linux report physical px; macOS reports points** in `PhysWidth` so the
shared `app.go`/`reveal.go` math works on Retina without OS checks. `WorkTopOffset`
= macOS menu-bar height (Win/Linux 0). `DockEdge` = "top"/"bottom" (Linux only).

| Concern | Windows | Linux (X11/XWayland) | macOS |
|---|---|---|---|
| Native handle | HWND via `EnumWindows`+PID; window itself topmost-styled | `NativeWindow()` returns a **GTK ptr**, so resolve real X id by `wmctrl -lp` PID match (`FindWindowByPID`) | `NativeWindow()` is the NSWindow directly (`FindWindowByPID` no-op) |
| Frameless/topmost | strip `WS_CAPTION/THICKFRAME/…`, add `WS_EX_TOOLWINDOW|WS_EX_LAYERED`, drop `WS_EX_APPWINDOW` | `xprop _NET_WM_WINDOW_TYPE=_DOCK` + `wmctrl add,above` | `NSWindowStyleMaskBorderless`, `NSStatusWindowLevel`, `CanJoinAllSpaces|Stationary`, no shadow, not movable |
| Reserve screen strip | `SHAppBarMessage` ABM_NEW/QUERYPOS/SETPOS, edge top | `_NET_WM_STRUT_PARTIAL` (12 cardinals) + `wmctrl -e` geom; only when `edgeReservable` (struts measure from root edge → multi-monitor aware via `DockEdge`) | **window pushdown**: AX API moves overlapping windows down (`pushdown_darwin.go`) — there is no AppBar |
| Monitors | `EnumDisplayMonitors`+`GetMonitorInfoW`+`GetDpiForMonitor` | parse `xrandr --listmonitors` (+ gnome 32px panel offset) | `NSScreen.screens`, `frame`/`visibleFrame`/`backingScaleFactor`; menu-bar height per screen |
| Opacity | `SetLayeredWindowAttributes` | `_NET_WM_WINDOW_OPACITY` via xprop | `setAlphaValue` |
| Click-through | toggle `WS_EX_TRANSPARENT` | **no-op** (needs XShape) | `setIgnoresMouseEvents` |
| Cursor poll | `GetCursorPos` | `xdotool getmouselocation` (subprocess, cached) | `NSEvent.mouseLocation` (flip Y) |
| Slide move/show/hide | `SetWindowPos` / `ShowWindow` / `SetWindowRgn` clip-top (multi-mon spill mask) | `xdotool windowmove`/`windowmap`/`windowunmap`; clip = no-op | `setFrameOrigin`/`orderFront`/`orderOut`; clip = no-op |
| Fullscreen detect | foreground win: not shell, no `WS_CAPTION`, covers its monitor, == bar's monitor | `_NET_ACTIVE_WINDOW`+`_NET_WM_STATE_FULLSCREEN` (xprop, cached 1s) | NSWorkspace space-change observer reads `AXFullScreen` of frontmost app |
| DWM hack | `ResetDwmFrame` (kills Wails' frame-extension chrome strip) | n/a | n/a |

The reveal machine (`reveal.go`) sits **above** all of this behind `WindowOps`
(WindowRect/MoveTo/ClipTop/Show/Hide/SetClickThrough/CursorPos/FullScreenActive/
AutoHideSupported). Precedence each `Tick` (80ms poll): fullscreen → collapse;
else pinned/editor → expand; else follow cursor with a 200ms grace collapse.
Slide = ease-out-cubic over 200ms moving the OS window's top edge, re-clipping the
above-monitor spill each 16ms frame; a `generation` counter supersedes in-flight
slides. macOS/Win/Linux all report `AutoHideSupported()==true`.

### Tray, autostart, updater
- **Tray** (`tray.go`): icon from raw PNG bytes (NOT a template icon — the brand
  icon is a solid colored square that would collapse to a blob if tinted); menu =
  title (disabled) / radio per account / radio per monitor / "Start on login"
  checkbox / "Settings…" / "Quit". `SetChecked` updates on account/monitor/startup
  changes. Rebuilt in `runTray`.
- **Icon embedding** (`icon_*.go`): `go:embed build/appicon.png` (mac/win) or
  `build/linux/icon.png` (linux). Frontend embedded via `//go:embed all:frontend/dist`.
- **Autostart** (`startup_*.go`): Win = HKCU `…\Run` registry value `ClawdPanel`;
  Linux = `~/.config/autostart/clawdpanel.desktop`; mac = `~/Library/LaunchAgents/
  com.clawdpanel.app.plist` + `launchctl load -w`.
- **Updater**: `CheckForUpdates` GETs the GitHub `releases/latest` API, parses
  tag/body/assets, runs `isNewerVersion` (custom dotted + `rc` comparator; `dev`
  always "newer"), `selectUpdateAsset` picks the per-OS/flavor asset, caches into
  `lastUpdateResult`, and (if newer) auto-opens the update window. `InstallUpdate`
  streams the download with a `progressWriter` emitting `update:progress`, then
  `runSilentInstaller` spawns a **detached** installer and `os.Exit(0)`:
  - Win: hidden PowerShell — stop process, run `.exe /S` elevated, relaunch.
  - Linux: AppImage = atomic file swap + relaunch; rpm/deb = `pkexec` install;
    else (manual/dev) = no in-place, open releases page. `installFlavor` detects
    via `$APPIMAGE`/`rpm -qf`/`dpkg -S`.
  - mac: no silent install — update window opens the releases page.

### Build/packaging (today)
- **Wails v3 + Task**: root `Taskfile.yml` delegates to per-OS Taskfiles which
  call `wails3 task build/package`. Frontend = Vite build → `frontend/dist` →
  `go:embed`. Bindings generated by `wails3 generate bindings`. CGO required on
  Linux (GTK4 + WebKitGTK 6.0) & macOS (Cocoa/AX); Docker cross-image for Linux.
- **CI** (`release.yml`): matrix win/mac/linux on tag `v*`. Installs Go (from
  `go.mod`, currently 1.26), Node 18, Wails `v3.0.0-alpha.96`, Linux GTK/WebKit/
  GStreamer dev libs + NSIS/nfpm/appimagetool. Produces: Windows NSIS `.exe`,
  macOS universal `.app`→`.pkg` (`pkgbuild`), Linux `.deb`+`.rpm` (nfpm) +
  `.AppImage`. Release job flattens artifacts → `softprops/action-gh-release`.
- `HotkeyConfig` exists in config but **is never registered** (vestigial — no
  global-shortcut wiring exists).
- The `server`/`docker` Taskfile targets are unused scaffold (no `//go:build
  server` files) — ignore for the rework.

---

## Rust/Slint design

### Workspace / crate map (this slice owns the top level)

```
clawdpanel/                       # cargo workspace
├─ Cargo.toml                     # [workspace] members
├─ crates/
│  ├─ app/            (bin)       # main.rs: event loop, single-instance, wires slices
│  ├─ types/          (lib)       # Config, MonitorInfo, BarData, StationConfig … (no deps; breaks cycles)
│  ├─ platform-shell/ (lib)       # ← THIS SLICE
│  ├─ ui/             (lib)       # .slint + slint-build, globals bridge   (UI slice)
│  ├─ claude-core/    (lib)       # config load/save, usage/stats/api      (claude slice)
│  └─ media/          (lib)       # audio engine, radio resolver, stations (media slice)
```

`platform-shell` internal modules (mirror the Go layout, `#[cfg(target_os=…)]`):

```
platform-shell/src/
├─ lib.rs              # public API: Shell, WindowOps trait, MonitorInfo re-export
├─ window/{windows,linux,macos}.rs   # ApplyBarStyles/Dock/Opacity/ClickThrough/Move/Show/Hide/Clip
├─ monitor/{windows,linux,macos}.rs  # GetMonitors (+ linux strut/edge logic ported verbatim)
├─ dock/                              # AppBar (win) / struts (linux) / pushdown (macos)
├─ fullscreen/{windows,linux,macos}.rs
├─ reveal.rs          # 1:1 port of reveal.go (WindowOps trait, tokio task or std thread)
├─ tray.rs            # tray-icon + muda menu
├─ autostart.rs       # per-OS, or auto-launch crate
├─ updater/           # check + flavor select + install + isNewerVersion port
└─ single_instance.rs # lock + reveal-ping IPC
```

**Why this split:** it preserves the existing module boundaries (so the porting is
file-for-file), keeps `platform-shell` free of UI deps (testable headless — the
`WindowOps` seam already proves this works), and a leaf `types` crate lets `ui`,
`claude-core`, and `platform-shell` share `Config`/`MonitorInfo` without cycles.

### Windowing & the 4 windows (Slint)
- Backend: **Slint with the winit backend** (`i-slint-backend-winit`) so we can
  reach `winit::Window` for the attributes Slint markup can't express
  (always-on-top, skip-taskbar, X11 dock type, raw handle).
- Each window is one Slint component (`MainBar`, `Settings`, `BrandMenu`,
  `Update`). `MainBar` created at startup; the other three created lazily and
  `hide()`-on-close (Slint keeps the component alive → state preserved, matching
  "hidden not destroyed").
- Window attributes ↔ Wails options:
  | Wails `WebviewWindowOptions` | Slint/winit |
  |---|---|
  | `Frameless` | `WindowAttributes::with_decorations(false)` |
  | `AlwaysOnTop` | `with_window_level(WindowLevel::AlwaysOnTop)` |
  | `DisableResize` | `with_resizable(false)` |
  | `Hidden:true` | `with_visible(false)` (then `Init` decides) |
  | `BackgroundColour 0x0B0C0E` | opaque Slint window `background` |
  | `Width/Height/Min*` | `with_inner_size` / `with_min_inner_size` |
  | `DevToolsEnabled` | drop (native; use `SLINT_DEBUG_PERFORMANCE` if needed) |
- **Native handle**: `window.window_handle()` (`raw-window-handle`) →
  `Win32WindowHandle.hwnd` / `XlibWindowHandle.window` / `AppKitWindowHandle.ns_view`
  → `[ns_view window]`. **Win:** Linux now gets the X window id directly — the
  `wmctrl -lp` PID lookup (`FindWindowByPID`) disappears (a fidelity *improvement*,
  less fragile).
- Move/show/hide for the slide can mostly use Slint's own
  `window().set_position(PhysicalPosition)` / `hide()` / `show()`; only clip-top
  (Windows `SetWindowRgn`) and click-through stay raw.

### Per-OS native calls (replace the subprocess/cgo zoo)
- **Windows** — `windows` crate (or `windows-sys`): `SetWindowLongPtrW`/
  `SetWindowPos`/`SetLayeredWindowAttributes`/`SHAppBarMessage`/`SetWindowRgn`/
  `EnumDisplayMonitors`/`GetMonitorInfoW`/`GetDpiForMonitor`/`GetCursorPos`/
  `GetForegroundWindow`/`DwmExtendFrameIntoClientArea`. Near-mechanical port of
  `window_windows.go`. `ResetDwmFrame` is likely **droppable** (it fought Wails'
  transparent-frame extension; an opaque Slint window shouldn't need it — verify).
- **Linux** — **`x11rb`** replaces all `wmctrl`/`xprop`/`xdotool` subprocesses:
  - dock hint/above: `ChangeProperty` on `_NET_WM_WINDOW_TYPE`(=`_DOCK`) + a
    `_NET_WM_STATE` client message (`_NET_WM_STATE_ABOVE`).
  - strut: `ChangeProperty` `_NET_WM_STRUT_PARTIAL` (the same 12-cardinal layout
    + `DockEdge`/`edgeReservable` logic, ported verbatim).
  - geometry/move: `ConfigureWindow`. map/unmap: `MapWindow`/`UnmapWindow`.
  - opacity: `_NET_WM_WINDOW_OPACITY`. cursor: `QueryPointer` on root.
  - monitors: RandR `GetMonitors` (or keep parsing as a fallback). Port
    `gnomePanelOffset`/`pickDockEdge` unchanged.
  - fullscreen: `GetProperty` `_NET_ACTIVE_WINDOW` + `_NET_WM_STATE`.
  - click-through: was a no-op; can **optionally** implement now with the
    XFixes/Shape input region (`x11rb` has both) — but match v1 and keep no-op to
    avoid scope creep.
- **macOS** — `objc2` + `objc2-app-kit` + `objc2-application-services`:
  `setStyleMask`/`setLevel:NSStatusWindowLevel`/`setCollectionBehavior`/
  `setAlphaValue`/`setIgnoresMouseEvents`/`setFrame`/`orderFront`/`orderOut`;
  `NSScreen` enumeration; `NSEvent.mouseLocation`. The main-thread requirement
  (`runOnMain`) maps to `dispatch`/Slint's `invoke_from_event_loop` (Slint UI is
  already main-thread). Accessory policy = `NSApp.setActivationPolicy(.accessory)`
  (plus `LSUIElement` in the bundle).
  - **Pushdown** (`pushdown_darwin.go`, the AX window-mover): the riskiest port.
    **Recommended: compile the existing `.m` source via the `cc` crate and FFI to
    it** (keep the proven throttling/observer/permission logic byte-for-byte),
    rather than re-deriving ~800 lines of AX bookkeeping in `objc2`. A pure-Rust
    port is possible but L/XL and behavior-risky.
  - fullscreen watcher: `objc2` NSWorkspace observer + `AXUIElementCopyAttribute`
    `AXFullScreen` — direct port of `fullscreen_darwin.go`.

### Reveal machine
`reveal.go` is already pure logic over the `WindowOps` interface → port to a Rust
trait of the same name. Production impl forwards to `platform-shell`; tests inject
a fake clock+cursor exactly like the Go tests (`machine_test.go`,`reveal_test.go`)
— keep that test parity. The 80ms poll = a `tokio` interval task (or `std` thread)
calling `tick()`; UI-affecting calls hop to the Slint loop via
`slint::invoke_from_event_loop`. Ease-out-cubic + generation-supersede logic is
unchanged.

### Single instance
`single-instance` crate for the lock (`com.clawdpanel.app`) + a tiny IPC channel
for the reveal-ping (Unix domain socket / Windows named pipe; or
`interprocess::local_socket` cross-platform). 2nd process: fail lock → connect →
send "reveal" → exit; 1st process: accept → `reveal()`. Mirrors
`OnSecondInstanceLaunch`.

### XWayland / X11 decision
The whole Linux dock path is X11-only (same as today — Wayland gives no
self-positioning/always-on-top/struts on GNOME). **Force the X11 backend** so we
run as an XWayland client under Wayland sessions: set `WINIT_UNIX_BACKEND=x11`
early in `main` (the Rust analogue of `GDK_BACKEND=x11`) and ensure winit is built
with X11 enabled. Keep the same `CLAWDPANEL_NO_XWAYLAND` escape hatch.

### Tray
`tray-icon` (+ `muda` for the menu) — the Tauri tray stack, cross-platform, driven
from the winit/Slint event loop. Icon from decoded RGBA (`image` crate on the
embedded PNG — **not** a template icon, preserving the colored brand square).
Menu: title (disabled) / per-account + per-monitor radio (`muda` check items in a
mutually-exclusive group, rebuilt when accounts change like `runTray`) /
"Start on login" check / "Settings…" / "Quit", with `set_checked` for the
account/monitor/startup updates. On Linux `tray-icon` uses the StatusNotifierItem/
AppIndicator host — same dependency story as today (keep the
`gnome-shell-extension-appindicator` recommend).

### Autostart
`auto-launch` crate covers all three OSes; but to match the exact artifacts
(registry value name `ClawdPanel`, desktop file name/keys, plist label
`com.clawdpanel.app` + `launchctl load -w`) a hand-rolled port of `startup_*.go`
is safer for 1:1 fidelity. **Recommend hand-roll** (small, fully matches today).

### Auto-updater
Custom module (the flavor logic is too app-specific for `self_update`):
- HTTP: `reqwest` (GitHub API + streamed download) + `serde`/`serde_json`.
- `is_newer_version` ported verbatim (dotted + `rc` comparator, `dev` always
  newer). `select_update_asset` + `install_flavor` per-OS exactly as
  `updater_*.go`.
- Progress: stream chunks, push percent/MB to a Slint property (replaces the
  `update:progress` event) so the update window's bar updates.
- Install: spawn detached then exit. Detach via `CREATE_NO_WINDOW` (windows
  crate) / `setsid` (`nix` or `libc` on unix). AppImage swap, `pkexec` rpm/deb,
  PowerShell `/S` — direct ports.
- Update window = the `Update` Slint component (ports `update.html`/`update.js`).

### Asset embedding
- UI: `go:embed all:frontend/dist` **disappears** — Slint compiles `.slint` into
  the binary via `slint-build` in `ui/build.rs`. No bundled HTML/CSS/JS.
- Icons: `include_bytes!("../../build/appicon.png")` (or `rust-embed` for a set);
  decode with `image` for the tray. App/installer icons stay as today's
  `build/**` assets.

### Binding bridge (Wails bindings/events → Slint globals)
Define a Slint `global Backend` with one `callback` per exported `App` method and
one `property`/`callback` per event, handled in Rust:
- Methods → `app.global::<Backend>().on_save_config(...)` etc. Each window
  component imports the globals it needs.
- Events → set a global property (UI binds to it) or invoke a global callback:
  `radio:state`, `claude:status`, `config:changed`, `account/monitor/pinned:
  changed`, `update:progress`, `settings:show`. Rust holds `Weak` handles to all
  4 windows and pushes via `invoke_from_event_loop`.

### Async / threading
`tokio` runtime for updater HTTP, the 500ms claude-status poll, the reveal poll,
and audio events (media slice). Slint owns the main thread; all cross-thread UI
writes go through `slint::invoke_from_event_loop` + `Weak<Component>`.

### Build / packaging (replace Task + Wails)
- `cargo build --release`; `ui/build.rs` runs `slint-build`. No Node/Vite, no
  Wails CLI, no `generate bindings`.
- **Linux:** keep **nfpm** for `.deb`/`.rpm` (reuse `nfpm.yaml` — **swap deps**:
  drop `libwebkitgtk-6.0`/`gtk4`; add the Slint winit runtime deps —
  `libxkbcommon`, `libfontconfig`, `libGL`/Mesa; `wmctrl`/`xprop`/`xdotool` are
  **no longer needed** since x11rb replaces them). AppImage: reuse
  `build-appimage.sh`/`AppRun` (bundle fonts; the GStreamer bundling is the media
  slice's call). `cargo-bundle` is an alternative but nfpm already works.
- **macOS:** `cargo-bundle` (or hand `Info.plist`) → `.app`; **keep
  `LSUIElement` + `NSAccessibilityUsageDescription` + `com.eyalm321.clawdpanel`**;
  `.pkg` via `pkgbuild` with the existing `build/darwin/scripts`. Universal =
  build `x86_64`+`aarch64` and `lipo`.
- **Windows:** `cargo build` + **reuse the NSIS `project.nsi`** for the installer
  `.exe` (preserves the self-updater's `/S` silent-install contract);
  `cargo-wix`/MSI is an option but NSIS keeps parity.
- **CI** (`release.yml`): same 3-OS matrix on tag `v*`. Replace setup-go/node/
  Wails steps with `actions-rust-lang/setup-rust-toolchain`; Linux apt list loses
  GTK/WebKit, gains the Slint runtime libs above; keep the NSIS/nfpm/appimagetool/
  pkgbuild steps and the release/flatten job unchanged.
- **Mobile (feasibility note only):** Slint *can* render on Android
  (`android-activity`) and iOS (experimental), so the UI could run — but the
  entire platform-shell concept (top-of-screen docking, struts/AppBar/
  NSStatusWindow, system tray, window pushdown, hover reveal) has **no mobile
  analogue**. The existing `android/`/`ios/` dirs are Wails scaffold. Treat mobile
  as out of scope; don't design it.

---

## Crate picks

- **slint** + **i-slint-backend-winit** — native UI, no WebView; winit gives raw
  window control the markup can't.
- **slint-build** — compiles `.slint` into the binary (replaces go:embed of dist).
- **raw-window-handle** — uniform native handle access for the per-OS calls.
- **windows** (or **windows-sys**) — all Win32 (window styles, AppBar, layered
  attrs, region clip, monitors, DWM, cursor, fullscreen).
- **x11rb** — pure-Rust X11; replaces every `wmctrl`/`xprop`/`xdotool` subprocess
  (struts, dock hint, geometry, opacity, map/unmap, cursor, fullscreen, RandR).
- **objc2** + **objc2-app-kit** + **objc2-application-services** — Cocoa window
  styling, NSScreen, NSEvent, AX (fullscreen). Plus **cc** to compile the existing
  `pushdown_darwin.m` AX logic and FFI to it (lowest-risk pushdown port).
- **tray-icon** + **muda** — cross-platform tray + menu with check/radio state
  (Linux = StatusNotifierItem/AppIndicator).
- **image** — decode embedded PNG → RGBA for the tray icon.
- **single-instance** + **interprocess** — lock + second-instance reveal ping.
- **tokio** — async runtime (updater, polls).
- **reqwest** + **serde** + **serde_json** — updater HTTP + GitHub JSON.
- **nix** (or **libc**) — `setsid` detached spawn on unix.
- **auto-launch** — *optional* autostart (else hand-roll for exact-file fidelity).
- **dirs** / **etcetera** — `AppDataDir` equivalents (APPDATA / XDG / Library).

---

## 1:1 fidelity risks

1. **macOS pushdown (AX)** — ~800 lines of AX observers, throttling, permission
   prompting, diagnostics. **Highest risk.** Mitigation: compile the existing
   `.m` via `cc` and FFI rather than re-port (keeps behavior + `pushdown.log`).
2. **Coordinate/DPI conventions** — the codebase deliberately mixes physical px
   (win/linux) vs points (mac) in `PhysWidth`, top-left origin with a Cocoa
   bottom-left flip, and per-monitor `DpiScale`. winit's Physical/Logical +
   `scale_factor` must be mapped meticulously or the dock/slide/hit-box drift.
   Mitigation: keep `MonitorInfo` semantics identical; port the conversions
   line-for-line; reuse the reveal test suite.
3. **Multi-window always-on-top + frameless + skip-taskbar in Slint** — Slint
   markup can't set all of these; requires winit window attributes + raw handles
   per window, and the tray event loop must co-drive with Slint. Mitigation:
   winit backend + `WinitWindowAccessor`; prototype the 4-window shell first.
4. **X11 strut math / dock-edge** — exact `_NET_WM_STRUT_PARTIAL` 12-cardinal
   layout, `edgeReservable`, gnome 32px offset must match or space reservation
   breaks on multi-monitor. Mitigation: port the math verbatim; the x11rb move is
   otherwise a robustness *gain*.
5. **Windows clip-top region during slide** — `SetWindowRgn` on the Slint surface
   may interact with winit's own surface management; verify the region survives
   redraws. Mitigation: apply on the raw HWND post-creation; fall back to
   move-only if the compositor already hides the spill.
6. **`ResetDwmFrame` necessity** — was a Wails-transparent-window artifact; an
   opaque Slint window probably doesn't need it, but unverified. Mitigation: test
   focus-loss on Windows; re-add via the `windows` crate if a chrome strip shows.
7. **Tray radio/check semantics + dynamic rebuild** — `muda` has no true radio
   group; emulate mutual exclusion and rebuild on account changes (today's
   `runTray`). Linux still needs an SNI host (unchanged limitation).
8. **Renderer/GPU on Linux** — Slint's femtovg/Skia renderer wants OpenGL; bare
   VMs/odd GPUs may need the software renderer. The bar is tiny → software
   renderer is acceptable. Add to packaging deps.
9. **Single-instance reveal IPC** — Wails bundled it; we must build the lock +
   ping path ourselves. Low-medium risk (well-trodden).
10. **Slint window background vs frameless transparency** — keep windows opaque
    `#0B0C0E` (today's `BackgroundColour`) to sidestep transparent-frame issues.

---

## Effort

Slice total: **L–XL**.

| Piece | Size |
|---|---|
| Workspace + `types` + slint-build skeleton + 4-window shell + globals bridge | M |
| Windows platform (window/monitor/dock/fullscreen/cursor) | M |
| Linux platform (x11rb dock/struts/monitors/cursor/fullscreen) | L |
| macOS window/monitor/fullscreen (objc2) | M |
| macOS pushdown (reuse `.m` via cc) | M (pure-Rust port: L–XL) |
| reveal port (already decoupled) + tests | S |
| tray + autostart | M |
| updater (3 flavors + window + progress) | M |
| single-instance + IPC | S–M |
| packaging (nfpm/appimage/pkg/NSIS) + CI | M |

### Ordering / dependencies on other slices
1. **Workspace + `types` crate + slint-build skeleton** — blocks *every* slice;
   do first (I own this).
2. **4-window shell + globals/property contract** — the **UI slice depends on
   this** for its window components and the binding surface; publish the
   `global Backend` callback/event contract early.
3. **Per-OS window/monitor/dock** — Windows first (no subprocess, simplest), then
   Linux (x11rb), then macOS (pushdown last).
4. **reveal** — depends only on `WindowOps`.
5. **tray + autostart** — fairly independent; need config from **claude-core**
   (accounts/monitors) for the menu.
6. **updater** — needs config + the Update window.
7. **packaging/CI** — last; depends on the media slice's audio backend choice
   (GStreamer-or-not changes Linux deps/bundling).

Cross-slice contracts I provide: the `types` crate (shared `Config`,
`MonitorInfo`, `BarData`), the `WindowOps` trait, the `global Backend` Slint
bridge, and the main event loop other slices plug into.
