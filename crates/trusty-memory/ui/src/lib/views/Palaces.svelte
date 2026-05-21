<script>
  /*
   * Why: The Palaces view is the operator's window into the memory hierarchy
   * — Palace → Wing → Room → Drawer. Each palace exposes aggregate counts;
   * expanding a palace drills into its drawers (the atomic memory units).
   * What: A collapsible tree. Top level lists every palace with wing / room
   * proxy / drawer / vector / KG-triple counts. Clicking a palace lazily
   * fetches its drawers via `GET /api/v1/palaces/{id}/drawers` and renders
   * them grouped by tag, each showing importance + content preview.
   * Test: open #/palaces, click a palace row, confirm its drawers load and
   * collapse again on a second click.
   */
  import { onMount } from 'svelte';
  import { api } from '../api.js';

  let palaces = $state([]);
  let error = $state(null);
  let loading = $state(true);

  // Per-palace expand state + lazily-loaded drawers, keyed by palace id.
  let expanded = $state({});
  let drawers = $state({}); // { [id]: { items, loading, error } }

  onMount(loadPalaces);

  async function loadPalaces() {
    loading = true;
    error = null;
    try {
      palaces = await api.listPalaces();
    } catch (e) {
      error = e.message || String(e);
      palaces = [];
    } finally {
      loading = false;
    }
  }

  /**
   * Why: drawers are the most expensive part of the tree, so we fetch them
   * lazily the first time a palace is expanded and cache the result.
   * What: toggles `expanded[id]`; on first expand, fetches the palace's
   * drawers and stores them in `drawers[id]`.
   * Test: click a palace, confirm its drawer list appears below the row.
   */
  async function togglePalace(id) {
    expanded = { ...expanded, [id]: !expanded[id] };
    if (expanded[id] && !drawers[id]) {
      drawers = { ...drawers, [id]: { items: [], loading: true, error: null } };
      try {
        const items = await api.listDrawers(id, { limit: 200 });
        drawers = {
          ...drawers,
          [id]: { items: Array.isArray(items) ? items : [], loading: false, error: null }
        };
      } catch (e) {
        drawers = {
          ...drawers,
          [id]: { items: [], loading: false, error: e.message || String(e) }
        };
      }
    }
  }

  /**
   * Why: a drawer's content can be long; the tree wants a one-line preview.
   * What: trims content to 140 chars with an ellipsis.
   * Test: preview("x".repeat(200)).length === 141.
   */
  function preview(text) {
    const t = (text || '').replace(/\s+/g, ' ').trim();
    return t.length <= 140 ? t : t.slice(0, 140) + '…';
  }
</script>

<h1 class="page-title">Palaces</h1>

