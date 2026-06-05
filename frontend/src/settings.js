// Entry point for the reusable settings popup window (settings.html).
//
// Go opens this window (frameless) from the tray "Configure…" items and tells
// it which panel to show — via the URL on first load, and via the
// "settings:show" event for subsequent opens / panel switches. The shell renders
// the ClawdPanel chrome and mounts the requested panel module.
import './style.css';
import { Events } from '@wailsio/runtime';
import { createShell, applyTheme } from './settings/shell.js';
import accounts from './settings/panel-accounts.js';
import terminals from './settings/panel-terminals.js';
import stations from './settings/panel-stations.js';
import options from './settings/panel-options.js';
import terminalOpen from './settings/panel-terminal-open.js';

// Insertion order drives the sidebar nav order; terminalOpen is nav:false so it
// never appears as a sidebar entry (it's the Shift-click launch dialog).
const PANELS = {
  [accounts.id]: accounts,
  [terminals.id]: terminals,
  [stations.id]: stations,
  [options.id]: options,
  [terminalOpen.id]: terminalOpen,
};

// A "show" request carries the panel id plus optional context (index/name) used
// by context-specific panels like terminal-open.
function payloadFromURL() {
  const u = new URLSearchParams(location.search);
  const panel = u.get('panel');
  return {
    panel: panel && PANELS[panel] ? panel : 'accounts',
    index: parseInt(u.get('index') || '0', 10),
    name: u.get('name') || '',
  };
}

function boot() {
  applyTheme();
  const shell = createShell(PANELS);

  // First load: context is encoded in the URL (the event may fire before this
  // listener is registered).
  const first = payloadFromURL();
  shell.show(first.panel, first);

  // Subsequent opens / panel switches on the already-loaded page.
  Events.On('settings:show', (e) => {
    const d = e && e.data;
    const p = Array.isArray(d) ? d[0] : d;
    const id = p && p.panel;
    if (id && PANELS[id]) shell.show(id, p);
  });

  // Track the bar's theme. localStorage is shared across the app's windows, and
  // the `storage` event fires here when the bar changes the theme.
  window.addEventListener('storage', (e) => {
    if (e.key === 'clawdpanel-theme') applyTheme();
  });
}

document.addEventListener('DOMContentLoaded', boot);
