# SLICE: ui-slint — Visual/UI layer → Slint

Fidelity anchor for the Wails→Rust/Slint rewrite. Goal: reproduce the retro HUD
look **1:1** natively (no WebView). This doc catalogs every window, component,
state, design token, and the Slint↔Rust contract each window needs. Data lives in
other slices; this slice owns *looks* and *the contract shape*.

> ⚠️ **Myth-buster up front:** the SHARED brief says "nunito woff2 — the retro HUD
> aesthetic." That is wrong. `frontend/src/assets/fonts/nunito-v16-latin-regular.woff2`
> is a **dead asset** — there is **no `@font-face`** anywhere and nothing imports it.
> The bar renders in a **monospace** stack (`--font`, see tokens). `bar.png` /
> `logo.png` / `appicon.png` (repo root) and `assets/images/logo-universal.png` are
> **not used in the UI** either — they're packaging/marketing/icon files. Every
> in-UI graphic is an **inline SVG** (invader, pin, radio play/pause/shuffle). Plan
> around mono fonts + vector SVG, not nunito/raster.

---

## Scope

Files this slice covers (all read-only sources):

| Area | Paths |
|---|---|
| Markup | `frontend/index.html` (bar), `frontend/menu.html`, `frontend/settings.html`, `frontend/update.html` |
| Styling | `frontend/src/style.css` (915 L — **the look**), `frontend/src/app.css` (legacy Wails template leftover, **unused** by these windows) |
| Bar logic/DOM | `frontend/src/main.js` (800 L) |
| Settings | `frontend/src/settings.js`, `frontend/src/settings/{shell,panel-accounts,panel-options,panel-stations}.js` |
| Menu / Update | `frontend/src/menu.js`, `frontend/src/update.js` |
| Assets used | inline SVGs only (in HTML/JS); fonts = **system mono stack** |
| Window chrome | `main.go:53-71` (bar window), `app.go:357-365` (settings), `app.go:407-413` (menu), `app.go:630-638` (update) |

---

## Current behavior — windows

Four frameless WebviewWindows, all `BackgroundColour = NewRGB(0x0B,0x0C,0x0E)`:

| Window | Size (px) | Chrome | Source |
|---|---|---|---|
| **Bar (HUD)** | 1920 × `cfg.BarHeight` (default **28**), resized to monitor width | Frameless, `AlwaysOnTop`, docked to monitor top edge (appbar), auto-hide slide driven by moving the OS window from Go | `main.go:53` |
| **Settings** | 660 × 420 (min 520 × 260) | Frameless, draggable titlebar (`--wails-draggable:drag`) | `app.go:357` |
| **Brand menu** | 188 × 64 | Frameless, anchored under brand icon, auto-hide on focus loss | `app.go:407` |
| **Update** | 520 × 380 (min 400 × 280) | Frameless, draggable titlebar | `app.go:630` |

The bar is the only "docked appbar" window; the other three are floating popups
positioned by Go. Auto-hide is **not** CSS — Go animates window Y so the dark
window slides off-screen leaving no frame (`style.css:58-61`).

---

## Design tokens (extract these exactly)

### Palette — base / CLAUDE default (`style.css:4-18, 275-283`)
```
--bg          #0b0c0e   deep terminal black (also window BackgroundColour)
--text        #f3f4f6   near-white
--text-muted  #6b7280   cool gray (brackets, dividers, labels, arrows)
--clay        #d77757   signature orange — the value-highlight accent
--green       #34d399   (note: :root declares #4fc64d but .theme-claude overrides
                         to #34d399; theme class is always applied at boot → 34d399)
--red         #f23959   high-usage warning
--font        'Cascadia Mono','Cascadia Code','Consolas','SF Mono','Menlo',
              'Fira Code','JetBrains Mono','DejaVu Sans Mono','Inconsolata',
              ui-monospace, monospace
--h           28px      bar height
```
Other fixed colors: `.lbl` `#c9cdd3`; `.sub`/status busy+idle `#b1b9f9` (lavender);
`.prog-empty` `rgba(107,114,128,0.45)`; warn-medium `#eab308` (yellow ≥85%);
warn-high `var(--red)` (≥95% / limit exceeded).

