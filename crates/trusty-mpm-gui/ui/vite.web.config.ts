import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// Standalone browser build — entry is web.html (mounts WebApp.svelte) and
// output goes to ./dist-web. No Tauri plugin: the web bundle talks to the
// daemon over plain REST/SSE via transport.ts.
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist-web',
    emptyOutDir: true,
    rollupOptions: {
      input: 'web.html',
    },
  },
  define: {
    __WEB_MODE__: true,
  },
  root: '.',
});
