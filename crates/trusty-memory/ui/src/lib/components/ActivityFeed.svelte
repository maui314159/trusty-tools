<script>
  /*
   * Why: The daemon already broadcasts DrawerAdded / PalaceCreated /
   * DreamCompleted / StatusChanged over `/sse`, but no view consumes it. A
   * persistent activity feed gives operators live evidence the daemon is
   * doing work without forcing them to refresh tables.
   * What: Connects to `/sse` via EventSource, parses each JSON frame, and
   * prepends it to a ring buffer capped at MAX_EVENTS. Auto-reconnects with
   * exponential backoff on error.
   * Test: open the SPA, post a drawer or palace, confirm the feed prepends
   * a new row within ~1s. Disconnect the daemon to verify the reconnecting
   * indicator appears.
   */
  import { onMount, onDestroy } from 'svelte';

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

  /**
   * Why: New events should appear at the top; old events fall off when we
   * exceed the cap.
   * What: Prepend `evt` and slice to MAX_EVENTS.
   * Test: push MAX_EVENTS+5 entries, assert length === MAX_EVENTS.
   */
  function pushEvent(evt) {
    const stamped = { ...evt, _ts: Date.now(), _id: `${Date.now()}-${Math.random()}` };
    events = [stamped, ...events].slice(0, MAX_EVENTS);
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

  onMount(() => {
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
   * What: Returns a short relative-time string.
   * Test: relTime(now - 1500) starts with "1s".
   */
  function relTime(ts) {
    const diff = Math.max(0, Date.now() - ts);
    const s = Math.floor(diff / 1000);
    if (s < 60) return `${s}s ago`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m}m ago`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}h ago`;
    return `${Math.floor(h / 24)}d ago`;
  }

  /**
   * Why: Per-event-type rendering keeps the feed scannable; emoji + short
   * description matches the spec.
   * What: Returns { icon, label, description } for a given event.
   * Test: describe({type:"palace_created", name:"foo"}).description includes "foo".
   */
  function describe(evt) {
    switch (evt.type) {
      case 'drawer_added':
        return {
          icon: '🧠',
          label: 'drawer',
          description: `${evt.palace_id} — ${evt.drawer_count} drawers`
        };
      case 'drawer_deleted':
        return {
          icon: '🗑',
          label: 'drawer',
          description: `${evt.palace_id} — ${evt.drawer_count} drawers`
        };
      case 'palace_created':
        return {
          icon: '🏛',
          label: 'palace',
          description: `${evt.name ?? evt.id} created`
        };
      case 'dream_completed':
        return {
          icon: '💭',
          label: 'dream',
          description: `merged ${evt.merged ?? 0}, pruned ${evt.pruned ?? 0}, compacted ${evt.compacted ?? 0}`
        };
      case 'status_changed':
        return {
          icon: '⚡',
          label: 'status',
          description: `${evt.total_drawers ?? 0} drawers · ${evt.total_vectors ?? 0} vectors · ${evt.total_kg_triples ?? 0} triples`
        };
      case 'lag':
        return {
          icon: '⏱',
          label: 'lag',
          description: `dropped ${evt.skipped ?? 0} events (slow consumer)`
        };
      default:
        return {
          icon: '·',
          label: evt.type ?? 'event',
          description: JSON.stringify(evt).slice(0, 120)
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
    <div class="feed-body">
      {#if events.length === 0}
        <div class="empty">
          {#if connected}
            Waiting for events…
          {:else}
            Disconnected — retrying.
          {/if}
        </div>
      {:else}
        <ul class="event-list">
          {#each events as evt (evt._id)}
            {@const d = describe(evt)}
            <li class="event-row">
              <span class="icon" aria-hidden="true">{d.icon}</span>
              <div class="event-main">
                <div class="event-line">
                  <span class="badge">{d.label}</span>
                  <span class="desc">{d.description}</span>
                </div>
                <!-- _tick is referenced so Svelte re-renders this row each second -->
                <div class="event-time">{relTime(evt._ts)}{_tick > -1 ? '' : ''}</div>
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
  .event-time {
    color: var(--trusty-text-muted, #9ca3af);
    font-size: 10px;
    margin-top: 2px;
  }
</style>