{#if error}
  <div class="card" style="border-color: var(--trusty-danger)">
    <div class="card-body" style="color: var(--trusty-danger)">{error}</div>
  </div>
{/if}

<div class="card">
  <div class="card-header flex-between">
    <span>Memory hierarchy</span>
    <button class="btn btn-sm" onclick={loadPalaces} disabled={loading}>
      {loading ? 'Refreshing…' : 'Refresh'}
    </button>
  </div>
  <div class="card-body" style="padding: 0">
    {#if loading}
      <div class="empty">Loading palaces…</div>
    {:else if palaces.length === 0}
      <div class="empty">No palaces yet.</div>
    {:else}
      <div class="tree">
        {#each palaces as p (p.id)}
          <div class="palace">
            <button
              class="tree-row palace-row"
              onclick={() => togglePalace(p.id)}
              aria-expanded={!!expanded[p.id]}
            >
              <span class="caret" class:open={expanded[p.id]}>▸</span>
              <span class="tree-icon">▤</span>
              <span class="tree-name">{p.name || p.id}</span>
              <span class="counts">
                <span class="badge badge-muted">{p.wing_count ?? 0} wings</span>
                <span class="badge badge-muted">{p.drawer_count ?? 0} drawers</span>
                <span class="badge badge-info">{p.vector_count ?? 0} vectors</span>
                <span class="badge badge-info">{p.kg_triple_count ?? 0} triples</span>
              </span>
            </button>
            {#if p.description}
              <div class="palace-desc">{p.description}</div>
            {/if}
            {#if expanded[p.id]}
              <div class="children">
                {#if drawers[p.id]?.loading}
                  <div class="tree-note">Loading drawers…</div>
                {:else if drawers[p.id]?.error}
                  <div class="tree-note tree-error">{drawers[p.id].error}</div>
                {:else if (drawers[p.id]?.items || []).length === 0}
                  <div class="tree-note">No drawers in this palace.</div>
                {:else}
                  <div class="drawer-head">
                    <span class="tree-icon">⌑</span>
                    <span class="text-sm text-secondary">
                      Drawers ({drawers[p.id].items.length})
                    </span>
                  </div>
                  {#each drawers[p.id].items as d (d.id)}
                    <div class="drawer-row">
                      <span class="tree-icon">·</span>
                      <div class="drawer-body">
                        <div class="drawer-content">{preview(d.content)}</div>
                        <div class="drawer-meta">
                          <span class="bar" title="importance {d.importance ?? 0}">
                            <span
                              class="bar-fill"
                              style="width: {Math.round((d.importance ?? 0) * 100)}%"
                            ></span>
                          </span>
                          {#each d.tags || [] as tag}
                            <span class="tag">{tag}</span>
                          {/each}
                        </div>
                      </div>
                    </div>
                  {/each}
                {/if}
              </div>
            {/if}
          </div>
        {/each}
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
  .tree {
    display: flex;
    flex-direction: column;
  }
  .palace {
    border-bottom: 1px solid var(--trusty-border);
  }
  .tree-row {
    display: flex;
    align-items: center;
    gap: var(--trusty-space-2);
    width: 100%;
    padding: var(--trusty-space-3) var(--trusty-space-5);
    background: none;
    border: none;
    text-align: left;
    flex-wrap: wrap;
  }
  .palace-row:hover {
    background: var(--trusty-content-bg);
  }
  .caret {
    display: inline-block;
    transition: transform 0.15s ease;
    color: var(--trusty-text-muted);
    font-size: var(--trusty-fs-xs);
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .tree-icon {
    color: var(--trusty-text-muted);
  }
  .tree-name {
    font-weight: 600;
    color: var(--trusty-text-primary);
  }
  .counts {
    display: flex;
    gap: var(--trusty-space-1);
    margin-left: auto;
    flex-wrap: wrap;
  }
  .palace-desc {
    padding: 0 var(--trusty-space-5) var(--trusty-space-2) 44px;
    font-size: var(--trusty-fs-sm);
    color: var(--trusty-text-muted);
  }
  .children {
    background: var(--trusty-content-bg);
    padding: var(--trusty-space-2) 0 var(--trusty-space-3) 0;
  }
  .drawer-head {
    display: flex;
    align-items: center;
    gap: var(--trusty-space-2);
    padding: var(--trusty-space-2) var(--trusty-space-5) var(--trusty-space-2) 44px;
  }
  .text-secondary {
    color: var(--trusty-text-secondary);
  }
  .drawer-row {
    display: flex;
    gap: var(--trusty-space-2);
    padding: var(--trusty-space-2) var(--trusty-space-5) var(--trusty-space-2) 60px;
  }
  .drawer-body {
    min-width: 0;
    flex: 1;
  }
  .drawer-content {
    font-size: var(--trusty-fs-sm);
    color: var(--trusty-text-primary);
    word-break: break-word;
  }
  .drawer-meta {
    display: flex;
    align-items: center;
    gap: var(--trusty-space-2);
    margin-top: 4px;
    flex-wrap: wrap;
  }
  .tree-note {
    padding: var(--trusty-space-2) var(--trusty-space-5) var(--trusty-space-2) 44px;
    font-size: var(--trusty-fs-sm);
    color: var(--trusty-text-muted);
  }
  .tree-error {
    color: var(--trusty-danger);
  }
</style>