### 5 themes (each overrides the 6 vars + lbl/sub/invader/arrow-hover/prog-empty/status)
| Theme | bg | text | muted | clay(accent) | green | red | extras |
|---|---|---|---|---|---|---|---|
| CLAUDE | `#0b0c0e` | `#f3f4f6` | `#6b7280` | `#d77757` | `#34d399` | `#f23959` | lbl `#c9cdd3`, sub `#b1b9f9` |
| FALLOUT | `#041204` | `#33ff33` | `#145c14` | `#33ff33` | `#33ff33` | `#ff3333` | lbl `#2db32d`, sub `#88ff88`, invader green glow, **CRT scanlines**, outlined prog-bar |
| AMBER | `#0f0700` | `#ffb000` | `#754400` | `#ffb000` | `#ffb000` | `#ff3300` | lbl `#b37b00`, sub `#ffd077`, invader amber glow, **CRT scanlines** |
| MATRIX | `#000000` | `#00ff00` | `#004400` | `#00ff00` | `#00ff00` | `#ff0000` | lbl `#00b300`, sub `#88ff88`, **blinking text cursor**, **CRT scanlines** |
| DRACULA | `#282a36` | `#f8f8f2` | `#6272a4` | `#ff79c6`(pink) | `#50fa7b` | `#ff5555` | lbl `#8be9fd`(cyan), sub `#bd93f9` |

Theme is persisted in `localStorage['clawdpanel-theme']` and **shared across all
windows** (settings/menu/update read it via the `storage` event → `applyTheme`).
Theme classes are scoped to `#bar.theme-*` AND `#settings-root.theme-*`.

### Sizes / spacing
- Bar: `font-size 11px`, `font-weight bold`, `letter-spacing 0.02em`, `padding 0 8px`,
  `border-bottom 1px solid rgba(107,114,128,.15)`.
- `.seg`: flex, `gap 5px`, `padding 0 5px`, full height.
- `.sep` ("·"): `font-size 8px`, `opacity .4`, `padding 0 5px`, normal weight.
- Invader SVG `15×11`, `fill var(--clay)`; pin SVG `12×12`; arrows `font-size 9px`;
  `.sub` `font-size 10px`.
- Progress: **9 chars** (`BAR_CHARS`, `main.js:12`). Char ramp by pct (`main.js:20-32`):
  `<25% ░`, `25–55% ▒`, `55–85% ▓`, `≥85% █`; empties rendered as `·`.
- Fallout outlined prog-bar: `90×8px`, only `border-right`+`border-bottom` 2px clay,
  fill `9px` tall (pops 1px above), red block `cursor-bar 8×10px` flush right.
- Radio transport chips: `16px` tall, ghost fill `rgba(255,255,255,.05)`, hairline
  border `rgba(255,255,255,.07)`, `border-radius 4px`, clustered `margin-left -3px`.
