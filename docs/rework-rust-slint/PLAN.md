# ClawdPanel — Rust/Slint rework master plan

Complete rewrite of ClawdPanel from **Wails v3 (Go + WebView/vanilla-JS)** to
**Rust + Slint** (native, no WebView), preserving **exact functionality and looks**.

Synthesizes 4 slice designs (read them for detail):
[ui-slint.md](ui-slint.md) · [claude-core.md](claude-core.md) · [media.md](media.md) · [shell-platform.md](shell-platform.md)

> Planning method: 4 parallel agents (hyperpanes fan-out) each explored one slice
> of the current code read-only and drafted its 1:1 Rust/Slint design.

---

## 1. What the app is (confirmed by exploration)
An always-on-top, frameless, retro **HUD bar** docked to a monitor's top edge, plus
3 lazy popup windows (settings, brand-menu, update). Accessory app (no dock icon),
single-instance, system-tray driven. It (a) reads Claude Code per-account usage from
disk + fetches **live usage from Anthropic** and renders a segmented meter; (b) plays
**YouTube radio** (VOD + live-DASH) as an in-app media player; (c) auto-hides via a
cursor-driven slide; (d) self-updates from GitHub releases. Cross-platform
win/linux(X11)/mac. (android/ios dirs are dead Wails scaffold.)

## 2. Target workspace (one cargo workspace)
```
clawdpanel/
├─ Cargo.toml                 # [workspace]
└─ crates/
   ├─ types/         (lib)    # LEAF, no deps — breaks cycles. Config, MonitorInfo,
   │                          #   BarData/ClaudeBarData, StationConfig, StationItem, media::Event
   ├─ platform-shell (lib)    # windows, monitors, dock/struts/appbar/pushdown, fullscreen,
   │                          #   REVEAL state machine, tray, autostart, updater, single-instance
   ├─ claude-core    (lib)    # config load/save, file readers, live usage fetch, BarData compute, poll engine
   ├─ media          (lib)    # media-audio (per-OS backends), media-radio (resolver+proxy+DASH staticizer), media-station
   ├─ ui             (lib)    # .slint markup + slint-build; Theme + window components + globals bridge
   └─ app            (bin)    # main.rs: event loop, single-instance, wires all slices
```
**Resolved ownership conflict:** the **reveal** auto-hide machine lives in
`platform-shell` (it drives window move/clip/show-hide, global cursor poll, and
fullscreen detection — all platform calls behind a `WindowOps` trait). The `media`
doc described it only because the brief grouped it there.

## 3. Cross-slice contracts (publish these first)
- **`types` crate** — shared structs (`Config`, `MonitorInfo`, `BarData`,
  `StationConfig`, `StationItem`, `media::Event`). Everything depends on it; it
  depends on nothing.
- **`WindowOps` trait** (platform-shell) — the reveal/media seam: window rect/move/
  clip-top/show/hide/click-through, cursor pos, fullscreen-active, autohide-supported.
  Fake-able → headless tests (reuse the Go reveal test suite).
- **Slint globals** (the Wails bindings/events replacement). Reconcile naming now:
  | Global | Owner | Holds |
  |---|---|---|
  | `Theme` | ui | palette + `theme_id` enum + `crt` bool; 5 themes in markup |
  | `ClaudeBar` | claude-core | `ClaudeBarData` (in) + `status` (in, 500ms path) |
  | `RadioBridge` | media | radio state/title/seek/volume (in) + transport callbacks (out) |
  | `Bar` | ui | account/monitor/theme/pin display + cycle/toggle callbacks |
  | `Settings`/`Menu`/`Update` | ui | per-window in-props + callbacks |
  | `Backend` | platform-shell | every former `App` method as a callback; events → property pushes |
  All cross-thread UI writes go through `slint::invoke_from_event_loop` + `Weak<Component>`.
- Async: one **tokio** multi-thread runtime (updater HTTP, claude polls, reveal poll,
  audio event spine). Slint owns the main thread.

## 4. Build order / milestones
**Phase 0 — skeleton (blocks everything; platform-shell owns).** Workspace + `types`
crate + `slint-build` + a 4-window frameless/always-on-top shell that opens and docks.
Publish the `global Backend` callback/event contract.

