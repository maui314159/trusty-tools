<script>
  /*
   * Why: The daemon broadcasts DrawerAdded / PalaceCreated / DreamCompleted
   * / StatusChanged over `/sse`. With many palaces active simultaneously,
   * operators need a filter input so they can focus on a single palace's
   * activity, plus an "Active only" toggle that hides palaces that haven't
   * written during the current session.
   * What: Connects to `/sse` via EventSource, parses each JSON frame, and
   * prepends it to a ring buffer capped at MAX_EVENTS. Filters by palace
   * name substring; "Active only" hides events from palaces not seen since
   * the page loaded. Auto-reconnects with exponential backoff on error.
   * Test: open the SPA, post a drawer or palace, confirm the feed prepends
   * a new row within ~1s. Type a substring and confirm filtering. Toggle
   * "Active only" and confirm only session-active palaces appear.
   */
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../api.js';

  const MAX_EVENTS = 100;
  // Exponential backoff: 1s, 2s, 4s, 8s, ..., capped at 30s.
  const BACKOFF_MIN_MS = 1000;
  const BACKOFF_MAX_MS = 30_000;

  let events = $state([]);
  let connected = $state(false);
  let backoffMs = BACKOFF_MIN_MS;
  let collapsed = $state(false);
  let source = null;
  let reconnectTimer = null;

  // Filter UI state.
  let filterText = $state('');
  let activeOnly = $state(false);

  // id -> name lookup, populated at mount from /api/v1/palaces. Used for
  // events whose `palace_name` field is missing (older daemons).
  let palaceNames = $state({});

  // Set of palace ids that have emitted at least one event this session.
  // Used by the "Active only" toggle.
  let activeIds = $state(new Set());

  /**
   * Why: New events should appear at the top; old events fall off when we
   * exceed the cap.
   * What: Prepend `evt` and slice to MAX_EVENTS. Records palace id as
   * "session-active" so the Active toggle has data to filter on.
   * Test: push MAX_EVENTS+5 entries, assert length === MAX_EVENTS.
   */
  function pushEvent(evt) {
    const stamped = { ...evt, _ts: Date.now(), _id: `${Date.now()}-${Math.random()}` };
    events = [stamped, ...events].slice(0, MAX_EVENTS);
    const pid = evt?.palace_id;
    if (pid && !activeIds.has(pid)) {
      const next = new Set(activeIds);
      next.add(pid);
      activeIds = next;
    }
  }

  function connect() {
    if (source) {
      try {
        source.close();
      } catch {
        /* ignore */
      }
      source = null;
    }

    try {
      source = new EventSource('/sse');
    } catch (e) {
      scheduleReconnect();
      return;
    }

    source.onopen = () => {
      connected = true;
      backoffMs = BACKOFF_MIN_MS;
    };

    source.onmessage = (msg) => {
      let parsed;
      try {
        parsed = JSON.parse(msg.data);
      } catch {
        return;
      }
      // Initial connect frame from the daemon — informational only.
      if (parsed?.type === 'connected') return;
      pushEvent(parsed);
    };

    source.onerror = () => {
      connected = false;
      try {
        source?.close();
      } catch {
        /* ignore */
      }
      source = null;
      scheduleReconnect();
    };
  }

  function scheduleReconnect() {
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, backoffMs);
    backoffMs = Math.min(backoffMs * 2, BACKOFF_MAX_MS);
  }

  /**
   * Why: Older daemons don't include `palace_name` in DrawerAdded frames;
   * the feed needs a fallback id→name table so the description stays
   * readable.
   * What: Fetches `/api/v1/palaces` once at mount, builds a id→name map.
   * Test: open the SPA, watch network tab — listPalaces fires once.
   */
  async function loadPalaceNames() {
    try {
      const list = await api.listPalaces();
      const map = {};
      for (const p of list || []) {
        if (p?.id) map[p.id] = p.name || p.id;
      }
      palaceNames = map;
    } catch {
      // Best-effort; feed still works with raw palace ids.
    }
  }

  onMount(() => {
    loadPalaceNames();
    connect();
  });

  onDestroy(() => {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (source) {
      try {
        source.close();
      } catch {
        /* ignore */
      }
      source = null;
    }
  });

  /**
   * Why: Raw epoch ms is unreadable; operators want "2s ago" / "3m ago".
   * What: Returns a short relative-time string. Accepts either a numeric
   * epoch ms or an ISO-8601 string (for `timestamp` field on DrawerAdded).
   * Test: relTime(now - 1500) starts with "1s".
   */
  function relTime(ts) {
    let t;
    if (typeof ts === 'string') {
      t = new Date(ts).getTime();
    } else {
      t = ts;
    }
    if (!Number.isFinite(t)) return '—';
    const diff = Math.max(0, Date.now() - t);
    const s = Math.floor(diff / 1000);
    if (s < 60) return `${s}s ago`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m}m ago`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}h ago`;
    return `${Math.floor(h / 24)}d ago`;
  }

  /**
   * Why: Each event has a `palace_id` (sometimes) and we want to render
   * the friendly name (Palace.name) in the feed. The daemon now includes
   * `palace_name` on DrawerAdded; older frames fall back to the cached
   * id→name map.
   * What: returns the best-known label for a palace id.
   * Test: labelFor({palace_id:"x", palace_name:"X"}) === "X".
   */
  function labelFor(evt) {
    return evt?.palace_name || palaceNames[evt?.palace_id] || evt?.palace_id || '';
  }

  /**
   * Why: Per-event-type rendering keeps the feed scannable; emoji + short
   * description matches the spec.
   * What: Returns { icon, label, description } for a given event.
   * Test: describe({type:"palace_created", name:"foo"}).description includes "foo".
   */
  function describe(evt) {
    switch (evt.type) {
      case 'drawer_added': {
        const name = labelFor(evt);
        return {
          icon: '🧠',
          label: 'drawer',
          description: `${name} — ${evt.drawer_count} drawers`,
          link: evt?.palace_id ? `#/palaces/${evt.palace_id}` : null
        };
      }
      case 'drawer_deleted': {
        const name = labelFor(evt);
        return {
          icon: '🗑',
          label: 'drawer',
          description: `${name} — ${evt.drawer_count} drawers`,
          link: evt?.palace_id ? `#/palaces/${evt.palace_id}` : null
        };
      }
      case 'palace_created':
        return {
          icon: '🏛',
          label: 'palace',
          description: `${evt.name ?? evt.id} created`,
          link: evt?.id ? `#/palaces/${evt.id}` : null
        };
      case 'dream_completed':
        return {
          icon: '💭',
          label: 'dream',
          description: `merged ${evt.merged ?? 0}, pruned ${evt.pruned ?? 0}, compacted ${evt.compacted ?? 0}`,
          link: null
        };
      case 'status_changed':
        return {
          icon: '⚡',
          label: 'status',
          description: `${evt.total_drawers ?? 0} drawers · ${evt.total_vectors ?? 0} vectors · ${evt.total_kg_triples ?? 0} triples`,
          link: null
        };
      case 'lag':
        return {
          icon: '⏱',
          label: 'lag',
          description: `dropped ${evt.skipped ?? 0} events (slow consumer)`,
          link: null
        };
      default:
        return {
          icon: '·',
          label: evt.type ?? 'event',
          description: JSON.stringify(evt).slice(0, 120),
          link: null
        };
    }
  }

  // Periodically force re-render so relative timestamps tick.
  let _tick = $state(0);
  let tickTimer = null;
  onMount(() => {
    tickTimer = setInterval(() => {
      _tick += 1;
    }, 1000);
  });
  onDestroy(() => {
    if (tickTimer) clearInterval(tickTimer);
  });

  // Derived list applying filter + activeOnly.
  let visibleEvents = $derived.by(() => {
    const f = filterText.trim().toLowerCase();
    return events.filter((evt) => {
      if (activeOnly) {
        const pid = evt?.palace_id;
        // Always keep events without a palace_id (palace_created, dream, etc.)
        // when activeOnly is on, since they carry useful daemon-level signals.
        if (pid && !activeIds.has(pid)) return false;
      }
      if (!f) return true;
      const haystack = `${evt?.palace_id ?? ''} ${labelFor(evt)} ${evt?.name ?? ''}`.toLowerCase();
      return haystack.includes(f);
    });
  });

  /**
   * Why: For DrawerAdded the daemon now sends a `timestamp` ISO string; we
   * prefer it over the SSE-arrival time because it's the wall-clock time of
   * the write (more accurate when the SSE channel has lag).
   * What: returns ISO `timestamp` if present, otherwise the local arrival ms.
   * Test: bestTime({timestamp:'2026-01-01T00:00:00Z', _ts: 0}) === iso.
   */
  function bestTime(evt) {
    return evt?.timestamp || evt?._ts;
  }