- Radio title viewport `.radio-title-wrap`: fixed `168px` (sized to seek scrubber so
  name⇄timeline toggle doesn't shift layout).
- Seek track `height 8px`, `border-radius 5px`, inset groove shadow; handle `12×13px`
  white→gray vertical gradient pill with two grip lines (`::before`).

### Effects / animation (`style.css`)
- `@keyframes blink` (cursor) `1s step-end infinite` — MATRIX text caret + Fallout bar caret.
- `@keyframes marquee` `translate3d(0)→translate3d(-50%,0)` `8s linear infinite` — "NOW PLAYING …" scroll.
- `@keyframes soundwave` opacity 0.7↔1 + green, `1.5s` — playing pause-icon pulse.
- `@keyframes radio-pulse` opacity 0.35↔1, `0.8s` — loading play-icon pulse.
- Invader hover `filter: drop-shadow(0 0 3px var(--clay))`; Fallout/Amber invader has a
  steady `drop-shadow(0 0 2px ...)` glow.
- Pin: `transform rotate(0→45deg)` + fill gray→clay when pinned; `transition .15s`.
- **CRT scanline overlay** (`style.css:361-374`) for Fallout/Amber/Matrix — a `::after`
  full-cover layer: `linear-gradient(rgba(18,16,16,0) 50%, rgba(0,0,0,.15) 50%)`
  (horizontal scanlines, `background-size 100% 3px`) **+** an RGB sub-pixel mask
  `linear-gradient(90deg, rgba(255,0,0,.04), rgba(0,255,0,.01), rgba(0,0,255,.04))`
  (`3px 100%`), `opacity .8`, `pointer-events:none`, `z-index 10`.
- Numerous `transition: color/filter/transform .1–.12s` on hovers.

---

## Component catalog (bar — `index.html` + `main.js`)

Left→right segments, each a `.seg`, separated by `.sep` "·". `normalizeBarSeparators()`
(`main.js:41`) collapses separators around hidden segments — Slint must replicate this
(a hidden segment must not leave a doubled/dangling "·").

| # | Segment (id) | Content | States |
|---|---|---|---|
| 1 | **Brand** `#seg-brand` | invader SVG | hover glow; `.menu-open` → opacity .7; click → `ToggleBrandMenu` |
| 2 | **Account** `#seg-acct` | `◀ NAME [PLAN] ▶` | arrows hidden if `<2` accounts; name uppercased; plan badge lavender; click name/arrows → cycle |
| 3 | **Weekly/Msgs** `#seg-msgs` | `MSGS:`/`WEEKLY:` + value + 9-char progress | label flips on whether limit configured; `%` vs `90.5K` (`fmtMsgs`); `.warn-medium`/`.warn-high`; progress hidden if no limit |
| 4 | **Reset** `#seg-reset` | `RESET:` + countdown | text only |
| 5 | **5H** `#seg-hourly` (+`#seg-hourly-reset`) | `5H:` + value + progress, `RESET:` countdown | hidden unless `hourlyPercent≥0` AND feature on; own warn classes |
| 6 | **Model** | value | text only |
| 7 | **Status** `#seg-status` | `IDLE`/busy text | classes `idle`/`busy` (lavender `#b1b9f9`), `offline`→shown as IDLE |
| — | **spacer** `.spacer` | flex:1 | pushes right cluster |
| 8 | **Radio** `#seg-radio` | `« ‹ [title/marquee or seek timeline] › » [play/pause] [shuffle] · VOL xx%` | see radio states below |
| 9 | **Monitor** `#seg-mon` | `◀ MON n ▶` | arrows hidden if `<2` monitors |
| 10 | **Theme** `#seg-theme` | `◀ THEME NAME ▶` | cycles 5 themes |
| 11 | **Pin** `#seg-pin` | pin SVG | `.pinned` (clay, rotated) vs unpinned (gray, upright) |

**Radio sub-states** (`main.js:467-504`, `style.css:541-602`):
- status icon: play ▶ (off/loading/err) ↔ pause ⏸ (playing); `.loading` clay pulse,
  `.playing` green soundwave pulse, `.err` red.
- title: idle = station name; playing = marquee `NOW PLAYING <name> · …` (animated).
- track arrows `‹ ›`: `.is-disabled` (opacity .3, no hover) when station has no tracks
  (queried via `RadioStationHasTracks`).
- shuffle: `.on` → white (mode toggle, not accent).
- volume: `VOL xx%`, scroll/click to change (`0–200%`, step 5 wheel / −10 click wrap).
- **seek timeline** (toggled by clicking title): `radio-tl-track` groove + draggable
  handle + `cur/dur` times; `.live` variant (dur≤0) shows inert green "LIVE", no handle.

### Other windows' components
- **Menu** (`menu.html`): `.brand-menu` with 2 `.brand-menu-item` rows: "CHECK FOR
  UPDATES" (glyph ◻→◼ busy→✔ done via CSS `::before`; right hint text with
  `is-busy/is-ok/is-update/is-error` colors) and "EXIT" (`--danger`, red hover).
