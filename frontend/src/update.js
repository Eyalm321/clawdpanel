import { Window, Events } from '@wailsio/runtime';
import { applyTheme } from './settings/shell.js';
import { GetLastUpdateResult, InstallUpdate } from '../bindings/clawdpanel/app.js';

let updateInfo = null;

function updateProgressUI(pct, downloaded, total) {
  const bar = document.getElementById('progress-bar-fill');
  const percentText = document.getElementById('progress-percent');
  const detailsText = document.getElementById('progress-details');
  
  if (bar) bar.style.width = pct + '%';
  if (percentText) percentText.textContent = Math.round(pct) + '%';
  if (detailsText) {
    detailsText.textContent = `${downloaded.toFixed(2)} / ${total.toFixed(2)} MB`;
  }
}

async function startUpdate() {
  if (!updateInfo || !updateInfo.downloadUrl) return;
  
  // Hide details, show progress
  document.getElementById('panel-details').style.display = 'none';
  document.getElementById('panel-progress').style.display = 'flex';
  
  try {
    await InstallUpdate(updateInfo.downloadUrl);
  } catch (err) {
    console.error('Update failed:', err);
    document.getElementById('progress-status').textContent = 'UPDATE FAILED';
    document.getElementById('progress-status').style.color = 'var(--color-error, #ff3333)';
    
    const detailsText = document.getElementById('progress-details');
    if (detailsText) detailsText.textContent = String(err);
  }
}

function closeWindow() {
  try { Window.Hide(); } catch (e) { console.error('hide failed', e); }
}

async function init() {
  applyTheme();
  
  // Retrieve version and release info from the cached result
  try {
    updateInfo = await GetLastUpdateResult();
    if (updateInfo) {
      document.getElementById('val-current').textContent = 'v' + updateInfo.current;
      document.getElementById('val-latest').textContent = 'v' + updateInfo.latest;
      document.getElementById('val-changelog').textContent = updateInfo.changelog || 'No release notes provided.';
    }
  } catch (err) {
    console.error('Failed to get update info:', err);
  }
  
  // Event listeners
  document.getElementById('modal-close').addEventListener('click', closeWindow);
  document.getElementById('btn-later').addEventListener('click', closeWindow);
  document.getElementById('btn-update').addEventListener('click', startUpdate);
  
  // Esc key dismiss
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeWindow();
  });
  
  // Listen for Go download progress events
  Events.On('update:progress', (event) => {
    const data = event ? event.data : null;
    if (data) {
      updateProgressUI(data.percent, data.downloaded, data.total);
    }
  });
}

document.addEventListener('DOMContentLoaded', init);
