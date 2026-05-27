# Phase 3 Async Symbol-Graph Build — Design RFC

**Date**: 2026-05-27
**Author**: Research agent
**Status**: DRAFT — awaiting user decision before implementation
**Tracking issues**: [#109](https://github.com/bobmatnyc/trusty-tools/issues/109) (Phase 3 redescribed), [#145](https://github.com/bobmatnyc/trusty-tools/issues/145) (Louvain decision)
**Baseline**: trusty-search v0.13.1 — `docs/trusty-search/regression-testing/v0.13.1-2026-05-27.md`

---

## 1. Background

trusty-search's indexing pipeline was split into three sequential stages in v0.9.0 (issue #109
Phase 1): Stage 1 (lexical / BM25, fast, synchronous) runs first and makes the index queryable
for `search_lexical` immediately; Stage 2 (embedder, slow, async sidecar) computes HNSW vectors
and makes `search_semantic` available; Stage 3 (symbol-graph build) makes `search_kg` and
`get_call_chain` available.

Phase 2 of #109 shipped in v0.12.x as the `trusty-embedderd` sidecar architecture (issue #110):
the embedder runs as a supervised child process with a stdio JSON-RPC transport, isolating the
large ONNX memory arena from the daemon and enabling independent recycling on failure.

Phase 3 of #109 was originally scoped as "async-split Stage 3 including Louvain community
detection." That motivation was invalidated by the empirical result in #145
(`docs/trusty-search/research/stage-3-kg-decision-2026-05-25.md`): Louvain's community-cohesion
ranking signal produced a −16.7 pp Hit@1 regression over `search_semantic` on KG-targeted queries
with zero uplift on general queries. Louvain was subsequently deleted. The #109 Phase 3 scope is
therefore redefined as: **async symbol-graph build only**.

---

## 2. Current State (v0.13.1)

From the v0.13.1 SSE complete event (trusty-tools corpus, 1,148 files, M4 Max / 128 GB):

| Phase | Duration | % of total |
|-------|----------|------------|
| embed_ms | 310,933 ms | 95.6% |
| vector_upsert_ms | 9,504 ms | 2.9% |
| kg_ms | **508 ms** | 0.2% |
| bm25_ms | 1,996 ms | 0.6% |
| parse_ms | 266 ms | 0.1% |
| **Total elapsed** | **325,216 ms** | — |

Symbol graph stats post-reindex: **17,586 nodes, 409,783 edges**
(CallsFunction: 47,797; ModuleContains: 361,986).

### How graph build works today

In `service/reindex.rs`, after all embedding batches complete and the semantic stage is marked
`Ready`, `rebuild_symbol_graph_for_reindex` is called **inline** on the same async task that
drove the embed loop. This function takes the full `CodeIndexer` write lock, runs petgraph
construction over all parsed tree-sitter facts, then `mark_graph_ready` flips `stages.graph` to
`StageStatus::Ready`.

The graph stage already participates in the `IndexStages` / `StageStatus` state machine (`Pending`
→ `InProgress` → `Ready`). The `search_capabilities` array does not include `"kg"` until
`stages.graph.status.is_ready()`. So **a gating mechanism already exists at the API surface** —
the graph stage just happens to become ready immediately after embedding today because the build
is inline and fast.

`search_kg` is gated on graph-ready (`"kg"` in search_capabilities); `get_call_chain` and
`search_similar` consume the symbol graph but are not separately gated by the stage state
(they return empty or error if the graph is empty).

---

## 3. Problem Statement

At v0.13.1 numbers, `kg_ms = 508 ms` on a 1,148-file corpus. This is 0.16% of total reindex
time. **The inline graph build imposes no user-observable latency today.**

The question is: at what corpus size does inline graph build become a problem?

### Scaling estimate

petgraph construction cost is approximately O(N × E_avg) where N = node count and E_avg = mean
edges per node. At 1,148 files: 17,586 nodes, 409,783 edges (23 edges/node avg), 508 ms.

Projecting linearly (a lower bound — in practice graph density grows super-linearly with corpus
size because more files means more cross-file call edges):

| Corpus size (files) | Estimated nodes | Estimated edges | kg_ms (linear) | kg_ms (2× density) |
|---------------------|-----------------|-----------------|-----------------|---------------------|
| 1,148 (v0.13.1) | 17,586 | 409,783 | 508 ms (measured) | — |
| 5,000 | ~77,000 | ~1.8M | ~2,200 ms | ~4,400 ms |
| 10,000 | ~153,000 | ~3.5M | ~4,400 ms | ~17,600 ms |
| 25,000 | ~382,000 | ~8.9M | ~11,000 ms | ~110,000 ms |

