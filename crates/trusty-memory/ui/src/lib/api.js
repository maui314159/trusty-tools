/*
 * Why: All UI components hit the same trusty-memory daemon REST surface;
 * centralizing fetch logic gives us one place to handle errors and JSON
 * parsing. The daemon serves the SPA at `/` and the API under `/api/v1/*`
 * (plus `/health`), so requests are always same-origin in production. In
 * `vite dev`, vite.config.js proxies `/api` and `/health` to the daemon.
 * What: Thin wrappers returning parsed JSON or throwing on non-2xx.
 * Test: Console-call api.health() and confirm the shape matches /health.
 */

async function request(path, opts = {}) {
  const res = await fetch(path, {
    headers: { 'Content-Type': 'application/json', ...(opts.headers || {}) },
    ...opts
  });
  if (!res.ok) {
    let detail = '';
    try {
      detail = await res.text();
    } catch {
      /* ignore */
    }
    throw new Error(`${res.status} ${res.statusText}: ${detail}`);
  }
  if (res.status === 204) return null;
  const ct = res.headers.get('content-type') || '';
  if (ct.includes('application/json')) return res.json();
  return res.text();
}

export const api = {
  /** Daemon liveness + resource metrics. */
  health: () => request('/health'),

  /** Aggregate daemon status (palace/drawer/vector/triple counts). */
  status: () => request('/api/v1/status'),

  /** Daemon configuration (provider, model, data root). */
  config: () => request('/api/v1/config'),

  /** List all memory palaces with their metadata + counts. */
  listPalaces: () => request('/api/v1/palaces'),

  /** Single palace detail by id. */
  getPalace: (id) => request(`/api/v1/palaces/${encodeURIComponent(id)}`),

  /**
   * List drawers within a palace. Optional `room` narrows to one room;
   * `limit` caps the result count.
   */
  listDrawers: (id, { room, tag, limit } = {}) => {
    const params = new URLSearchParams();
    if (room) params.set('room', room);
    if (tag) params.set('tag', tag);
    if (limit) params.set('limit', String(limit));
    const qs = params.toString();
    return request(
      `/api/v1/palaces/${encodeURIComponent(id)}/drawers${qs ? `?${qs}` : ''}`
    );
  },

  /** Tail the daemon's in-memory log ring buffer. */
  logsTail: (n = 200) =>
    request(`/api/v1/logs/tail?n=${encodeURIComponent(n)}`),

  /** Aggregate dream-cycle stats across all palaces. */
  dreamStatus: () => request('/api/v1/dream/status'),

  /** Trigger a dream cycle across all palaces and return aggregate stats. */
  dreamRun: () => request('/api/v1/dream/run', { method: 'POST' }),

  /** Request a graceful daemon shutdown. */
  stopDaemon: () => request('/api/v1/admin/stop', { method: 'POST' })
};
