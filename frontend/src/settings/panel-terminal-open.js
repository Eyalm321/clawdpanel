// "Open terminal" panel for the settings popup. Shown via the bar's Shift-click
// on a terminal entry (Go's OpenTerminalPrompt). The user can pick an account
// (other than the active one) and type an optional per-launch sublabel; OPEN
// (or Enter) launches the terminal scoped to that account with the sublabel
// appended to the tab title, then closes the popup. Both are per-launch only —
// nothing is written to config. nav:false so it has no sidebar entry.
import { GetConfig, OpenTerminalAs } from '../../bindings/clawdpanel/app.js';
import { Window } from '@wailsio/runtime';

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
  ));
}

export default {
  id: 'terminal-open',
  title: 'OPEN TERMINAL',
  nav: false,
  async mount(body, data) {
    const index = data && Number.isInteger(data.index) ? data.index : 0;
    const name = (data && data.name) || '';

    const config = await GetConfig();
    const accounts = (config && config.accounts) || [];
    const activeAccount = (config && config.activeAccount) || 0;
    const multiAccount = accounts.length > 1;

    const acctOptions = accounts
      .map((a, i) => `<option value="${i}" ${i === activeAccount ? 'selected' : ''}>${escapeHtml(a.name)}</option>`)
      .join('');

    // Only surface the account picker when there's a real choice to make.
    const acctRow = multiAccount ? `
        <label class="panel-row">
          <span class="panel-label">ACCOUNT</span>
          <select id="to-account" class="editor-select panel-grow">${acctOptions}</select>
        </label>` : '';

    body.innerHTML = `
      <div class="panel">
        <div class="panel-hint">Opening <b>${escapeHtml(name)}</b> — choose an account and an optional sublabel for this tab.</div>
        ${acctRow}
        <label class="panel-row">
          <span class="panel-label">SUBLABEL</span>
          <input type="text" id="to-sublabel" class="editor-input panel-grow" placeholder="backend" autocomplete="off" spellcheck="false">
        </label>
        <div class="panel-actions">
          <button id="to-open" class="editor-btn editor-btn--accent">OPEN</button>
          <button id="to-cancel" class="editor-btn">CANCEL</button>
        </div>
      </div>`;

    const input = body.querySelector('#to-sublabel');
    const acctSelect = body.querySelector('#to-account');

    const open = async () => {
      const acctIdx = acctSelect ? parseInt(acctSelect.value, 10) : activeAccount;
      try {
        await OpenTerminalAs(index, acctIdx, input.value.trim());
      } catch (err) {
        console.error('Failed to open terminal:', err);
      }
      Window.Hide();
    };

    body.querySelector('#to-open').addEventListener('click', open);
    body.querySelector('#to-cancel').addEventListener('click', () => Window.Hide());
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); open(); }
    });
    input.focus();
  },
};