- **Settings** (`settings.html` + `shell.js`): `.modal` → `.modal-titlebar`
  (invader + title + ✕ close) → `.modal-main` = `.modal-nav` sidebar (132px,
  ACCOUNTS/STATIONS/OPTIONS, active item clay left-border) + `.modal-body` panel.
  Panels: **Accounts** (select + EDIT/ADD/DELETE, NAME/PATH form), **Stations**
  (select + form with dynamic URL-row list, `+ URL`, ✕ remove), **Options** (5
  feature checkboxes `accent-color var(--clay)`). Controls = `.editor-input`/
  `.editor-select`/`.editor-btn`(+`--accent`) bumped to 26px in the modal.
- **Update** (`update.html`): titlebar + details panel (CURRENT/LATEST version grid,
  scrollable monospace RELEASE NOTES box, LATER/UPDATE NOW buttons) ⇄ progress panel
  (status text, 26px bar with fill + centered `%` using `mix-blend-mode:difference`,
  `x.xx / y.yy MB`).

---

## Rust/Slint design

### Layout & module tree
One UI crate `clawdpanel-ui` owning all `.slint`. Compile with `slint-build` in
`build.rs`. Suggested file tree (mirrors the 4 windows + shared atoms):

```
ui/
  theme.slint        // global Theme { palette properties, per-theme setters }
  tokens.slint       // sizes/spacings/durations as consts
  atoms.slint        // Invader, PinIcon, PlayIcon, PauseIcon, ShuffleIcon (Path-based)
                     // Separator, Arrow, GhostChip, EditorInput/Select/Btn
  crt.slint          // ScanlineOverlay component (themed)
  bar.slint          // BarWindow + all 11 segments + Radio cluster + SeekTimeline
  settings.slint     // SettingsWindow + Shell + Accounts/Stations/Options panels
  menu.slint         // BrandMenuWindow
  update.slint       // UpdateWindow
```

Each window is a Slint `Window` exported to Rust as its own component (`BarWindow`,
`SettingsWindow`, `BrandMenuWindow`, `UpdateWindow`), so the Rust side can create
them independently the way Go creates 4 WebviewWindows today. Slint window flags:
`no-frame: true`, `background: transparent`/`#0b0c0e`, `always-on-top` for the bar.
**Docking/appbar/auto-hide stays in the platform slice** (Slint can't reserve screen
edge); the UI just exposes the bar at a backend-set size/position.

### Theme system
A Slint **global** `Theme` holding all palette properties (`bg`, `text`, `muted`,
`clay`, `green`, `red`, `lbl`, `sub`, plus a `theme_id` enum and a `crt` bool).
Rust sets `theme_id`; a pure-Slint `if`/`states` block maps id→concrete colors so
the whole switch happens in markup (matching the CSS `.theme-*` class approach).
All components read `Theme.clay` etc. — no hardcoded colors. The shared-across-windows
behavior (localStorage `storage` event) becomes: Rust holds the theme in app state and
pushes `theme_id` into every open window's `Theme` global on change.

### SVG icons → Slint `Path`
The invader, pin, play/pause, shuffle are tiny vector paths. Two options:
1. **`Path { commands: "M…" }`** — paste the existing SVG path `d` strings almost
   verbatim (the invader path in `index.html:15` and `shell.js:11` is reusable as-is).
2. Embed as SVG `Image` via `@image-url` (Slint renders SVG through resvg).
Prefer (1) for the monochrome glyphs so `fill: Theme.clay` re-themes them for free;
use (2) only if a path is fussy.

### Fonts
Bundle a real monospace TTF (the CSS stack's first hits — Cascadia Mono — aren't
guaranteed present). Register via `slint-build` `EmbedResourcesKind::EmbedFiles` +
`font-family: "Cascadia Mono"` (or ship **Cascadia Code** / **JetBrains Mono** TTF and
name accordingly). **Ignore the nunito woff2** — it was never used. Slint can't load
`.woff2`; ship `.ttf`/`.otf`.

