// Accounts panel for the settings popup. Ported from the old inline bar editor
// (main.js), re-laid-out vertically with room to breathe. Data round-trips the
// whole config via GetConfig/SaveConfig; SaveConfig emits "config:changed" which
// refreshes the bar.
import { GetConfig, SaveConfig } from '../../bindings/clawdpanel/app.js';

export default {
  id: 'accounts',
  title: 'ACCOUNTS',
  async mount(body) {
    let config = await GetConfig();
    let formMode = 'edit';

    body.innerHTML = `
      <div class="panel">
        <div class="panel-list" id="acct-list">
          <label class="panel-row">
            <span class="panel-label">ACCOUNT</span>
            <select id="acct-select" class="editor-select panel-grow"></select>
          </label>
          <div class="panel-actions">
            <button id="acct-edit" class="editor-btn">EDIT</button>
            <button id="acct-add" class="editor-btn">ADD</button>
            <button id="acct-del" class="editor-btn">DELETE</button>
          </div>
        </div>

        <div class="panel-form" id="acct-form" style="display:none">
          <label class="panel-row">
            <span class="panel-label">NAME</span>
            <input type="text" id="acct-name" class="editor-input panel-grow" placeholder="main">
          </label>
          <label class="panel-row">
            <span class="panel-label">PATH</span>
            <input type="text" id="acct-path" class="editor-input panel-grow" placeholder="~/.claude">
          </label>
          <div class="panel-actions">
            <button id="acct-save" class="editor-btn editor-btn--accent">SAVE</button>
            <button id="acct-cancel" class="editor-btn">CANCEL</button>
          </div>
        </div>
      </div>`;

    const $ = (id) => body.querySelector(id);
    const listEl = $('#acct-list');
    const formEl = $('#acct-form');
    const select = $('#acct-select');
    const nameI = $('#acct-name');
    const pathI = $('#acct-path');

    function renderSelect() {
      select.innerHTML = '';
      const accounts = config.accounts || [];
      accounts.forEach((acc, i) => {
        const opt = document.createElement('option');
        opt.value = i;
        opt.textContent = `${acc.name} (${acc.path})`;
        select.appendChild(opt);
      });
      const active = config.activeAccount || 0;
      if (active < accounts.length) select.value = active;
    }

    function showList() {
      formEl.style.display = 'none';
      listEl.style.display = 'flex';
    }

    function showForm(mode) {
      formMode = mode;
      listEl.style.display = 'none';
      formEl.style.display = 'flex';
      if (mode === 'add') {
        nameI.value = '';
        pathI.value = '';
      } else {
        const idx = parseInt(select.value, 10);
        const accounts = config.accounts || [];
        if (idx >= 0 && idx < accounts.length) {
          nameI.value = accounts[idx].name;
          pathI.value = accounts[idx].path;
        }
      }
      nameI.focus();
    }

    async function save() {
      const name = nameI.value.trim();
      const path = pathI.value.trim();
      if (!name || !path) { alert('Name and Path cannot be empty!'); return; }
      const accounts = config.accounts || [];
      if (formMode === 'add') {
        accounts.push({ name, path });
        config.activeAccount = accounts.length - 1;
      } else {
        const idx = parseInt(select.value, 10);
        if (idx >= 0 && idx < accounts.length) accounts[idx] = { name, path };
      }
      config.accounts = accounts;
      try {
        await SaveConfig(config);
        renderSelect();
        showList();
      } catch (err) {
        console.error('Failed to save config:', err);
        alert('Error saving config: ' + err);
      }
    }

    async function del() {
      const accounts = config.accounts || [];
      if (accounts.length <= 1) { alert('At least one account is required. Cannot delete!'); return; }
      const idx = parseInt(select.value, 10);
      if (idx < 0 || idx >= accounts.length) return;
      if (!confirm(`Are you sure you want to delete account "${accounts[idx].name}"?`)) return;
      accounts.splice(idx, 1);
      let active = config.activeAccount || 0;
      if (active >= accounts.length) active = 0;
      config.activeAccount = active;
      config.accounts = accounts;
      try {
        await SaveConfig(config);
        renderSelect();
        showList();
      } catch (err) {
        console.error('Failed to delete account:', err);
        alert('Error deleting: ' + err);
      }
    }

    $('#acct-edit').addEventListener('click', () => showForm('edit'));
    $('#acct-add').addEventListener('click', () => showForm('add'));
    $('#acct-del').addEventListener('click', del);
    $('#acct-save').addEventListener('click', save);
    $('#acct-cancel').addEventListener('click', showList);

    renderSelect();
    showList();
  },
};
