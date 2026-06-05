// Terminals panel for the settings popup. Ported from the old inline bar editor
// (main.js). Manages the list of terminal entries (NAME/DIR/COLOR) plus the
// single global launcher PROGRAM; choosing "custom" reveals exe/args.
import {
  GetConfig, SaveConfig, ListTerminalPresets, DetectTerminal, PickDirectory,
} from '../../bindings/clawdpanel/app.js';

export default {
  id: 'terminals',
  title: 'TERMINALS',
  async mount(body) {
    let config = await GetConfig();
    const presets = await ListTerminalPresets();
    config.launcher = config.launcher || { preset: '', exe: '', args: [] };
    const validKeys = new Set(presets.map(p => p.key));
    if (!config.launcher.preset || (!validKeys.has(config.launcher.preset) && config.launcher.preset !== 'custom')) {
      try { config.launcher = await DetectTerminal(); } catch (e) { /* keep empty */ }
    }
    let formMode = 'edit';

    body.innerHTML = `
      <div class="panel">
        <div class="panel-list" id="term-list">
          <label class="panel-row">
            <span class="panel-label">TERMINAL</span>
            <select id="term-select" class="editor-select panel-grow"></select>
          </label>
          <div class="panel-actions">
            <button id="term-edit" class="editor-btn">EDIT</button>
            <button id="term-add" class="editor-btn">ADD</button>
            <button id="term-del" class="editor-btn">DELETE</button>
          </div>
          <label class="panel-row">
            <span class="panel-label">PROGRAM</span>
            <select id="term-launcher" class="editor-select panel-grow" title="Terminal program used to open every entry"></select>
          </label>
          <div class="panel-form" id="term-custom" style="display:none">
            <label class="panel-row">
              <span class="panel-label">EXE</span>
              <input type="text" id="term-exe" class="editor-input panel-grow" placeholder="myterm">
            </label>
            <label class="panel-row">
              <span class="panel-label">ARGS</span>
              <input type="text" id="term-args" class="editor-input panel-grow" placeholder="--title {title} -d {dir} {cmd}">
            </label>
            <div class="panel-actions">
              <button id="term-custom-save" class="editor-btn editor-btn--accent">SET</button>
            </div>
          </div>
        </div>

        <div class="panel-form" id="term-form" style="display:none">
          <label class="panel-row">
            <span class="panel-label">NAME</span>
            <input type="text" id="term-name" class="editor-input panel-grow" placeholder="CRM">
          </label>
          <label class="panel-row">
            <span class="panel-label">DIR</span>
            <input type="text" id="term-dir" class="editor-input panel-grow" placeholder="~/proj">
            <button id="term-browse" class="editor-btn" title="Browse for a directory">…</button>
          </label>
          <label class="panel-row">
            <span class="panel-label">COLOR</span>
            <input type="color" id="term-color" class="editor-input editor-input--color" value="#3B82F6">
          </label>
          <div class="panel-actions">
            <button id="term-save" class="editor-btn editor-btn--accent">SAVE</button>
            <button id="term-cancel" class="editor-btn">CANCEL</button>
          </div>
        </div>
      </div>`;

    const $ = (id) => body.querySelector(id);
    const listEl = $('#term-list');
    const formEl = $('#term-form');
    const customEl = $('#term-custom');
    const select = $('#term-select');
    const launcher = $('#term-launcher');

    const selectedIdx = () => parseInt(select.value, 10);

    function renderSelect() {
      select.innerHTML = '';
      const terms = config.terminals || [];
      if (terms.length === 0) {
        const opt = document.createElement('option');
        opt.value = -1;
        opt.textContent = '(none — click ADD)';
        select.appendChild(opt);
        return;
      }
      terms.forEach((t, i) => {
        const opt = document.createElement('option');
        opt.value = i;
        opt.textContent = `${t.name} (${t.dir || '~'})`;
        select.appendChild(opt);
      });
    }

    function renderLauncher() {
      launcher.innerHTML = '';
      presets.forEach((p) => {
        const opt = document.createElement('option');
        opt.value = p.key;
        opt.textContent = p.label;
        launcher.appendChild(opt);
      });
      const cur = (config.launcher && config.launcher.preset) || '';
      if (cur) launcher.value = cur;
      toggleCustom();
    }

    function toggleCustom() {
      const isCustom = launcher.value === 'custom';
      const listVisible = listEl.style.display !== 'none';
      customEl.style.display = (isCustom && listVisible) ? 'flex' : 'none';
      if (isCustom) {
        const l = config.launcher || {};
        $('#term-exe').value = l.exe || '';
        $('#term-args').value = (l.args || []).join(' ');
      }
    }

    function showList() {
      formEl.style.display = 'none';
      listEl.style.display = 'flex';
      toggleCustom();
    }

    function showForm(mode) {
      formMode = mode;
      listEl.style.display = 'none';
      formEl.style.display = 'flex';
      customEl.style.display = 'none';
      const nameI = $('#term-name');
      const dirI = $('#term-dir');
      const colorI = $('#term-color');
      if (mode === 'add') {
        nameI.value = '';
        dirI.value = '~';
        colorI.value = '#3B82F6';
      } else {
        const idx = selectedIdx();
        const terms = config.terminals || [];
        if (idx >= 0 && idx < terms.length) {
          nameI.value = terms[idx].name || '';
          dirI.value = terms[idx].dir || '';
          colorI.value = terms[idx].color || '#3B82F6';
        }
      }
      nameI.focus();
    }

    async function persist() {
      try {
        await SaveConfig(config);
        return true;
      } catch (err) {
        console.error('Failed to save terminals:', err);
        alert('Error saving: ' + err);
        return false;
      }
    }

    async function save() {
      const name = $('#term-name').value.trim();
      const dir = $('#term-dir').value.trim();
      const color = $('#term-color').value;
      if (!name) { alert('Name cannot be empty!'); return; }
      const terms = config.terminals || [];
      if (formMode === 'add') {
        terms.push({ name, color, dir });
      } else {
        const idx = selectedIdx();
        if (idx >= 0 && idx < terms.length) {
          const command = terms[idx].command || '';
          terms[idx] = command ? { name, color, dir, command } : { name, color, dir };
        }
      }
      config.terminals = terms;
      if (await persist()) { renderSelect(); showList(); }
    }

    async function del() {
      const terms = config.terminals || [];
      const idx = selectedIdx();
      if (idx < 0 || idx >= terms.length) return;
      if (!confirm(`Delete terminal "${terms[idx].name}"?`)) return;
      terms.splice(idx, 1);
      config.terminals = terms;
      if (await persist()) { renderSelect(); showList(); }
    }

    $('#term-edit').addEventListener('click', () => {
      if (selectedIdx() < 0) { alert('No terminal to edit — click ADD.'); return; }
      showForm('edit');
    });
    $('#term-add').addEventListener('click', () => showForm('add'));
    $('#term-del').addEventListener('click', del);
    $('#term-save').addEventListener('click', save);
    $('#term-cancel').addEventListener('click', showList);
    $('#term-browse').addEventListener('click', async () => {
      try {
        const dir = await PickDirectory();
        if (dir) $('#term-dir').value = dir;
      } catch (err) {
        console.error('Directory picker failed:', err);
      }
    });

    launcher.addEventListener('change', async () => {
      config.launcher = config.launcher || {};
      config.launcher.preset = launcher.value;
      toggleCustom();
      if (launcher.value !== 'custom') {
        config.launcher.exe = '';
        config.launcher.args = [];
        await persist();
      }
    });

    $('#term-custom-save').addEventListener('click', async () => {
      const exe = $('#term-exe').value.trim();
      const argsStr = $('#term-args').value.trim();
      if (!exe) { alert('Custom terminal needs an executable.'); return; }
      config.launcher = {
        preset: 'custom',
        exe,
        args: argsStr ? argsStr.split(/\s+/) : [],
      };
      await persist();
    });

    renderSelect();
    renderLauncher();
    showList();
  },
};
