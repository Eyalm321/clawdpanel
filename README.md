<p align="center"><img src="logo.png" alt="ClawdPanel logo" width="160" /></p>

<h1 align="center">👾 ClawdPanel</h1>

<p align="center"><strong>A retro-styled desktop HUD for Claude Code.</strong><br/>
Monitor token usage, switch accounts, dock across monitors, and stream Lo-Fi radio — all from a native cross-platform utility bar.</p>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-%3E%3D1.75-orange?logo=rust&logoColor=white" alt="Rust >=1.75" />
  <img src="https://img.shields.io/badge/Slint-v1-blue" alt="Slint v1" />
  <img src="https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-success" alt="Windows | macOS | Linux" />
  <img src="https://img.shields.io/badge/RAM-~6MB-orange" alt="~6MB RAM" />
  <img src="https://img.shields.io/badge/license-MIT-green" alt="MIT License" />
</p>

<p align="center"><img src="bar.png" alt="Bar layout" /></p>

<p align="center"><em>Always-on-top · Frameless · Multi-monitor Dock · Pin / auto-hide on hover · System tray · Multi-account</em></p>

---

## 💡 Why ClawdPanel?

A permanent lightweight desktop HUD for Claude Code users — live token monitoring, multi-account switching, cross-monitor docking, retro terminal aesthetics, ambient Lo-Fi radio, zero-browser workflow.

Unlike browser dashboards or terminal-only tools, ClawdPanel lives directly in your desktop environment with native OS integrations: Windows AppBar reservation, macOS LaunchAgents, Linux `_NET_WM_STRUT_PARTIAL`, system-tray everywhere.

## 👤 Built for

- Claude Code power users running long sessions
- Teams juggling multiple Claude accounts
- Terminal enthusiasts and retro / CRT-aesthetic fans
- Anyone who prefers a HUD over an extra browser tab

---

## 🎬 Demo

<!-- TODO: drop GIFs into docs/demo/ and replace this section with a 3-column table:
| Themes | Auto-hide | Claude FM |
|---|---|---|
| ![](docs/demo/themes.gif) | ![](docs/demo/hide.gif) | ![](docs/demo/fm.gif) |
-->

