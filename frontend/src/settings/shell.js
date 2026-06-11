// Reusable settings-popup shell.
//
// Renders the ClawdPanel chrome (invader logo + title + close button) around a
// feature-agnostic body, applies the active theme, and mounts exactly one
// registered "panel" at a time. Adding a new editable feature is just writing
// another panel module (see panel-*.js) and registering it in settings.js — the
// shell knows nothing about accounts or stations specifically.
import { Window } from '@wailsio/runtime';

// Same 8-bit invader used by the bar brand (index.html).
const INVADER = '<svg class="invader" viewBox="0 0 11 8"><path d="M2,0h1v1h-1z M8,0h1v1h-1z M3,1h1v1h-1z M7,1h1v1h-1z M2,2h7v1h-7z M1,3h2v1h-2z M4,3h1v1h-1z M6,3h1v1h-1z M8,3h2v1h-2z M0,4h11v1h-11z M0,5h1v1h-1z M2,5h7v1h-7z M10,5h1v1h-1z M0,6h1v1h-1z M2,6h1v1h-1z M8,6h1v1h-1z M10,6h1v1h-1z M3,7h2v1h-2z M6,7h2v1h-2z"/></svg>';

const THEMES = ['CLAUDE', 'FALLOUT', 'AMBER', 'MATRIX', 'DRACULA'];

// Apply the theme the bar last persisted. localStorage is shared across the
// app's webview windows (same origin), so the popup tracks the bar's choice.
export function applyTheme() {
  const root = document.getElementById('settings-root');
  if (!root) return;
  const saved = localStorage.getItem('clawdpanel-theme') || 'CLAUDE';
  const name = THEMES.includes(saved) ? saved : 'CLAUDE';
  THEMES.forEach((t) => root.classList.remove(`theme-${t.toLowerCase()}`));
  root.classList.add(`theme-${name.toLowerCase()}`);
}

function closeWindow() {
  // Hide (not Close): Go keeps the window object so reopening is instant.
  try { Window.Hide(); } catch (e) { console.error('hide failed', e); }
}

// createShell builds the chrome once and returns a controller able to swap the
// mounted panel. `panels` is a map of id -> { title, mount(bodyEl), nav?,
// navLabel? }. Panels with `nav !== false` get a left-sidebar entry (in map
// insertion order); a panel with `nav === false` hides the sidebar and renders
// standalone as a focused dialog.
export function createShell(panels) {
  const root = document.getElementById('settings-root');

  // Build the sidebar nav from the nav-eligible panels, preserving order.
  const navIds = Object.keys(panels).filter((id) => panels[id].nav !== false);
  const navHtml = navIds
    .map((id) => `<button class="modal-nav-item" data-panel="${id}">${panels[id].navLabel || panels[id].title}</button>`)
    .join('');

  root.innerHTML = `
    <div class="modal">
      <div class="modal-titlebar" style="--wails-draggable: drag">
        <span class="modal-brand">${INVADER}</span>
        <span class="modal-title" id="modal-title">SETTINGS</span>
        <button class="modal-close" id="modal-close" title="Close (Esc)" style="--wails-draggable: no-drag">✕</button>
      </div>
      <div class="modal-main">
        <nav class="modal-nav" id="modal-nav">${navHtml}</nav>
        <div class="modal-body" id="modal-body"></div>
      </div>
    </div>`;

  const titleEl = root.querySelector('#modal-title');
  const bodyEl = root.querySelector('#modal-body');
  const navEl = root.querySelector('#modal-nav');
  root.querySelector('#modal-close').addEventListener('click', closeWindow);

  async function show(panelId, data) {
    const panel = panels[panelId] || panels[Object.keys(panels)[0]];
    if (!panel) return;
    titleEl.textContent = panel.title;

    // A non-nav panel reads as a focused dialog — hide the sidebar; everything
    // else shows it with the active item highlighted.
    const showNav = panel.nav !== false;
    navEl.style.display = showNav ? '' : 'none';
    navEl.querySelectorAll('.modal-nav-item').forEach((b) => {
      b.classList.toggle('active', b.dataset.panel === panelId);
    });

    bodyEl.innerHTML = '';
    try {
      await panel.mount(bodyEl, data);
    } catch (err) {
      console.error(`Failed to mount panel "${panelId}":`, err);
    }
  }

  // Sidebar navigation.
  navEl.querySelectorAll('.modal-nav-item').forEach((btn) => {
    btn.addEventListener('click', () => show(btn.dataset.panel));
  });

  // Esc closes from any panel.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeWindow();
  });

  return { show };
}
