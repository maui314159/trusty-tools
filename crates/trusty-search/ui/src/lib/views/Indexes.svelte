<script>
  /*
   * Why: The Indexes view is the admin pane for the daemon — list every
   * registered collection, register a new one, kick off a reindex, or evict
   * a stale one. The redesign (issue #38) adds a last-indexed column, a
   * per-index disk-usage column, and a live reindex-in-progress spinner fed
   * by the per-index `/reindex/stream` SSE feed.
   * What: Header card with a create form, then a table of every index with
   * disk + last-indexed columns and per-row Reindex / Delete buttons. A
   * reindex shows a spinner + progress until the SSE `complete` event.
   * Test: trigger a reindex on a seeded index and confirm the spinner shows
   * progress and clears on completion.
   */
  import { onDestroy } from 'svelte';
  import { api } from '../api.js';
  import { getIndexes, getLoading, getError, refreshIndexes } from '../state.svelte.js';

  let indexes = $derived(getIndexes());
  let loading = $derived(getLoading());
  let error = $derived(getError());

  let newId = $state('');
  let newPath = $state('');
  let creating = $state(false);
  let createError = $state(null);
  let rowError = $state(null);
  let busyId = $state(null);

  // Per-index reindex progress: { [id]: { indexed, total } }. An entry's
  // presence means a reindex is in-flight; it is removed on completion.
  let progress = $state({});
  // Live EventSource handles keyed by index id so we can close on unmount.
  const streams = {};

  onDestroy(() => {
    for (const id of Object.keys(streams)) closeStream(id);
  });

  function closeStream(id) {
    if (streams[id]) {
      streams[id].close();
      delete streams[id];
    }
  }

  async function createIndex(e) {
    e?.preventDefault?.();
    if (!newId.trim() || !newPath.trim()) return;
    creating = true;
    createError = null;
    try {
      await api.createIndex(newId.trim(), newPath.trim());
      newId = '';
      newPath = '';
      await refreshIndexes();
    } catch (err) {
      createError = err.message || String(err);
    } finally {
      creating = false;
    }
  }

  /**
   * Why: a reindex is fire-and-forget; the daemon returns an SSE stream URL.
   * Subscribing lets the table show live progress instead of a static label.
   * What: POSTs the reindex, then opens the `/reindex/stream` EventSource and
   * mirrors `progress`/`complete`/`error` events into local state.
   * Test: trigger a reindex, observe the spinner update then clear.
   */
  async function reindex(id) {
    rowError = null;
    busyId = id;
    try {
      const res = await api.reindex(id);
      progress = { ...progress, [id]: { indexed: 0, total: 0 } };
      const url = res?.stream_url || `/indexes/${encodeURIComponent(id)}/reindex/stream`;
      closeStream(id);
      const src = new EventSource(url);
      streams[id] = src;
      src.onmessage = (ev) => {
        let evt;
        try {
          evt = JSON.parse(ev.data);
        } catch {
          return;
        }
        if (evt.event === 'progress') {
          progress = {
            ...progress,
            [id]: { indexed: evt.indexed ?? 0, total: evt.total ?? 0 }
          };
        } else if (evt.event === 'complete' || evt.event === 'error') {
          const next = { ...progress };
          delete next[id];
          progress = next;
          closeStream(id);
          if (evt.event === 'error') {
            rowError = `Reindex ${id}: ${evt.message || 'failed'}`;
          }
          refreshIndexes().catch(() => {});
        }
      };
      src.onerror = () => {
        // Stream ended (daemon closes it on completion) — drop progress.
        const next = { ...progress };
        delete next[id];
        progress = next;
        closeStream(id);
      };
    } catch (err) {
      rowError = `Reindex ${id}: ${err.message || err}`;
    } finally {
      busyId = null;
    }
  }

  async function remove(id) {
    if (!confirm(`Delete index "${id}"? On-disk data is preserved.`)) return;
    rowError = null;
    busyId = id;
    try {
      closeStream(id);
      await api.deleteIndex(id);
      await refreshIndexes();
    } catch (err) {
      rowError = `Delete ${id}: ${err.message || err}`;
    } finally {
      busyId = null;
    }
  }

  /**
   * Why: disk_bytes is raw; operators want human units.
   * What: byte-count → "X KB / MB / GB", or "—" when null.
   * Test: humanBytes(1048576) === "1.0 MB".
   */
  function humanBytes(bytes) {
    if (typeof bytes !== 'number') return '—';
    if (bytes < 1024) return `${bytes} B`;
    const kb = bytes / 1024;
    if (kb < 1024) return `${kb.toFixed(1)} KB`;
    const mb = kb / 1024;
    if (mb < 1024) return `${mb.toFixed(1)} MB`;
    return `${(mb / 1024).toFixed(2)} GB`;
  }

  /**
   * Why: an RFC3339 timestamp is precise but not glanceable.
   * What: render as a localised date-time, or "never" when null.
   * Test: humanTime(null) === "never".
   */
  function humanTime(iso) {
    if (!iso) return 'never';
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString();
  }