_Animated demos coming soon. For now, see [the bar layout above](#-clawdpanel) and the [themes](#-visual-design) section below._

---

## 🖥️ Core Features

- **Live token usage** — weekly + hourly consumption, percentage, shaded progress bar, reset countdown
- **Multi-account** — switch any number of Claude accounts (separate `~/.claude` paths) via tray or Settings
- **Multi-monitor docking** — pick the target monitor at any time; reserves screen space on Windows + Linux X11
- **Pin / auto-hide on hover** — unpin to slide the bar off-screen; cursor at top edge slides it back
- **System tray** — switch account, switch monitor, toggle start-on-login, manage accounts, quit
- **Start on login** — native autostart on all three platforms
- **5 retro themes** — Claude, Fallout, Amber, Matrix, Dracula
- **Headless Claude FM** — embedded Lo-Fi YouTube stream with custom volume control

---

## 🚀 Quick Start

Download from the [Releases](../../releases/latest) page:

| Platform | File | Notes |
|---|---|---|
| Windows 10/11 x64 | `ClawdPanel-*-windows-amd64-setup.exe` | NSIS installer. Native executable (no WebView2 or Go runtime required). |
| macOS 10.13+ (Intel + Apple Silicon) | `ClawdPanel-*-macos-universal.pkg` | Double-click to install to `/Applications`. |
| Debian / Ubuntu | `clawdpanel_*_amd64.deb` | `sudo apt install ./clawdpanel_*_amd64.deb` |
| Fedora / RHEL | `clawdpanel-*.x86_64.rpm` | `sudo dnf install ./clawdpanel-*.x86_64.rpm` |
| Any Linux (portable) | `ClawdPanel-x86_64.AppImage` | `chmod +x ClawdPanel-x86_64.AppImage && ./ClawdPanel-x86_64.AppImage` |

Installers wire up Claude Code's `statuslineCommand` automatically and clean it up on uninstall — no terminal commands needed. AppImage users get a one-time first-run prompt instead.

<details>
<summary><strong>First-launch security warnings (unsigned v2)</strong></summary>

- **Windows** → SmartScreen "Windows protected your PC" → *More info* → *Run anyway*
- **macOS** → "ClawdPanel cannot be opened…" → System Settings → Privacy & Security → *Open Anyway*, or right-click the .app → *Open*
- **Linux .deb/.rpm** → no warnings (root install)
- **AppImage** → no warnings (user-mode)

</details>

<details>
<summary><strong>Build from source</strong></summary>

Requires Rust 1.75+ (using Cargo).

#### Linux Prerequisites

Install the required development and library packages:

```bash
sudo apt-get update
sudo apt-get install -y \
  pkg-config \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libfontconfig1-dev libgl1-mesa-dev \
  libx11-dev libxcursor-dev libxi-dev libxrandr-dev libxcb1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  libgtk-3-dev libayatana-appindicator3-dev
```

#### Build / Run Commands

```bash
# Run locally in development mode
cargo run -p clawdpanel-app

# Build the release binary
cargo build --release -p clawdpanel-app

# Run the release binary
./target/release/clawdpanel
```

</details>

---

## 🎨 Visual Design

Five distinct CRT-scanline-filtered themes, cycle on the fly:

| Theme | Vibe |
|---|---|
| 🔸 **CLAUDE** (default) | Flat CLI — signature terracotta orange (`#d77757`), lavender badges (`#b1b9f9`), white headers |
| 🟢 **FALLOUT** | Pip-Boy green HUD, outlined progress brackets, glowing CRT scanlines |
| 🟡 **AMBER** | DEC VT100 / Fallout NV amber terminal — glowing values, dimmed labels |
| 📟 **MATRIX** | Digital rain — sharp green characters, blinking caret synced to warning status |
| 😈 **DRACULA** | Sleek dark mode — cyan labels, pastel pink progress |

<p align="center"><strong>Claude</strong> &nbsp;·&nbsp; <em>terracotta + lavender, the default</em></p>
<p align="center"><img src="docs/themes/claude.png" alt="Claude theme" /></p>

<p align="center"><strong>Fallout</strong> &nbsp;·&nbsp; <em>Pip-Boy green with CRT scanlines</em></p>
<p align="center"><img src="docs/themes/fallout.png" alt="Fallout theme" /></p>

<p align="center"><strong>Amber</strong> &nbsp;·&nbsp; <em>DEC VT100 glowing amber</em></p>
<p align="center"><img src="docs/themes/amber.png" alt="Amber theme" /></p>

<p align="center"><strong>Matrix</strong> &nbsp;·&nbsp; <em>digital rain with dot dividers</em></p>
<p align="center"><img src="docs/themes/matrix.png" alt="Matrix theme" /></p>

<p align="center"><strong>Dracula</strong> &nbsp;·&nbsp; <em>dark mode with pastel pink progress</em></p>
<p align="center"><img src="docs/themes/dracula.png" alt="Dracula theme" /></p>

### Typography & TUI glyphs

- **Developer-first font stack**: prefers Cascadia Mono / Cascadia Code, then SF Mono, Menlo, Fira Code, JetBrains Mono, DejaVu Sans Mono, Inconsolata — uses whatever's installed.
- **Retro shaded meters**: usage rendered with `░▒▓█` glyphs that change shade with warning tier; unused cells are faint terminal middle dots (`·`).

---

## 🛠️ Advanced Features

### Pin / auto-hide

A pin icon to the right of THEME toggles:
- **Pinned** (orange, tilted): docked, permanently visible — the default.
- **Unpinned** (gray, upright): bar slides up off-screen; cursor at the top edge slides it back.

<details>
<summary>Implementation notes</summary>

Rust-side cursor polling at 100 ms — Slint/winit's `mouseleave` events are bypassed since polling the OS cursor is more reliable on a 28 px window. A 200 ms grace period prevents accidental dismissal on cursor overshoot. The slide animates the OS window's Y position at ~60 fps (ease-out cubic) with a clipping mask that hides any portion that would spill onto a monitor sitting above. (Windows and Linux X11 only; on macOS and Linux Wayland the toggle still affects docked-vs-floating state but the slide is a no-op.)

</details>

### Multi-monitor docking

Pick the target monitor from the tray menu. On Windows, **AppBar mode** uses `SHAppBarMessage` to reserve screen space — maximized windows automatically tile below. On Linux X11 it sets `_NET_WM_STRUT_PARTIAL` for compatible compositors. On macOS and Linux Wayland the bar floats at the topmost window level without reserving space.

### Claude FM (headless Lo-Fi radio)

Embedded YouTube audio stream — no browser windows needed.

- 📻 **Masked marquee**: when playing, the label scrolls `NOW PLAYING CLAUDE FM ·` horizontally behind a 75 px mask; reverts instantly on pause.
- 🔊 **0–200 % volume range**: extended headroom mapped linearly to YouTube's `0–100`.
- 🎛️ **Dual control**: scroll-wheel adjusts in 5 % steps; clicking `VOL` cycles in 10 % steps.
- 💾 **State persistence**: volume and theme saved to `localStorage`.

### Smart status overrides

Dynamic and static `OFFLINE` indicators are translated globally into a lavender **`IDLE`** badge (`#b1b9f9`) to preserve your active CLI context.

---

## 🧭 Architecture

```
Anthropic  GET /api/oauth/usage        Claude Code (CLI)
   (direct, bypasses any proxy)               │
       │                                       │  statuslineCommand hook
       │  OAuth token from .credentials.json   ▼
       │                              ~/.claude/rate_limits.json   ←  written every prompt
       │                                       │  (fallback when the direct fetch is unavailable)
       └──────────────┬────────────────────────┘
                      │  poll (refreshSeconds), live source preferred
                      ▼
             ClawdPanel backend (Rust)
                      │
                      │  Slint Properties & Callbacks (native compile)
                      ▼
             Slint UI (app.slint)
                      │
                      └── OS integrations (platform-shell): AppBar / NSWindow / X11 dock,
                                           system tray, autostart, monitors
```

ClawdPanel fetches usage **directly from Anthropic** (`/api/oauth/usage`, using the
account's stored OAuth token), hardcoded to the real API host so it keeps updating
even when Claude Code is pointed at a proxy (e.g. `ANTHROPIC_BASE_URL`, Headroom) that
doesn't relay rate-limit data. The statusline-written `rate_limits.json` is used as a
fallback when the direct fetch is unavailable (offline, expired token). Set
`CLAWDPANEL_DISABLE_LIVE_USAGE=1` to disable the direct fetch and use the file only.

---

## ⚙️ Configuration

Auto-created on first run:

| Platform | Path |
|---|---|
| Windows | `%APPDATA%\ClawdPanel\config.json` |
| macOS | `~/Library/Application Support/ClawdPanel/config.json` |
| Linux | `$XDG_CONFIG_HOME/ClawdPanel/config.json` (fallback `~/.config/ClawdPanel/config.json`) |

```json
{
  "monitor": 0,
  "theme": "claude",
  "opacity": 0.92,
  "refreshSeconds": 15,
  "barHeight": 28,
  "appBarMode": true,
  "startWithWindows": false
}
```

> **Accounts** are managed inside the app — open **Manage accounts** from the system tray to add, rename, switch between, or remove Claude account paths. No JSON editing required.

| Field | Description |
|---|---|
| `barHeight` | Pixel height of the bar |
| `refreshSeconds` | Re-read interval for Claude data files |
| `theme` | `claude`, `fallout`, `amber`, `matrix`, `dracula` |
| `appBarMode` | Reserve screen space (Windows / Linux X11 only) |

<details>
<summary><strong>How live usage capture works</strong></summary>

ClawdPanel reads `~/.claude/rate_limits.json`, populated by Claude Code's `statuslineCommand` hook. Installers set this hook automatically and clear it on uninstall by editing `~/.claude/settings.json` (only the `statuslineCommand` key — other keys are preserved).

If you built from source or are using the AppImage and want to configure manually:

```bash
claude config set statuslineCommand "node -e \"const fs=require('fs');const p=require('path');const os=require('os');const d=fs.readFileSync(0,'utf-8');if(d){const parsed=JSON.parse(d);fs.writeFileSync(p.join(os.homedir(),'.claude','rate_limits.json'),JSON.stringify({...parsed,captured_at:Date.now()}))}\""
```

Every Claude prompt then writes a tiny JSON payload to `rate_limits.json`, which ClawdPanel picks up instantly.

</details>

---

## 📁 Project Structure

```
clawdpanel/
├── Cargo.toml                       # Root Cargo workspace manifest
├── Cargo.lock
├── patches/
│   └── rusty_ytdl/                  # Patched YouTube client to handle playlist structures
└── crates/
    ├── app/                         # Coordination, main entry point, settings, radio, updater, menu integration
    ├── claude-core/                 # Watched ~/.claude files, token calculations, and local cache
    ├── media/                       # Headless Claude FM YouTube extraction, DASH streaming, local caching proxy, and playback backend (GStreamer on Linux, stub elsewhere)
    ├── platform-shell/              # OS window creation, auto-updater download/relaunch, system tray, and monitor reservation
    ├── types/                       # Shared configuration models and types
    └── ui/                          # Slint user interface compile definitions and fonts (e.g. app.slint, bar.slint)
```

---

## ⚠️ Known limitations (v2 cross-platform)

- **Linux Wayland**: no portable protocol for "stay above other windows" or "reserve screen space". KWin honors `_NET_WM_WINDOW_TYPE_DOCK`; GNOME/Mutter mostly ignores it; wlroots compositors (Hyprland, Sway) vary. X11 sessions work correctly.
- **macOS docking**: NSWindow at `NSStatusWindowLevel` floats above other windows but can't reserve screen space the way Windows AppBar does — accepted as macOS-native behavior.
- **macOS Gatekeeper** (unsigned v2): see *First-launch security warnings* in Quick Start.
- **Settings merge safety**: if `~/.claude/settings.json` already exists but contains invalid JSON, the installer logs a warning and skips the modification rather than overwriting it.

---

## 📄 License

MIT — see [LICENSE](LICENSE).
