// Why: Standalone browser entrypoint — mounts the REST-only shell so the same
// dashboard is reachable from a plain browser without the Tauri runtime.
// What: Imports global styles and mounts WebApp.svelte onto #app.
// Test: `pnpm build:web` then serve dist-web/ — the page loads and polls the
// daemon over REST.
import './app.css';
import WebApp from './WebApp.svelte';

const app = new WebApp({
  target: document.getElementById('app') as HTMLElement,
});

export default app;