</script>

<h1 class="page-title">Indexes</h1>

<div class="card mb-4">
  <div class="card-header">Register a new index</div>
  <div class="card-body">
    <form class="create-row" onsubmit={createIndex}>
      <input
        type="text"
        class="input"
        placeholder="Index id (e.g. my-project)"
        bind:value={newId}
      />
      <input
        type="text"
        class="input"
        placeholder="Absolute root path (e.g. /Users/me/code/my-project)"
        bind:value={newPath}
      />
      <button
        type="submit"
        class="btn btn-primary"
        disabled={creating || !newId.trim() || !newPath.trim()}
      >
        {creating ? 'Creating…' : 'Create'}
      </button>
    </form>
    {#if createError}
      <p class="text-sm mt-3" style="color: var(--trusty-danger)">{createError}</p>
    {/if}
  </div>
</div>

<div class="card">
  <div class="card-header flex-between">
    <span>Registered indexes</span>
    <button class="btn btn-sm" onclick={refreshIndexes} disabled={loading}>
      {loading ? 'Refreshing…' : 'Refresh'}
    </button>
  </div>
  <div class="card-body" style="padding: 0">
    {#if rowError}
      <div class="row-error">{rowError}</div>
    {/if}
    {#if error}
      <div class="row-error">{error}</div>
    {/if}
    {#if indexes.length === 0}
      <div class="empty">
        {#if loading}Loading…{:else}No indexes registered yet.{/if}
      </div>
    {:else}
      <div class="table-wrap">
        <table class="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Documents</th>
              <th>Disk</th>
              <th>Last indexed</th>
              <th>Root path</th>
              <th>Status</th>
              <th style="text-align: right">Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each indexes as ix (ix.id)}
              <tr>
                <td><strong>{ix.id}</strong></td>
                <td>{(ix.chunk_count ?? 0).toLocaleString()}</td>
                <td class="text-mono text-xs">{humanBytes(ix.disk_bytes)}</td>
                <td class="text-xs text-muted">{humanTime(ix.last_indexed)}</td>
                <td class="text-mono text-xs text-muted truncate" style="max-width: 280px">
                  {ix.root_path || '—'}
                </td>
                <td>
                  {#if progress[ix.id]}
                    <span class="badge badge-info reindex-badge">
                      <span class="spinner"></span>
                      {#if progress[ix.id].total > 0}
                        {progress[ix.id].indexed}/{progress[ix.id].total}
                      {:else}
                        reindexing…
                      {/if}
                    </span>
                  {:else if ix.error}
                    <span class="badge badge-danger">error</span>
                  {:else}
                    <span class="badge badge-success">ready</span>
                  {/if}
                </td>
                <td style="text-align: right; white-space: nowrap">
                  <button
                    class="btn btn-sm"
                    disabled={busyId === ix.id || !!progress[ix.id]}
                    onclick={() => reindex(ix.id)}
                  >
                    {progress[ix.id] ? 'Working…' : 'Reindex'}
                  </button>
                  <button
                    class="btn btn-sm btn-danger"
                    disabled={busyId === ix.id}
                    onclick={() => remove(ix.id)}
                  >
                    Delete
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </div>
</div>

<style>
  .page-title {
    font-size: var(--trusty-fs-xl);
    margin: 0 0 var(--trusty-space-5) 0;
    font-weight: 600;
  }
  .create-row {
    display: grid;
    grid-template-columns: 1fr 2fr auto;
    gap: var(--trusty-space-2);
    align-items: stretch;
  }
  .table-wrap {
    overflow-x: auto;
  }
  @media (max-width: 720px) {
    .create-row {
      grid-template-columns: 1fr;
    }
  }
  .row-error {
    padding: var(--trusty-space-3) var(--trusty-space-5);
    color: var(--trusty-danger);
    background: var(--trusty-danger-soft);
    border-bottom: 1px solid var(--trusty-border);
    font-size: var(--trusty-fs-sm);
  }
  .reindex-badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
  }
  .spinner {
    width: 10px;
    height: 10px;
    border: 2px solid currentColor;
    border-right-color: transparent;
    border-radius: 50%;
    display: inline-block;
    animation: spin 0.7s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