At 10k-file corpora the inline build may reach 5–18 seconds, which is a meaningful tail on the
reindex. At 25k files it becomes the second-largest reindex cost after embedding.

**Current production corpora**: trusty-tools (1,148 files, 508 ms). No evidence of any indexed
corpus near 10k files today.

---

## 4. Design Options

### Option A: Keep inline (do nothing)

**Description**: No change. Graph build remains synchronous after embed, before `mark_graph_ready`.

**Complexity**: S (zero)
**Risk**: none today; risk grows if corpora exceed ~5k files
**Observed-need threshold**: Not met at v0.13.1. The 508 ms kg_ms is immaterial compared to
310,933 ms embed_ms.

**Recommendation trigger**: If a measured baseline shows `kg_ms > 5,000 ms` on a production corpus
(approximately 10k files), revisit this decision.

---

### Option B: Async graph build with `STAGE_NOT_READY` gating

**Description**: After embedding completes and semantic is marked `Ready`, spawn a tokio task to
build the symbol graph. The daemon returns an HTTP 202 response on the reindex stream immediately
after Stage 2. Tools that require the graph (`search_kg`, `get_call_chain`) check
`stages.graph.status.is_ready()` and return a structured `STAGE_NOT_READY` response when the
graph is still building. Callers can poll `/indexes/:id/status` and wait for `"kg"` to appear
in `search_capabilities`.

**State transitions**:
```
Reindex start → stages.graph = Pending
After all batches committed → stages.graph = InProgress (spawn async build task)
After petgraph build completes → stages.graph = Ready
```

**API contract changes**:
- `search_kg`: if `stages.graph.status != Ready`, return `{ "error": "STAGE_NOT_READY", "stage": "graph", "retry_after_ms": 500 }`
- `get_call_chain`: same gating
- `search_similar`: `search_similar` uses HNSW (not the symbol graph directly) — no gating change needed
- `search_all`: continues to omit KG expansion if graph is not ready (this already works today via
  the `search_capabilities` check in the query pipeline)
- `/indexes/:id/status`: `stages.graph` field already serialized — no new wire format needed

**Complexity**: M (medium)
- Spawn the async task and hand off `Arc<IndexHandle>` — straightforward.
- Gate `search_kg` and `get_call_chain` on `stages.graph.is_ready()` — the check site is small.
- Integration test: assert `STAGE_NOT_READY` is returned immediately after embed, then assert
  `search_kg` succeeds after `stages.graph` transitions to `Ready`.
- The `IndexStages` state machine and `StageStatus` enum are already in place; no new types needed.

**Risk**: moderate complexity on the test side; no new data-race risk because the graph is behind
`Arc<RwLock<SymbolGraph>>` already.

**Observed-need threshold**: Not met at v0.13.1 (508 ms).

---

### Option C: Incremental graph (per-file rebuild on file change)

**Description**: Instead of a full graph rebuild on each reindex, maintain the graph
incrementally — add/remove nodes and edges when individual files are indexed by the FileWatcher
or `index-file` endpoint.

**Complexity**: L (large)
- petgraph does not support incremental add/remove cleanly; edges cross file boundaries so a
  single-file update requires removing all edges touching that file's symbols, re-parsing, and
  re-inserting — equivalent to a partial rebuild that is harder to reason about than a full one.
- Correctness is difficult to ensure: renamed symbols, deleted files, refactored call sites leave
  stale edges that a full rebuild would clear.

**Risk**: high. Stale graph edges produce wrong `search_kg` and `get_call_chain` results without
obvious failure signals.

**Observed-need threshold**: Would be warranted if the full graph rebuild blocked the daemon on
every FileWatcher event (single-file change). At current corpus size, a 508 ms full rebuild on
every save event would be noticeable but not catastrophic. The FileWatcher path does not trigger
a full reindex today — it calls `index-file` (per-file), not `reindex`. Incremental graph would
only be beneficial if the daemon grows a "background continuous rebuild" capability separate from
explicit reindex. This is not in scope for v0.14.

**Recommendation**: do not implement in v0.14. Revisit if continuous-indexing latency becomes a
tracked metric.

---

## 5. Recommendation

**Recommend Option A (keep inline) for v0.14. Propose closing #109 Phase 3 with a
measured-threshold condition.**

### Rationale

The headline number is `kg_ms = 508 ms` on the trusty-tools corpus at v0.13.1 (1,148 files,
17,586 nodes, 409,783 edges). This is **0.16% of total reindex wall-clock time**. Embedding
(310,933 ms) dominates reindex by a factor of 612×. There is no user-observable latency penalty
from the inline graph build at current production corpus sizes.

