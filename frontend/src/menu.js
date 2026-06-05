// Entry point for the brand-icon dropdown (menu.html).
//
// Go opens this small frameless window anchored under the ClawdPanel brand icon
// (see App.OpenBrandMenu) and auto-hides it on focus loss. It offers two actions:
// "Check for updates" (queries the latest GitHub release) and "Exit" (quits the
// app). The window is hidden, not destroyed, so its state persists between opens.
import './style.css';
import { Browser } from '@wailsio/runtime';
import { applyTheme } from './settings/shell.js';
import { CheckForUpdates, Quit, CloseBrandMenu } from '../bindings/clawdpanel/app.js';

let checking = false;

function setHint(text, cls) {
  const hint = document.getElementById('menu-update-hint');
  if (!hint) return;
  hint.textContent = text || '';
  hint.className = 'brand-menu-hint' + (cls ? ' ' + cls : '');
}

async function onCheckUpdates() {
  if (checking) return;
  checking = true;
  const item = document.getElementById('menu-check-updates');
  // ◼ while the async check runs (the :active press state ends on mouseup).
  item.classList.remove('is-done');
  item.classList.add('is-busy');
  setHint('CHECKING…', 'is-busy');
  try {
    const res = await CheckForUpdates();
    if (res.error) {
      setHint(res.error.toUpperCase(), 'is-error');
    } else if (res.updateAvailable) {
      setHint(`v${res.latest} AVAILABLE`, 'is-update');
    } else {
      item.classList.add('is-done'); // ✔ up to date (the checkmark says it all)
      setHint('');
    }
  } catch (e) {
    console.error('update check failed:', e);
    setHint('CHECK FAILED', 'is-error');
  } finally {
    item.classList.remove('is-busy');
    checking = false;
  }
}

async function onExit() {
  try { await Quit(); } catch (e) { console.error('quit failed:', e); }
}

function boot() {
  applyTheme();
  document.getElementById('menu-check-updates').addEventListener('click', onCheckUpdates);
  document.getElementById('menu-exit').addEventListener('click', onExit);

  // Track the bar's theme — localStorage is shared across the app's windows.
  window.addEventListener('storage', (e) => {
    if (e.key === 'clawdpanel-theme') applyTheme();
  });

  // Reset the transient hint + status glyph each time the menu is reopened (the
  // window is reused, so a stale "UP TO DATE" / ✔ would otherwise linger).
  window.addEventListener('focus', () => {
    if (checking) return;
    setHint('');
    const item = document.getElementById('menu-check-updates');
    item.classList.remove('is-done', 'is-busy');
  });

  // Dismiss on Escape, mirroring the focus-loss auto-hide.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { try { CloseBrandMenu(); } catch (_) {} }
  });
}

document.addEventListener('DOMContentLoaded', boot);
