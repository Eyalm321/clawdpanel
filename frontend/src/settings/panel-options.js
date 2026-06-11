// Bar Options panel for the settings popup. Toggles which optional bar segments
// are active. Each toggle maps to a config.features flag; saving fires
// "config:changed", which re-renders the bar AND (for Radio) brings the backing
// resource up/down on the Go side — disabling Radio stops the native audio
// engine, not just the segment.
import { GetConfig, SaveConfig } from '../../bindings/clawdpanel/app.js';

// key → label, in display order. Keys match config.FeatureConfig json tags.
const FEATURES = [
  { key: 'radio', label: 'RADIO' },
  { key: 'monitor', label: 'MONITOR' },
  { key: 'theme', label: 'THEME' },
  { key: 'weeklyUsage', label: 'WEEKLY USAGE' },
  { key: 'hourlyUsage', label: '5H USAGE' },
];

export default {
  id: 'options',
  title: 'BAR OPTIONS',
  navLabel: 'OPTIONS',
  async mount(body) {
    let config = await GetConfig();
    const features = config.features || {};

    const rows = FEATURES.map(({ key, label }) => `
      <label class="panel-row">
        <span class="panel-label">${label}</span>
        <input type="checkbox" class="panel-check" data-feature="${key}" ${features[key] !== false ? 'checked' : ''}>
      </label>`).join('');

    body.innerHTML = `
      <div class="panel">
        <div class="panel-hint">Show or hide bar segments. Disabling <b>Radio</b> stops the background audio engine, not just the player.</div>
        <div class="panel-list">${rows}</div>
      </div>`;

    body.querySelectorAll('.panel-check[data-feature]').forEach((cb) => {
      cb.addEventListener('change', async () => {
        config.features = config.features || {};
        config.features[cb.dataset.feature] = cb.checked;
        try {
          await SaveConfig(config);
        } catch (err) {
          console.error('Failed to save bar options:', err);
          alert('Error saving options: ' + err);
          cb.checked = !cb.checked; // revert the toggle on failure
        }
      });
    });
  },
};