### Reproducing CSS effects in Slint
| CSS effect | Slint approach |
|---|---|
| Flat colors, borders, radius | `Rectangle { background; border-width; border-color; border-radius }` — direct. |
| Char progress (░▒▓█ · ) | Keep as a `Text` whose string Rust/Slint builds from pct (port `renderProgress`). Trivial 1:1. |
| Fallout outlined prog-bar | `Rectangle` with only right+bottom `border` (Slint borders are uniform → fake the two missing edges by overlaying two thin `Rectangle`s, or draw the 2 visible edges as child rects). Minor. |
| Marquee | animated `x` on the title `Text` inside a `clip: true` viewport: `animate x { duration: 8s; iteration-count: -1; }` looping 0→−width/2. |
| Blink caret | `animate opacity { duration: 1s; easing: ease; iteration-count: -1; }` with a 2-stop (or a boolean toggled by a `Timer`) to mimic `step-end` hard cut. |
| soundwave / radio-pulse | property animations on icon `opacity`/`color`. |
| Linear gradients (handle, titlebar tint) | `@linear-gradient(deg, …)` — direct. |
| Drop-shadow **glow** on invader (text/vector glow) | Slint has **no blur/glow filter**. Approx with a slightly larger, lower-opacity duplicate `Path` behind, or accept a flat icon. ⚠ risk. |
| **CRT scanlines** (tiled 3px gradients) | Slint `@linear-gradient` doesn't tile via `background-size`. Use a pre-rendered tiny tiling `Image` (3px scanline PNG) stretched with `image-fit: tile`, or a custom `@radial`/repeating shader. ⚠ risk (see below). |
| Inset box-shadow (seek groove) | Slint shadows are **outer only** (`drop-shadow-*`). Fake inset with a darker inner `Rectangle` + top hairline. ⚠ minor risk. |
| `mix-blend-mode: difference` (update %) | **Not supported.** Use a fixed contrasting color or split the `%` text into two clipped halves (over-fill vs over-track). ⚠ risk. |
| Hover/active/focus | Slint `TouchArea { has-hover }` + `states`/`when` blocks. |

---

## Slint ↔ Rust binding surface (contract shape per window)

Implemented as Slint **globals** with `in` properties (data → UI) and `callback`s
(UI → backend). Other slices supply the data; shapes below are the contract.

### `BarWindow` — global `Bar`
**in properties** (from the `GetBarData` shape, `main.js:88-176`):
`account_name: string`, `subscription: string`, `period_label: string` (MSGS/WEEKLY),
`period_value: string`, `period_pct: float`, `period_has_limit: bool`,
`warn_level: enum{none,medium,high}`, `reset_in: string`,
`hourly_visible: bool`, `hourly_value: string`, `hourly_pct: float`,
`hourly_reset_in: string`, `hourly_warn: enum`, `primary_model: string`,
`status: enum{idle,busy,offline}`,
`accounts_count: int`, `monitors_count: int`, `monitor_label: string`,
`theme_id: enum`, `pinned: bool`,
features: `show_radio/show_monitor/show_theme/show_weekly/show_hourly: bool`,
radio: `radio_state: enum{off,loading,playing,err}`, `radio_title: string`,
`radio_marquee: bool`, `stations_count: int`, `track_nav_enabled: bool`,
`shuffle_on: bool`, `volume_pct: int`, `seek_open: bool`, `seek_pos/seek_dur: float`,
`seek_live: bool`.
**callbacks (out):** `cycle_account(int dir)`, `cycle_monitor(int dir)`,
`cycle_theme(int dir)`, `toggle_pin()`, `toggle_brand_menu()`, `radio_toggle()`,
`cycle_station(int dir)`, `track_next()`, `track_prev()`, `toggle_shuffle()`,
`set_volume(int pct)`, `cycle_volume()`, `seek(float frac)`, `open_seek(bool)`.

