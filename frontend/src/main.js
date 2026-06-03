import './style.css';
import {
  GetBarData, GetConfig, SetActiveAccount,
  GetMonitors, SetMonitor, ToggleClickThrough, GetVersion,
  SaveConfig, SetPinned,
  RadioPlayStation, RadioPause, RadioSetVolume, RadioSetShuffle, SetActiveStation,
  RadioNext, RadioPrev, RadioStationHasTracks, RadioSeek,
  OpenTerminal, OpenTerminalPrompt, ToggleBrandMenu
} from '../bindings/claudepanel/app.js';
import { Events } from '@wailsio/runtime';

const BAR_CHARS = 9;

// Format message count: 90543 → "90.5K", 1234 → "1.2K", 150 → "150"
function fmtMsgs(n) {
  if (n >= 1000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'K';
  return String(n);
}

function renderProgress(pct) {
  const filled = Math.min(BAR_CHARS, Math.round(pct * BAR_CHARS));
  const empty  = BAR_CHARS - filled;
  let char = '░';
  if (pct >= 0.25 && pct < 0.55) {
    char = '▒';
  } else if (pct >= 0.55 && pct < 0.85) {
    char = '▓';
  } else if (pct >= 0.85) {
    char = '█';
  }
  return { fill: char.repeat(filled), empty: '·'.repeat(empty) };
}

// normalizeBarSeparators owns every inter-segment "·" so hiding any segment
// (via a Bar Options toggle or the data-driven hourly gate) never leaves a
// doubled or dangling separator. It walks the bar contents in order and shows a
// separator only when a visible segment precedes it and none has been shown
// since — collapsing runs of hidden segments + their separators to a single
// divider, and dropping leading/trailing ones. Segment visibility is read from
// each element's own inline style.display, set by the apply*/refresh functions.
function normalizeBarSeparators() {
  const container = document.getElementById('bar-main-contents');
  if (!container) return;
  let seenSeg = false;   // a visible segment has appeared at/after the start
  let gapHasSep = false; // a separator already shown since the last visible seg
  for (const el of container.children) {
    if (el.classList.contains('spacer')) continue;
    if (el.classList.contains('sep')) {
      const show = seenSeg && !gapHasSep;
      el.style.display = show ? '' : 'none';
      if (show) gapHasSep = true;
    } else if (el.style.display !== 'none') {
      seenSeg = true;
      gapHasSep = false;
    }
  }
}

// applyFeatureVisibility shows/hides the optional bar segments per cfg.features
// (a missing flag counts as enabled). The terminal segment is owned by
// applyTermSegment and the 5-hour block by refresh (it also needs live data),
// so this handles radio / monitor / theme / weekly. Ends by normalizing the
// separators around whatever changed.
function applyFeatureVisibility() {
  const f = (cfg && cfg.features) || {};
  const setDisp = (id, on) => {
    const el = document.getElementById(id);
    if (el) el.style.display = on ? '' : 'none';
  };
  setDisp('seg-radio', f.radio !== false);
  setDisp('seg-mon', f.monitor !== false);
  setDisp('seg-theme', f.theme !== false);
  setDisp('seg-msgs', f.weeklyUsage !== false);
  setDisp('seg-reset', f.weeklyUsage !== false);
  normalizeBarSeparators();
}

// State
let cfg        = null;
let monitors   = [];
let refreshId  = null;

async function refresh() {
  try {
    const data = await GetBarData();
    if (!data) return;

    // Account + subscription
    document.getElementById('val-acct').textContent =
      (data.accountName || '---').toUpperCase();
    const sub = data.subscriptionType || '';
    document.getElementById('val-sub').textContent  = sub ? `[${sub}]` : '';

    // Weekly Usage/Messages this billing period
    const msgs = data.periodMessages || 0;
    const limit = data.periodMsgLimit || 0;
    const lblMsgs = document.querySelector('#seg-msgs .lbl');

    if (limit > 0) {
      if (lblMsgs) lblMsgs.textContent = 'WEEKLY:';
      const pct = data.periodPercent || 0;
      document.getElementById('val-msgs').textContent = (pct * 100).toFixed(0) + '%';
    } else {
      if (lblMsgs) lblMsgs.textContent = 'MSGS:';
      document.getElementById('val-msgs').textContent = fmtMsgs(msgs);
    }

    // Progress bar — only when a limit is configured
    const progWrap = document.getElementById('prog-wrap');
    if (limit > 0) {
      const pct = data.periodPercent || 0;
      // 1. Text blocks
      const p = renderProgress(pct);
      document.getElementById('prog-fill-text').textContent = p.fill;
      document.getElementById('prog-empty-text').textContent = p.empty;
      // 2. Solid outlined bar
      document.getElementById('prog-fill-bar').style.width = Math.min(100, Math.max(0, pct * 100)) + '%';
      
      progWrap.style.display = '';
    } else {
      progWrap.style.display = 'none';
    }

    // Dynamic warning classes
    const pct = data.periodPercent || 0;
    const warnMed = data.periodMsgLimit > 0 && pct >= 0.85 && pct < 0.95;
    const warnHigh = data.limitExceeded || (data.periodMsgLimit > 0 && pct >= 0.95);
    document.getElementById('seg-msgs').classList.toggle('warn-medium', warnMed);
    document.getElementById('seg-msgs').classList.toggle('warn-high', warnHigh);

    // 5-hour rolling usage and reset. Shown only when data is available AND the
    // 5H feature is enabled; the surrounding separators are owned by
    // normalizeBarSeparators (called at the end of refresh).
    const segHourly = document.getElementById('seg-hourly');
    const segHourlyReset = document.getElementById('seg-hourly-reset');
    const hourlyEnabled = !cfg || !cfg.features || cfg.features.hourlyUsage !== false;
    if (data.hourlyPercent >= 0 && hourlyEnabled) {
      document.getElementById('val-hourly').textContent = (data.hourlyPercent * 100).toFixed(0) + '%';

      const hpct = data.hourlyPercent || 0;
      // 1. Text blocks
      const hp = renderProgress(hpct);
      document.getElementById('prog-fill-hourly-text').textContent = hp.fill;
      document.getElementById('prog-empty-hourly-text').textContent = hp.empty;
      // 2. Solid outlined bar
      document.getElementById('prog-fill-hourly-bar').style.width = Math.min(100, Math.max(0, hpct * 100)) + '%';

      document.getElementById('val-hourly-reset').textContent = data.hourlyResetIn || '---';

      if (segHourly) segHourly.style.display = '';
      if (segHourlyReset) segHourlyReset.style.display = '';

      // Dynamic hourly warnings
      const hwarnMed = hpct >= 0.85 && hpct < 0.95;
      const hwarnHigh = hpct >= 0.95;
      segHourly.classList.toggle('warn-medium', hwarnMed);
      segHourly.classList.toggle('warn-high', hwarnHigh);
    } else {
      if (segHourly) segHourly.style.display = 'none';
      if (segHourlyReset) segHourlyReset.style.display = 'none';
    }

    // Reset countdown
    document.getElementById('val-reset').textContent = data.resetIn || '---';

    // Model
    document.getElementById('val-model').textContent = data.primaryModel || '---';

    // Status
    let displayStatus = data.status || 'IDLE';
    if (displayStatus === 'OFFLINE') displayStatus = 'IDLE';
    
    const status = displayStatus.toLowerCase();
    document.getElementById('val-status').textContent = displayStatus;
    const segSt = document.getElementById('seg-status');
    segSt.className = 'seg ' + status;

    // Tidy separators after the weekly/hourly segments settled to their final
    // visibility for this tick.
    normalizeBarSeparators();

  } catch (err) {
    console.error('refresh error:', err);
  }
}

async function updateMonitorDisplay() {
  try {
    cfg      = await GetConfig();
    monitors = await GetMonitors();
    document.getElementById('val-mon').textContent =
      String((cfg.monitor || 0) + 1);
  } catch (e) { /* ignore */ }
}

async function init() {
  try {
    initTheme();
    cfg      = await GetConfig();
    monitors = await GetMonitors();

    pinned = cfg.pinned !== false;
    applyPinUI();

    // Hide account cycling arrows if there is only one account configured
    const accounts = (cfg && cfg.accounts) || [];
    if (accounts.length < 2) {
      document.getElementById('btn-acct-prev').style.display = 'none';
      document.getElementById('btn-acct-next').style.display = 'none';
      document.getElementById('val-acct').style.cursor = 'default';
    } else {
      document.getElementById('btn-acct-prev').style.display = '';
      document.getElementById('btn-acct-next').style.display = '';
      document.getElementById('val-acct').style.cursor = 'pointer';
    }

    // Hide monitor cycling arrows if there is only one monitor detected
    const totalMonitors = monitors.length;
    if (totalMonitors < 2) {
      document.getElementById('btn-mon-prev').style.display = 'none';
      document.getElementById('btn-mon-next').style.display = 'none';
    } else {
      document.getElementById('btn-mon-prev').style.display = '';
      document.getElementById('btn-mon-next').style.display = '';
    }

    applyTermSegment();
    applyFeatureVisibility();

    // Radio stations (config-driven) + persisted selection/volume.
    stations = (cfg && cfg.stations) || [];
    activeStationIdx = (cfg && cfg.activeStation) || 0;
    if (activeStationIdx >= stations.length) activeStationIdx = 0;
    if (cfg && typeof cfg.radioVolume === 'number') {
      currentVolume = Math.round(cfg.radioVolume * 100);
    }
    applyStationsUI();
    updateVolumeUI();

    const intervalMs = ((cfg && cfg.refreshSeconds) || 15) * 1000;
    await refresh();
    await updateMonitorDisplay();

    refreshId = setInterval(refresh, intervalMs);

    // Initialize native player volume
    try {
      await RadioSetVolume(currentVolume / 100.0);
    } catch (e) {
      console.error('Failed to set initial radio volume:', e);
    }

    // One ordered handler for config changes: re-read cfg FIRST, then re-render
    // everything that depends on it (so the hourly gate in refresh() sees the
    // fresh feature flags rather than a stale copy). account/monitor keep their
    // lightweight dedicated handlers.
    Events.On('config:changed', onConfigChanged);
    Events.On('account:changed', refresh);
    Events.On('monitor:changed', updateMonitorDisplay);
    Events.On('claude:status', refresh);
    // Auto-hide slide animation is driven from Go (window position);
    // no JS-side animation state to manage.

  } catch (err) {
    console.error('init error:', err);
  }
}

// ── Account cycling ──────────────────────────────────────────────────────────

async function cycleAccount(dir) {
  try {
    cfg = await GetConfig();
    const total = (cfg.accounts || []).length;
    if (total < 2) return;
    const next = ((cfg.activeAccount || 0) + dir + total) % total;
    await SetActiveAccount(next);
    cfg.activeAccount = next;
    await refresh();
  } catch (e) { console.error(e); }
}

document.getElementById('btn-acct-prev').addEventListener('click', () => cycleAccount(-1));
document.getElementById('btn-acct-next').addEventListener('click', () => cycleAccount(+1));

// Also allow clicking the account name itself to cycle forward if multiple are configured
document.getElementById('val-acct').addEventListener('click', () => cycleAccount(+1));

// ── Monitor cycling ──────────────────────────────────────────────────────────

async function cycleMon(dir) {
  try {
    cfg      = await GetConfig();
    monitors = await GetMonitors();
    const total = monitors.length;
    if (total < 2) return;
    const next = ((cfg.monitor || 0) + dir + total) % total;
    await SetMonitor(next);
    cfg.monitor = next;
    document.getElementById('val-mon').textContent = String(next + 1);
  } catch (e) { console.error(e); }
}

document.getElementById('btn-mon-prev').addEventListener('click', () => cycleMon(-1));
document.getElementById('btn-mon-next').addEventListener('click', () => cycleMon(+1));

// ── Theme cycling ───────────────────────────────────────────────────────────
const THEMES = ['CLAUDE', 'FALLOUT', 'AMBER', 'MATRIX', 'DRACULA'];
let activeThemeIdx = 0;

function applyTheme(idx) {
  const bar = document.getElementById('bar');
  // Remove old theme classes
  THEMES.forEach(t => bar.classList.remove(`theme-${t.toLowerCase()}`));
  
  const themeName = THEMES[idx];
  bar.classList.add(`theme-${themeName.toLowerCase()}`);
  document.getElementById('val-theme').textContent = themeName;
  localStorage.setItem('claudepanel-theme', themeName);
}

function cycleTheme(dir) {
  activeThemeIdx = (activeThemeIdx + dir + THEMES.length) % THEMES.length;
  applyTheme(activeThemeIdx);
}

// Set up listeners for theme cycler
document.getElementById('btn-theme-prev').addEventListener('click', () => cycleTheme(-1));
document.getElementById('btn-theme-next').addEventListener('click', () => cycleTheme(+1));
document.getElementById('val-theme').addEventListener('click', () => cycleTheme(+1));

function initTheme() {
  const savedTheme = localStorage.getItem('claudepanel-theme') || 'CLAUDE';
  let idx = THEMES.indexOf(savedTheme);
  if (idx === -1) idx = 0;
  activeThemeIdx = idx;
  applyTheme(idx);
}

// ── Pin / Unpin (auto-hide) ─────────────────────────────────────────────────
// Auto-hide is driven entirely by a Go-side cursor-position poller — WebView2
// mouseleave is unreliable on a 28-px-tall window, so JS doesn't observe hover
// at all. The poller compares the OS cursor against the bar's screen rect.
let pinned = true;

function applyPinUI() {
  document.getElementById('seg-pin').classList.toggle('pinned', pinned);
}

async function togglePin() {
  pinned = !pinned;
  applyPinUI();
  try {
    await SetPinned(pinned);
  } catch (e) { console.error('SetPinned failed:', e); }
}

document.getElementById('seg-pin').addEventListener('click', togglePin);

// ── Brand menu ────────────────────────────────────────────────────────────────
// Clicking the ClaudePanel logo toggles a small dropdown window (Check for
// updates / Exit) anchored beneath the icon. The window is created and positioned
// by Go and auto-hides when it loses focus; the bar only triggers the toggle.
document.getElementById('seg-brand').addEventListener('click', async () => {
  try { await ToggleBrandMenu(); } catch (e) { console.error('toggle menu failed:', e); }
});

// ── Radio Player (background audio streaming) ────────────────────────────────
// The Go backend manages native playback and emits state events via 'radio:state'.
// The frontend only maintains the station list and sends commands (play, pause, volume).

// Stations are config-driven now (managed from the tray "Configure Stations…").
// The bar cycler indexes cfg.stations; the Go station engine owns the queue,
// shuffle, auto-advance and looping. We only send a station index to play.
let stations = [];
let activeStationIdx = 0;
let isRadioPlaying = false;
let currentVolume = 100;

function activeStation() {
  if (!stations.length) return { name: '---' };
  if (activeStationIdx < 0 || activeStationIdx >= stations.length) activeStationIdx = 0;
  return stations[activeStationIdx];
}

// Show the cycler arrows only when there's more than one station; refresh the
// idle title. Called after config (re)loads.
function applyStationsUI() {
  const prev = document.getElementById('btn-radio-prev');
  const next = document.getElementById('btn-radio-next');
  const show = stations.length >= 2 ? '' : 'none';
  if (prev) prev.style.display = show;
  if (next) next.style.display = show;
  if (!isRadioPlaying) setRadioStatus('off');
  updateShuffleUI();
  updateTrackNavUI();
}

// Whether the active station can be stepped track-by-track. The backend is
// authoritative (it re-parses each item the way the player does, so a
// watch?v=…&list=… playlist saved with a stale "video" kind is still recognised);
// we cache the result so the ‹ › click guards stay synchronous.
let trackNavActive = false;

// Gray out the ‹ › track buttons when the station has nothing to skip to (a single
// video, a single livestream, or an empty station). Re-queries the backend; safe
// to fire-and-forget from synchronous callers.
async function updateTrackNavUI() {
  let on = false;
  if (stations.length) {
    try { on = await RadioStationHasTracks(activeStationIdx); }
    catch (e) { console.error('RadioStationHasTracks failed:', e); }
  }
  trackNavActive = on;
  for (const id of ['btn-radio-track-prev', 'btn-radio-track-next']) {
    const el = document.getElementById(id);
    if (el) el.classList.toggle('is-disabled', !on);
  }
}

function updateVolumeUI() {
  const volEl = document.getElementById('radio-vol');
  if (volEl) volEl.textContent = currentVolume + '%';
}

async function setVolume(vol) {
  currentVolume = Math.min(200, Math.max(0, vol));
  localStorage.setItem('claudepanel-fm-volume', currentVolume);
  updateVolumeUI();
  try {
    await RadioSetVolume(currentVolume / 100.0);
  } catch (e) {
    console.error('RadioSetVolume failed:', e);
  }
}

async function cycleVolume() {
  let nextVol = currentVolume - 10;
  if (nextVol < 0) {
    nextVol = currentVolume === 0 ? 200 : 0;
  }
  await setVolume(nextVol);
}

// Reflect the active station's shuffle mode on the bar's shuffle icon (clay when
// on, dimmed when off). Called whenever the station or its state changes.
function updateShuffleUI() {
  const el = document.getElementById('btn-radio-shuffle');
  if (!el) return;
  const on = !!activeStation().shuffle;
  el.classList.toggle('on', on);
  el.title = on ? 'Shuffle: on (click to turn off)' : 'Shuffle: off (click to shuffle)';
}

function setRadioStatus(state) {
  const statusEl = document.getElementById('radio-status');
  const titleEl  = document.getElementById('radio-title');
  if (!statusEl) return;
  const stationName = activeStation().name;
  // Drive the play/pause icon + color via state classes (no more [ON]/[OFF] text).
  statusEl.classList.remove('playing', 'loading', 'err');
  switch (state) {
    case 'load':
      isRadioPlaying = false;
      statusEl.classList.add('loading');
      statusEl.title = 'Loading…';
      if (titleEl) { titleEl.textContent = stationName; titleEl.classList.remove('marquee'); }
      resetTimeline(); // new track: zero the scrubber until the first progress tick
      break;
    case 'on':
      isRadioPlaying = true;
      statusEl.classList.add('playing');
      statusEl.title = 'Pause';
      if (titleEl) {
        titleEl.textContent = `NOW PLAYING ${stationName} · NOW PLAYING ${stationName} · `;
        titleEl.classList.add('marquee');
      }
      break;
    case 'off':
      isRadioPlaying = false;
      statusEl.title = 'Play';
      if (titleEl) { titleEl.textContent = stationName; titleEl.classList.remove('marquee'); }
      break;
    case 'err':
      isRadioPlaying = false;
      statusEl.classList.add('err');
      statusEl.title = 'Error — click to retry';
      if (titleEl) { titleEl.textContent = stationName; titleEl.classList.remove('marquee'); }
      break;
  }
  updateShuffleUI();
}

// ── Seek timeline ────────────────────────────────────────────────────────────
// Click the track title to reveal an inline scrubber. The Go engine emits
// throttled progress events (state=playing, progress=true) carrying position +
// duration in seconds; we paint the groove/handle from those. Dragging the
// handle (or clicking the track) seeks via RadioSeek. dur<=0 ⇒ a livestream:
// the groove shows an inert "LIVE" with no handle.
let tlOpen = false;
let tlDragging = false;
let curPos = 0;
let curDur = 0;
let tlHideTimer = null;

// Auto-collapse the scrubber back to the title a few seconds after the cursor
// leaves the radio segment (cancelled while hovering or mid-drag).
function scheduleTimelineHide() {
  clearTimeout(tlHideTimer);
  tlHideTimer = setTimeout(() => {
    if (tlOpen && !tlDragging) toggleTimeline(false);
  }, 3000);
}
function cancelTimelineHide() { clearTimeout(tlHideTimer); }

function fmtTime(s) {
  s = Math.max(0, Math.floor(s || 0));
  const m = Math.floor(s / 60);
  return m + ':' + String(s % 60).padStart(2, '0');
}

function updateTimeline(pos, dur) {
  curPos = pos; curDur = dur;
  const tl = document.getElementById('radio-timeline');
  if (!tl) return;
  const fill = document.getElementById('radio-tl-fill');
  const handle = document.getElementById('radio-tl-handle');
  const curEl = document.getElementById('radio-time-cur');
  const durEl = document.getElementById('radio-time-dur');
  const live = !(dur > 0);
  tl.classList.toggle('live', live);
  if (live) {
    if (fill) fill.style.width = '100%';
    if (curEl) curEl.textContent = 'LIVE';
    if (durEl) durEl.textContent = '';
    return;
  }
  const frac = Math.min(1, Math.max(0, pos / dur));
  if (fill) fill.style.width = (frac * 100) + '%';
  if (handle) handle.style.left = (frac * 100) + '%';
  if (curEl) curEl.textContent = fmtTime(pos);
  if (durEl) durEl.textContent = fmtTime(dur);
}

// Neutral "starting a track" paint (duration not known yet) — avoids flashing
// LIVE for a VOD during the brief load before the first progress tick.
function resetTimeline() {
  curPos = 0; curDur = 0;
  const tl = document.getElementById('radio-timeline');
  if (tl) tl.classList.remove('live');
  const fill = document.getElementById('radio-tl-fill');
  const handle = document.getElementById('radio-tl-handle');
  const curEl = document.getElementById('radio-time-cur');
  const durEl = document.getElementById('radio-time-dur');
  if (fill) fill.style.width = '0%';
  if (handle) handle.style.left = '0%';
  if (curEl) curEl.textContent = '0:00';
  if (durEl) durEl.textContent = '--:--';
}

// Show/hide the inline scrubber. Opening swaps the marquee title for the groove;
// closing restores it. Toggles when `open` is omitted.
function toggleTimeline(open) {
  tlOpen = (open === undefined) ? !tlOpen : open;
  const seg = document.getElementById('seg-radio');
  const tl = document.getElementById('radio-timeline');
  if (seg) seg.classList.toggle('timeline-open', tlOpen);
  if (tl) tl.hidden = !tlOpen;
  cancelTimelineHide(); // a fresh toggle resets any pending auto-collapse
}

// Drag-to-seek: pointer drag on the track scrubs; release issues one RadioSeek.
// A plain click jumps to that point. stopPropagation keeps these off the
// segment's play/pause click handler.
(function wireSeek() {
  const track = document.getElementById('radio-tl-track');
  if (!track) return;
  const fill = document.getElementById('radio-tl-fill');
  const handle = document.getElementById('radio-tl-handle');

  const fracFromEvent = (e) => {
    const r = track.getBoundingClientRect();
    if (r.width <= 0) return 0;
    return Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
  };
  const paint = (frac) => {
    if (fill) fill.style.width = (frac * 100) + '%';
    if (handle) handle.style.left = (frac * 100) + '%';
    const curEl = document.getElementById('radio-time-cur');
    if (curEl && curDur > 0) curEl.textContent = fmtTime(frac * curDur);
  };

  track.addEventListener('pointerdown', (e) => {
    if (curDur <= 0) return; // livestream: not seekable
    e.stopPropagation();
    e.preventDefault();
    tlDragging = true;
    try { track.setPointerCapture(e.pointerId); } catch (_) {}
    paint(fracFromEvent(e));
  });
  track.addEventListener('pointermove', (e) => {
    if (!tlDragging) return;
    e.stopPropagation();
    paint(fracFromEvent(e));
  });
  track.addEventListener('pointerup', async (e) => {
    if (!tlDragging) return;
    e.stopPropagation();
    tlDragging = false;
    try { track.releasePointerCapture(e.pointerId); } catch (_) {}
    const frac = fracFromEvent(e);
    if (curDur > 0) {
      try { await RadioSeek(frac * curDur); }
      catch (err) { console.error('RadioSeek failed:', err); }
    }
  });
  track.addEventListener('pointercancel', () => { tlDragging = false; });
})();

// Receive and handle state from native player
Events.On('radio:state', (event) => {
  const data = event ? event.data : null;
  if (!data) return;
  // Filter to the active station: the engine stamps each event with its index
  // (the playing videoID changes per track as the queue auto-advances).
  if (typeof data.stationIdx === 'number' && data.stationIdx !== activeStationIdx) {
    return;
  }
  // Throttled playhead tick (not a state transition): move the seek timeline
  // without disturbing the status/marquee. Ignored mid-drag so the handle the
  // user is holding doesn't snap back.
  if (data.progress) {
    if (!tlDragging) updateTimeline(data.position || 0, data.duration || 0);
    return;
  }
  switch (data.state) {
    case 'loading':
      setRadioStatus('load');
      break;
    case 'playing':
      setRadioStatus('on');
      break;
    case 'ended':
      // Transient: a track finished and the engine is advancing to the next
      // one. Keep showing "playing" — a fresh loading/playing will follow.
      break;
    case 'paused':
      setRadioStatus('off');
      break;
    case 'idle':
      setRadioStatus('off');
      break;
    case 'error':
      console.error('Native player error:', data.error);
      setRadioStatus('err');
      break;
  }
});

async function toggleRadio() {
  if (!stations.length) return;
  try {
    if (isRadioPlaying) {
      await RadioPause();
    } else {
      setRadioStatus('load');
      await RadioPlayStation(activeStationIdx);
    }
  } catch (err) {
    console.error('Radio error:', err);
    setRadioStatus('err');
  }
}

// Toggle shuffle mode for the active station. Optimistically flips the local flag
// (so the icon responds instantly) and asks Go to persist + apply it. This is a
// pure mode toggle: it never starts playback, so toggling while paused stays
// paused — it only randomizes future auto-advance. Reverts the icon if the call
// fails.
async function toggleShuffle() {
  if (!stations.length) return;
  const st = activeStation();
  const next = !st.shuffle;
  st.shuffle = next;
  updateShuffleUI();
  try {
    await RadioSetShuffle(activeStationIdx, next);
  } catch (e) {
    console.error('RadioSetShuffle failed:', e);
    st.shuffle = !next;
    updateShuffleUI();
  }
}

// Skip to the next/previous track within the active station. Guarded by
// trackNavActive so a click on a grayed-out arrow is a no-op (the buttons keep
// pointer events for layout, so we ignore the click here rather than via CSS).
async function trackNext() {
  if (!trackNavActive) return;
  try { await RadioNext(); } catch (e) { console.error('RadioNext failed:', e); }
}
async function trackPrev() {
  if (!trackNavActive) return;
  try { await RadioPrev(); } catch (e) { console.error('RadioPrev failed:', e); }
}

async function cycleStation(dir) {
  if (stations.length < 2) return;
  const wasPlaying = isRadioPlaying;
  activeStationIdx = (activeStationIdx + dir + stations.length) % stations.length;
  try { await SetActiveStation(activeStationIdx); } catch (e) { /* non-fatal */ }
  updateTrackNavUI(); // the new station may differ in skippability
  resetTimeline();    // clear the scrubber for the new station

  if (wasPlaying) {
    try {
      setRadioStatus('load');
      await RadioPlayStation(activeStationIdx);
    } catch (e) {
      console.error('Failed to switch station:', e);
      setRadioStatus('err');
    }
  } else {
    setRadioStatus('off');
  }
}

const radioSeg = document.getElementById('seg-radio');
radioSeg.addEventListener('click', async (e) => {
  if (e.target.id === 'btn-radio-prev') { await cycleStation(-1); return; }
  if (e.target.id === 'btn-radio-next') { await cycleStation(+1); return; }
  if (e.target.id === 'btn-radio-track-prev') { await trackPrev(); return; }
  if (e.target.id === 'btn-radio-track-next') { await trackNext(); return; }
  // Title reveals the seek timeline; the left time read-out (where the title
  // was) collapses it. Clicks on the track itself are handled by the pointer
  // (drag/seek) handlers — swallow them here so they don't toggle play/pause.
  if (e.target.id === 'radio-title') { toggleTimeline(true); return; }
  if (e.target.id === 'radio-time-cur') { toggleTimeline(false); return; }
  if (e.target.closest('#radio-tl-track')) return;
  if (e.target.closest('#btn-radio-shuffle')) { await toggleShuffle(); return; }
  if (e.target.id === 'radio-vol' || e.target.id === 'radio-vol-lbl') {
    await cycleVolume();
    return;
  }
  await toggleRadio();
});

radioSeg.addEventListener('wheel', async (e) => {
  e.preventDefault();
  const diff = e.deltaY < 0 ? 5 : -5;
  await setVolume(currentVolume + diff);
}, { passive: false });

// Auto-collapse the scrubber back to the name shortly after the cursor leaves
// the radio segment; cancel the countdown the moment it returns.
radioSeg.addEventListener('mouseleave', () => { if (tlOpen) scheduleTimelineHide(); });
radioSeg.addEventListener('mouseenter', cancelTimelineHide);

updateVolumeUI();
setRadioStatus('off');

// Account, terminal and station editing now live in a separate popup window
// (settings.html / src/settings/*), opened from the tray "Configure…" items.
// The bar only keeps its cyclers below.

// ── Terminal launcher cycler ─────────────────────────────────────────────────
// ◀ ● NAME ▶ — clicking the name (or dot) opens a new, labeled terminal running
// `claude` in the entry's directory. Mirrors cycleMon/cycleTheme. The segment is
// hidden entirely when no launchers are configured (like the account arrows when
// fewer than two accounts).
let activeTermIdx = 0;
// While a terminal is being launched the button shows "LAUNCHING <NAME>" for a
// brief moment as click feedback (the terminal itself opens detached).
let isLaunching = false;

function applyTermSegment() {
  const seg = document.getElementById('seg-term');
  const terms = (cfg && cfg.terminals) || [];
  const enabled = !cfg || !cfg.features || cfg.features.terminals !== false;
  // Hidden when the feature is off OR there's nothing to launch. The adjacent
  // separator (#sep-term) is owned by normalizeBarSeparators, not here.
  if (terms.length === 0 || !enabled) {
    if (seg) seg.style.display = 'none';
    normalizeBarSeparators();
    return;
  }
  if (seg) seg.style.display = '';
  if (activeTermIdx >= terms.length) activeTermIdx = 0;

  const t = terms[activeTermIdx];
  const name = (t.name || '---').toUpperCase();
  document.getElementById('val-term').textContent = isLaunching ? `LAUNCHING ${name}` : `LAUNCH ${name}`;
  const dot = document.getElementById('dot-term');
  if (t.color) {
    dot.style.background = t.color; // exact configured hex, inline (beats theme CSS)
    dot.style.display = 'inline-block';
  } else {
    dot.style.display = 'none';
  }

  // Hide the arrows when there's only one entry to cycle through.
  const showArrows = terms.length >= 2 ? '' : 'none';
  document.getElementById('btn-term-prev').style.display = showArrows;
  document.getElementById('btn-term-next').style.display = showArrows;

  normalizeBarSeparators();
}

function cycleTerm(dir) {
  const terms = (cfg && cfg.terminals) || [];
  if (terms.length < 2) return;
  activeTermIdx = (activeTermIdx + dir + terms.length) % terms.length;
  applyTermSegment();
}

// Re-read config and re-render the segment after any config change (editor save).
async function refreshTerminals() {
  try {
    cfg = await GetConfig();
    applyTermSegment();
  } catch (e) { /* ignore */ }
}

let lastTermOpen = 0;
async function openTerm(e) {
  const terms = (cfg && cfg.terminals) || [];
  if (terms.length === 0) return;
  // Shift-click: prompt for a per-launch sublabel in the popup rather than
  // opening immediately. Plain click stays an instant one-click open.
  if (e && e.shiftKey) {
    try { await OpenTerminalPrompt(activeTermIdx); }
    catch (err) { console.error('terminal prompt failed:', err); }
    return;
  }
  // ~400ms debounce so a fast double-click can't spawn two windows.
  const now = Date.now();
  if (now - lastTermOpen < 400) return;
  lastTermOpen = now;
  isLaunching = true;
  applyTermSegment();
  try {
    await OpenTerminal(activeTermIdx, '');
  } catch (err) {
    alert('Could not open terminal: ' + err);
  }
  isLaunching = false;
  applyTermSegment();
}

document.getElementById('btn-term-prev').addEventListener('click', () => cycleTerm(-1));
document.getElementById('btn-term-next').addEventListener('click', () => cycleTerm(+1));
document.getElementById('val-term').addEventListener('click', openTerm);
document.getElementById('dot-term').addEventListener('click', openTerm);

// ── Radio stations: bar cycler refresh ───────────────────────────────────────
// Editing stations now lives in the settings popup; the bar only re-reads the
// list after a save so its cycler reflects edits.
async function refreshStations() {
  try {
    cfg = await GetConfig();
    stations = (cfg && cfg.stations) || [];
    if (activeStationIdx >= stations.length) activeStationIdx = 0;
    applyStationsUI();
  } catch (e) { /* ignore */ }
}

// Single config-change handler. Order matters: refreshTerminals/refreshStations
// re-read cfg, then applyFeatureVisibility + refresh render against that fresh
// copy (refresh's 5H gate reads cfg.features, so it must run last).
async function onConfigChanged() {
  await refreshTerminals();
  applyFeatureVisibility();
  await refreshStations();
  await refresh();
}

// ── Boot ─────────────────────────────────────────────────────────────────────
document.addEventListener('DOMContentLoaded', init);