**Phase 1 — parallel:**
- *platform-shell:* per-OS window/monitor/dock — **Windows first** (no subprocess,
  simplest), then **Linux** (x11rb replaces wmctrl/xprop/xdotool), then **macOS**
  (objc2; pushdown last). reveal port (depends only on `WindowOps`).
- *claude-core:* land **early** — small, test-pinned; defines `ClaudeBar`. Port the
  Go golden tests + `testdata/*` verbatim as the acceptance gate.
- *ui:* Theme/tokens/atoms + the **bar** window against the published contracts.

**Phase 2:**
- *media:* config types → radio resolver + audio traits (parallel) → per-OS audio
  backends → station queue → wire `RadioBridge`. Port the **DASH staticizer
  byte-for-byte** + `staticize_test.go` + the `live-dynamic.mpd` fixture (silence-bug guard).
- *ui:* settings (3 panels + dynamic URL rows) + menu + update windows + radio
  cluster + seek timeline.
- *platform-shell:* tray + autostart + single-instance IPC + **updater** (needs the Update window).

**Phase 3 — polish & ship.** CRT/glow/blend visual-fidelity tail; packaging + CI
(depends on the audio-backend decision). Mobile: out of scope.

## 5. Top risks (ranked, with mitigations)
1. **YouTube extraction — XL (media).** `rusty_ytdl` ≠ kkdai/youtube; signature
   cipher + format/manifest extraction drift with YouTube. → behind the
   `StreamResolver` trait; integration-test vs the same probe IDs; ready to vendor/patch.
2. **DASH "playing but silent" — XL if mishandled (media).** PTO/startNumber shift +
   Linux PAUSED-preroll→ASYNC_DONE→buffer-fill→PLAYING sequence is the whole fix. →
   port staticizer verbatim incl. `timescale=1000` assert + `r=`-reject; carry the
   test + fixture; gstreamer-rs maps the preroll 1:1.
3. **macOS AX pushdown — highest platform risk (~800 lines).** → compile the existing
   `pushdown_darwin.m` via the `cc` crate and FFI to it (keep proven behavior), don't
   re-derive in objc2.
4. **Audio backend strategy — decision required (see §6).**
5. **Slint visual-fidelity gaps (ui), all cosmetic w/ mitigations:** no blur ⇒ glow/
   drop-shadow via duplicate-halo; CRT scanlines via a 3px tiling PNG (`image-fit:tile`);
   `mix-blend-mode:difference` unsupported ⇒ clip-split or fixed contrast; inset shadow
   ⇒ inner rects; `step-end` blink ⇒ boolean `Timer` toggle. Must bundle a mono **TTF**
   (Cascadia Code / JetBrains Mono) — the nunito woff2 was never used.
6. **DPI/coordinate conventions (platform).** Code deliberately mixes physical px
   (win/linux) vs points (mac) in `PhysWidth`, top-left vs Cocoa bottom-left flip. →
   keep `MonitorInfo` semantics identical, port conversions line-for-line, reuse reveal tests.
7. **Multi-window always-on-top/frameless/skip-taskbar in Slint.** → winit backend +
   raw-window-handle per window; prototype the 4-window shell in Phase 0.

## 6. Decisions (LOCKED — 2026-06-15)
- **Audio backend → B (native per-OS):** Linux gstreamer-rs · macOS objc2/AVPlayer ·
  Windows `windows` crate/WinRT MediaPlayer. 1:1 with today, drops the PowerShell
  subprocess. *Open item carried into media work:* verify AVPlayer/WinRT accept the
  static-DASH proxy URL; if not, keep the HLS fallback for mac/win (already an open Q today).
- **Mobile → out of scope.** android/ios dirs are dead Wails scaffold; don't design them.
- **Installers → keep NSIS (`.exe`) + nfpm (`.deb`/`.rpm`) + AppImage + pkgbuild**, to
  preserve the self-updater's silent-install contract.
- **Font → Cascadia Code** (first in today's CSS mono stack). Ship the TTF embedded via slint-build.

## 7. Effort (rough)
ui **L** · claude-core **M** · media **L–XL** (youtube/DASH the unknowns) ·
platform-shell **L–XL** (per-OS + pushdown). Net: a substantial multi-week rewrite;
claude-core + the 4-window shell are the fast, low-risk early wins; media youtube/DASH
and macOS pushdown are the long poles.