### `BrandMenuWindow` — global `Menu`
**in:** `check_state: enum{idle,busy,done}`, `hint: string`, `hint_kind: enum{none,busy,ok,update,error}`.
**callbacks:** `check_updates()`, `quit()`, `close()`.

### `SettingsWindow` — global `Settings`
**in:** `active_panel: enum{accounts,stations,options}`, `accounts: [{name,path}]`,
`active_account: int`, `stations: [{name, items:[{raw,id}], shuffle}]`,
`features: {radio,monitor,theme,weeklyUsage,hourlyUsage: bool}`.
**callbacks:** `show_panel(enum)`, `save_config(Config)`, `parse_station_item(string)
→ Item` (server-side validation, currently `ParseStationItem`), `close()`.
(Native dialogs replace JS `alert`/`confirm`.)

### `UpdateWindow` — global `Update`
**in:** `current: string`, `latest: string`, `changelog: string`,
`has_installer: bool` (downloadUrl present), `phase: enum{details,progress}`,
`pct: float`, `downloaded_mb: float`, `total_mb: float`, `status_text: string`.
**callbacks:** `install()`, `open_download_page()`, `later()`, `close()`.

---

## Crate picks
- **slint** — the UI toolkit (markup + Rust integration). Core of this slice.
- **slint-build** — `build.rs` compilation + font/asset embedding.
- **i-slint-backend-winit** (via slint feature `backend-winit`) — needed because the
  bar requires custom window flags (frameless, transparent, always-on-top) and the
  platform slice must reach the raw winit/`raw-window-handle` for docking/appbar.
- **raw-window-handle** — hand the native handle to the platform slice (replaces
  Go's `a.hwnd` docking).
- **resvg** (transitively, Slint's SVG renderer) — only if any icon ships as SVG
  `Image` rather than `Path`.
- No extra font crate — Slint embeds TTF directly.

## 1:1 fidelity risks
1. **Glow / drop-shadow on text & vector** (invader hover glow, Fallout/Amber steady
   glow). Slint has no blur filter. *Mitigation:* duplicate-path halo or accept flat;
   visually minor on a 15px icon.
2. **CRT scanline overlay** (Fallout/Amber/Matrix). The tiled 3px gradient + RGB mask
   can't be a single Slint gradient. *Mitigation:* a tiny tiling PNG (`image-fit:
   tile`) at `opacity .8`, or a fragment shader. Achievable, but pixel-exact match
   needs care — highest-effort effect here.
3. **`mix-blend-mode: difference`** on the update `%` readout. Unsupported.
   *Mitigation:* clip-split the text or use a fixed high-contrast color.
4. **Inset shadow** on the seek groove → fake with inner rects (close, not identical).
5. **`step-end` blink** — Slint easing is continuous; emulate the hard on/off with a
   boolean `Timer` toggle rather than an eased opacity animation.
6. **Monospace font availability** — must bundle a TTF; the CSS relied on whatever
   mono the OS had. Pick one (Cascadia Code / JetBrains Mono) and ship it so all
   platforms match. Metrics will differ slightly from each user's old fallback —
   acceptable since we now control the font.
7. **Separator normalization** — port `normalizeBarSeparators` as Slint layout logic
   (only show a "·" between two visible segments). Pure logic, low risk.
8. **Window transparency + docked auto-hide** — depends on the platform slice; the UI
   alone can't reserve the screen edge. Flag as a cross-slice dependency.

## Effort
**L** overall. Breakdown: theme/tokens/atoms **S–M**; bar window incl. radio cluster +
seek timeline **M–L** (most components, most states); settings (3 panels + dynamic URL
rows) **M**; menu + update **S** each; CRT/glow effects **M** (the fiddly fidelity
tail).

**Dependencies / ordering:** depends on the **platform slice** (frameless/transparent
window creation, docking, auto-hide) and consumes data shapes from the **backend/data
slices** (`GetBarData`, `GetConfig`, radio engine, updater). Build order: window
chrome + Theme global first → bar segments → radio → settings/menu/update → effects
polish last.
