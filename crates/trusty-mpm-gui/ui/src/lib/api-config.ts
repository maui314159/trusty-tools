// Why: Centralizes daemon-URL resolution and runtime Tauri detection so every
// other module agrees on where the daemon lives and which transport to use.
// What: Exposes the default daemon URL, an `isTauri()` runtime check, and an
// `apiBase()` accessor that honors a user override stored in localStorage.
// Test: With localStorage empty, `apiBase()` returns DEFAULT_DAEMON_URL; after
// setting `trusty-mpm.daemonUrl`, it returns the stored value.

export const DEFAULT_DAEMON_URL = 'http://127.0.0.1:7880';

/** True when running inside the Tauri desktop runtime (v2 internals present). */
export const isTauri = (): boolean =>
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/** Resolve the daemon base URL — user override wins over the default. */
export function apiBase(): string {
  if (typeof localStorage === 'undefined') return DEFAULT_DAEMON_URL;
  return localStorage.getItem('trusty-mpm.daemonUrl') ?? DEFAULT_DAEMON_URL;
}