</script>

<aside class="feed" class:collapsed>
  <div class="feed-head">
    <div class="title">
      <span class="dot" class:online={connected} class:offline={!connected}></span>
      Activity
    </div>
    <div class="head-right">
      <span class="status-text">
        {#if connected}live{:else}reconnecting…{/if}
      </span>
      <button
        type="button"
        class="toggle"
        aria-label="Toggle activity feed"
        onclick={() => (collapsed = !collapsed)}
      >
        {collapsed ? '▸' : '▾'}
      </button>
    </div>
  </div>

  {#if !collapsed}
    <div class="feed-controls">
      <input
        type="search"
        class="filter-input"
        placeholder="🔍 Filter by palace…"
        bind:value={filterText}
      />
      <label class="toggle-row" title="Hide palaces with no activity this session">
        <input type="checkbox" bind:checked={activeOnly} />
        <span>Active only</span>
      </label>
    </div>

    <div class="feed-body">
      {#if visibleEvents.length === 0}
        <div class="empty">
          {#if events.length === 0}
            {#if connected}
              Waiting for events…
            {:else}
              Disconnected — retrying.
            {/if}
          {:else}
            No events match the filter.
          {/if}
        </div>
      {:else}
        <ul class="event-list">
          {#each visibleEvents as evt (evt._id)}
            {@const d = describe(evt)}
            <li class="event-row">
              <span class="icon" aria-hidden="true">{d.icon}</span>
              <div class="event-main">
                <div class="event-line">
                  <span class="badge">{d.label}</span>
                  {#if d.link}
                    <a class="desc link" href={d.link}>{d.description}</a>
                  {:else}
                    <span class="desc">{d.description}</span>
                  {/if}
                </div>
                <!-- _tick is referenced so Svelte re-renders this row each second -->
                <div class="event-time">{relTime(bestTime(evt))}{_tick > -1 ? '' : ''}</div>
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
  {/if}
</aside>

<style>
  .feed {
    position: fixed;
    right: 0;
    top: 0;
    bottom: 0;
    width: 320px;
    background: var(--trusty-content-bg, #fff);
    border-left: 1px solid var(--trusty-border, #e5e7eb);
    display: flex;
    flex-direction: column;
    z-index: 10;
    transition: width 0.2s ease;
  }
  .feed.collapsed {
    width: 200px;
  }
  .feed-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--trusty-border, #e5e7eb);
    font-size: 13px;
    font-weight: 600;
    background: var(--trusty-bg-subtle, #fafafa);
  }
  .feed-controls {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--trusty-border, #e5e7eb);
    background: var(--trusty-bg-subtle, #fafafa);
  }
  .filter-input {
    width: 100%;
    padding: 4px 8px;
    border-radius: 4px;
    border: 1px solid var(--trusty-border, #e5e7eb);
    font-size: 12px;
  }
  .toggle-row {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    color: var(--trusty-text-secondary, #6b7280);
    cursor: pointer;
  }
  .title {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .head-right {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .status-text {
    font-size: 11px;
    color: var(--trusty-text-secondary, #6b7280);
    font-weight: 400;
  }
  .toggle {
    background: transparent;
    border: none;
    color: var(--trusty-text-secondary, #6b7280);
    cursor: pointer;
    font-size: 12px;
    padding: 2px 4px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .dot.online {
    background: var(--trusty-success, #10b981);
    box-shadow: 0 0 4px rgba(16, 185, 129, 0.5);
  }
  .dot.offline {
    background: var(--trusty-warning, #f59e0b);
  }
  .feed-body {
    flex: 1;
    overflow-y: auto;
    padding: 8px 0;
  }
  .empty {
    text-align: center;
    color: var(--trusty-text-muted, #9ca3af);
    font-size: 12px;
    padding: 24px 16px;
  }
  .event-list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .event-row {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 8px 16px;
    border-bottom: 1px solid var(--trusty-border-light, #f3f4f6);
    font-size: 12px;
  }
  .event-row:last-child {
    border-bottom: none;
  }
  .icon {
    flex: 0 0 18px;
    font-size: 14px;
    line-height: 18px;
  }
  .event-main {
    flex: 1;
    min-width: 0;
  }
  .event-line {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .badge {
    display: inline-block;
    padding: 1px 6px;
    border-radius: 4px;
    background: var(--trusty-bg-subtle, #f3f4f6);
    color: var(--trusty-text-secondary, #6b7280);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .desc {
    color: var(--trusty-text, #111827);
    overflow: hidden;
    text-overflow: ellipsis;
    word-break: break-word;
  }
  .desc.link {
    color: var(--trusty-accent, #4f46e5);
    text-decoration: none;
  }
  .desc.link:hover {
    text-decoration: underline;
  }
  .event-time {
    color: var(--trusty-text-muted, #9ca3af);
    font-size: 10px;
    margin-top: 2px;
  }
</style>