The `IndexStages` state machine already supports a `Pending` → `InProgress` → `Ready` transition
for the graph stage. If async build becomes necessary (corpus > ~10k files by measured `kg_ms`
data), Option B can be implemented in a single focused PR with minimal API surface changes
because the gating infrastructure already exists. The work is not lost by deferring it.

Implementing Option B today would add test surface area and an observable `STAGE_NOT_READY`
error path for a problem that does not yet exist in production. This is premature.

**Close #109 when**: the regression-testing snapshot for a production corpus with ≥5,000 files
shows `kg_ms > 5,000 ms`. At that point Option B becomes the correct follow-through.

---

## 6. Acceptance Criteria (if Option B is chosen in the future)

These are pre-written to enable fast implementation when the threshold is crossed.

### State machine changes

No new types needed. Use existing `StageStatus::{Pending, InProgress, Ready}` on `IndexStages.graph`.

### Tools that gate on graph-ready

| MCP tool / endpoint | Gating required | Response when not ready |
|---------------------|-----------------|-------------------------|
| `search_kg` | Yes | `{ "error": "STAGE_NOT_READY", "stage": "graph", "retry_after_ms": 500 }` |
| `get_call_chain` | Yes | Same |
| `search_all` | No (already omits KG expansion when graph stage not ready) | — |
| `search_semantic` | No | — |
| `search_lexical` | No | — |
| `search_similar` | No (uses HNSW, not symbol graph) | — |
| `/facts` queries | No | — |

### "Ready" signal

The graph-ready signal is `stages.graph.status == StageStatus::Ready`, polled via
`GET /indexes/:id/status` (already serializes the `stages` field). No new endpoint needed.
The `search_capabilities` array already emits `"kg"` once graph is ready — MCP tool descriptions
can instruct callers to check this field before calling `search_kg`.

### Spawn pattern

```rust
// After mark_semantic_ready_graph_in_progress:
let handle_clone = Arc::clone(&handle);
tokio::spawn(async move {
    let kg = rebuild_symbol_graph_for_reindex(&handle_clone).await;
    mark_graph_ready(&handle_clone).await;
    tracing::info!(
        kg_ms = kg.kg_ms,
        symbol_count = kg.symbol_count,
        edge_count = kg.edge_count,
        "graph build complete (async)"
    );
});
// SSE complete event fires here, before graph is done
```

The graph `Arc<RwLock<SymbolGraph>>` is already shared-ownership — the spawned task holds a
clone of the `Arc<IndexHandle>` and no lifetime issues arise.

### Integration test sketch

```rust
// Trigger reindex, stream to complete event
// Assert: stages.graph.status == "in_progress" immediately after SSE complete
// Assert: search_kg returns STAGE_NOT_READY immediately after SSE complete
// Wait: poll /indexes/:id/status until stages.graph.status == "ready"
// Assert: search_kg returns results (no STAGE_NOT_READY)
```

---

## 7. Open Questions for User Decision

1. **Close #109 now or leave open as a reminder?** The recommendation is to close with a
   "reopen when kg_ms > 5,000 ms on a production corpus" note, and file a child ticket
   `#109-B` for Option B implementation at that threshold. Alternatively, leave #109 open
   and add the 5,000 ms threshold as an acceptance criterion on the ticket.

2. **Should the v0.14 regression snapshot explicitly measure `kg_ms` and record the
   node/edge growth rate as a tracked metric?** This would give early warning before the
   threshold is crossed. Recommend yes — it costs nothing to include `kg_ms` in the snapshot
   table going forward (it is already emitted in the SSE complete event).

3. **Is there a known large corpus (>5k files) that will be indexed in the near term?** If yes,
   run a one-off reindex against that corpus and record `kg_ms` before deciding whether Option B
   is needed for v0.14.

4. **`get_call_chain` gating**: currently `get_call_chain` returns empty results if the graph is
   empty, but does not return an error. If Option B is implemented, should it return a structured
   `STAGE_NOT_READY` error (Option B acceptance criteria above) or silently return empty? The
   structured error is recommended for debuggability.

---

## Cross-links

- [#109](https://github.com/bobmatnyc/trusty-tools/issues/109) — three-stage pipeline (this RFC)
- [#145](https://github.com/bobmatnyc/trusty-tools/issues/145) — Louvain deletion decision
- [#129](https://github.com/bobmatnyc/trusty-tools/issues/129) — cross-release performance tracking
- [stage-3-kg-decision-2026-05-25.md](./stage-3-kg-decision-2026-05-25.md) — empirical basis for Louvain deletion
- [v0.13.1-2026-05-27.md](../regression-testing/v0.13.1-2026-05-27.md) — baseline snapshot
