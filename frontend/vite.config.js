import { defineConfig } from 'vite';
import { resolve } from 'path';
import { fileURLToPath } from 'url';

const __dirname = fileURLToPath(new URL('.', import.meta.url));

// `wails3 dev` assigns the frontend dev-server port and proxies the embedded
// asset server to it. It exposes that as FRONTEND_DEVSERVER_URL (and, on some
// versions, VITE_PORT). Bind whichever it gives us so the Go side can connect;
// without this the dev server comes up on vite's default port and `wails3 dev`
// reports "unable to connect to frontend server".
function devServerPort() {
  const url = process.env.FRONTEND_DEVSERVER_URL;
  if (url) {
    try {
      const p = Number(new URL(url).port);
      if (p) return p;
    } catch { /* ignore malformed URL */ }
  }
  const vp = Number(process.env.VITE_PORT);
  return vp || undefined;
}
const devPort = devServerPort();

export default defineConfig({
  base: './',
  server: devPort ? { port: devPort, strictPort: true } : undefined,
  resolve: {
    alias: {
      // Resolve the runtime to the copy the Go binary serves at /wails/
      // runtime.js — the same one the generated bindings use. The npm
      // package stopped publishing at 3.0.0-alpha.79 while the Go side
      // bundles newer runtimes; loading both gives two runtime instances
      // whose event protocols drift apart (Go-emitted events never reach
      // Events.On). One served runtime = always version-matched.
      '@wailsio/runtime': '/wails/runtime.js',
    },
  },
  build: {
    rollupOptions: {
      // Three HTML entries: the bar (index.html), the settings popup
      // (settings.html) and the brand dropdown (menu.html), all built into dist/
      // and served by the Go asset server.
      input: {
        main: resolve(__dirname, 'index.html'),
        settings: resolve(__dirname, 'settings.html'),
        menu: resolve(__dirname, 'menu.html'),
        update: resolve(__dirname, 'update.html'),
      },
      external: [
        '/wails/runtime.js',
        '/wails/transport.js'
      ]
    }
  }
});
