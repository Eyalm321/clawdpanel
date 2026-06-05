// Stations panel for the settings popup. Ported from the old inline bar editor
// (main.js). A station has a NAME and an ordered list of video/playlist URLs.
// URLs are validated server-side via ParseStationItem on save. (Shuffle is no
// longer a per-station setting — it's the bar's one-shot shuffle button.)
import { GetConfig, SaveConfig, ParseStationItem } from '../../bindings/clawdpanel/app.js';

export default {
  id: 'stations',
  title: 'STATIONS',
  async mount(body) {
    let config = await GetConfig();
    let formMode = 'edit';

    body.innerHTML = `
      <div class="panel">
        <div class="panel-list" id="st-list">
          <label class="panel-row">
            <span class="panel-label">STATION</span>
            <select id="st-select" class="editor-select panel-grow"></select>
          </label>
          <div class="panel-actions">
            <button id="st-edit" class="editor-btn">EDIT</button>
            <button id="st-add" class="editor-btn">ADD</button>
            <button id="st-del" class="editor-btn">DELETE</button>
          </div>
        </div>

        <div class="panel-form" id="st-form" style="display:none">
          <label class="panel-row">
            <span class="panel-label">NAME</span>
            <input type="text" id="st-name" class="editor-input panel-grow" placeholder="MY RADIO">
          </label>
          <div class="panel-row panel-row--top">
            <span class="panel-label">URLS</span>
            <div class="panel-grow">
              <div id="st-urls" class="station-url-list"></div>
              <button id="st-url-add" class="editor-btn" title="Add another video or playlist URL">+ URL</button>
            </div>
          </div>
          <div class="panel-actions">
            <button id="st-save" class="editor-btn editor-btn--accent">SAVE</button>
            <button id="st-cancel" class="editor-btn">CANCEL</button>
          </div>
        </div>
      </div>`;

    const $ = (id) => body.querySelector(id);
    const listEl = $('#st-list');
    const formEl = $('#st-form');
    const select = $('#st-select');
    const urlsEl = $('#st-urls');

    const selectedIdx = () => parseInt(select.value, 10);

    function renderSelect() {
      select.innerHTML = '';
      const list = config.stations || [];
      if (list.length === 0) {
        const opt = document.createElement('option');
        opt.value = -1;
        opt.textContent = '(none — click ADD)';
        select.appendChild(opt);
        return;
      }
      list.forEach((st, i) => {
        const opt = document.createElement('option');
        opt.value = i;
        const count = (st.items || []).length;
        opt.textContent = `${st.name} (${count} item${count === 1 ? '' : 's'})`;
        select.appendChild(opt);
      });
    }

    function addUrlInput(value) {
      const row = document.createElement('div');
      row.className = 'station-url-row';
      const input = document.createElement('input');
      input.type = 'text';
      input.className = 'editor-input station-url-input panel-grow';
      input.placeholder = 'youtube.com/watch?v=… or playlist?list=…';
      input.value = value || '';
      const del = document.createElement('button');
      del.className = 'station-url-del';
      del.textContent = '✕';
      del.title = 'Remove this URL';
      del.addEventListener('click', () => row.remove());
      row.appendChild(input);
      row.appendChild(del);
      urlsEl.appendChild(row);
    }

    function collectUrls() {
      const out = [];
      urlsEl.querySelectorAll('.station-url-input').forEach((el) => {
        const v = el.value.trim();
        if (v) out.push(v);
      });
      return out;
    }

    function showList() {
      formEl.style.display = 'none';
      listEl.style.display = 'flex';
    }

    function showForm(mode) {
      formMode = mode;
      listEl.style.display = 'none';
      formEl.style.display = 'flex';
      const nameI = $('#st-name');
      urlsEl.innerHTML = '';
      if (mode === 'add') {
        nameI.value = '';
        addUrlInput('');
      } else {
        const idx = selectedIdx();
        const list = config.stations || [];
        if (idx >= 0 && idx < list.length) {
          const st = list[idx];
          nameI.value = st.name || '';
          const items = st.items || [];
          if (items.length === 0) addUrlInput('');
          else items.forEach((it) => addUrlInput(it.raw || it.id || ''));
        }
      }
      nameI.focus();
    }

    async function save() {
      const name = $('#st-name').value.trim();
      if (!name) { alert('Station name cannot be empty!'); return; }
      const urls = collectUrls();
      if (urls.length === 0) { alert('Add at least one video or playlist URL.'); return; }
      const items = [];
      for (const u of urls) {
        try {
          items.push(await ParseStationItem(u));
        } catch (e) {
          alert(`Invalid YouTube URL/ID:\n${u}\n\n${e}`);
          return;
        }
      }
      const list = config.stations || [];
      if (formMode === 'add') {
        // Shuffle is toggled from the bar, not here — new stations start off.
        list.push({ name, items, shuffle: false });
      } else {
        const idx = selectedIdx();
        // Preserve the bar-driven shuffle flag across detail edits.
        if (idx >= 0 && idx < list.length) {
          list[idx] = { name, items, shuffle: !!list[idx].shuffle };
        }
      }
      config.stations = list;
      try {
        await SaveConfig(config);
        renderSelect();
        showList();
      } catch (err) {
        console.error('Failed to save station:', err);
        alert('Error saving station: ' + err);
      }
    }

    async function del() {
      const list = config.stations || [];
      const idx = selectedIdx();
      if (idx < 0 || idx >= list.length) return;
      if (!confirm(`Delete station "${list[idx].name}"?`)) return;
      list.splice(idx, 1);
      if ((config.activeStation || 0) >= list.length) config.activeStation = 0;
      config.stations = list;
      try {
        await SaveConfig(config);
        renderSelect();
        showList();
      } catch (err) {
        console.error('Failed to delete station:', err);
        alert('Error deleting station: ' + err);
      }
    }

    $('#st-edit').addEventListener('click', () => {
      if (selectedIdx() < 0) { alert('No station to edit — click ADD.'); return; }
      showForm('edit');
    });
    $('#st-add').addEventListener('click', () => showForm('add'));
    $('#st-del').addEventListener('click', del);
    $('#st-url-add').addEventListener('click', () => addUrlInput(''));
    $('#st-save').addEventListener('click', save);
    $('#st-cancel').addEventListener('click', showList);

    renderSelect();
    showList();
  },
};
