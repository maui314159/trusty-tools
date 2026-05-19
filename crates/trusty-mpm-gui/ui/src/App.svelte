<script lang="ts">
  // Why: The Tauri desktop entrypoint component. It renders the same Dashboard
  // tree as the web build but is the natural seam for wiring native Tauri
  // event listeners (window events, deep links) that the browser build lacks.
  // What: Mounts Dashboard. Tauri-specific listeners can be registered here in
  // onMount via `@tauri-apps/api/event` without touching shared components.
  // Test: Launch under Tauri → the dashboard renders inside the native window
  // and `transport.invoke` routes to Rust commands.
  import Dashboard from './components/Dashboard.svelte';
</script>

<Dashboard />
