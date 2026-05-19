<script lang="ts">
  // Why: The standalone browser entrypoint component. It renders the identical
  // Dashboard tree as the desktop build; the only difference is the absence of
  // the Tauri runtime, so `transport.invoke` falls through to REST/SSE.
  // What: Mounts Dashboard with no Tauri wiring.
  // Test: Serve `dist-web/` and open in a browser → the dashboard renders and
  // polls the daemon over plain REST.
  import Dashboard from './components/Dashboard.svelte';
</script>

<Dashboard />
