// Why: Tauri desktop entrypoint — mounts the Tauri-aware shell so the app
// renders inside the native window.
// What: Imports global styles and mounts App.svelte onto #app.
// Test: `pnpm dev` then launch Tauri — the window shows the dashboard header.
import './app.css';
import App from './App.svelte';

const app = new App({
  target: document.getElementById('app') as HTMLElement,
});

export default app;
