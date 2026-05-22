# trusty-search Performance Baseline

Recorded against the `trusty-tools` monorepo (~22 crates, Rust workspace).
Date: 2026-05-22
Commit: c83bda53315839056b616db5e4a95972102bfcf4
Hardware: Apple M4 Max — 128 GiB RAM

## Index Characteristics

| Metric | Value |
|---|---|
| Project | trusty-tools |
| Files indexed | TBD |
| Chunks created | TBD |
| Graph nodes | TBD |
| Graph edges | TBD |
| Communities | TBD |
| Modularity | TBD |
| Index time (full reindex) | TBD |

> TBD values are populated by running the full regression suite with `--nocapture`
> and recording the printed output. Update this table after the first full reindex
> against this commit.

## Query Latency Thresholds (Regression Gates)

| Threshold | Value | Rationale |
|---|---|---|
| P50 latency | <= 500 ms | Interactive feel for agent use |
| P99 latency | <= 2000 ms | Acceptable tail for complex queries |
| Concurrent (8 parallel) | < 5 s total | Multi-agent workload |

These thresholds are enforced by the integration tests in
`crates/trusty-search/tests/baseline_trusty_tools.rs`.

## Regression Query Set

These 8 queries are the canonical regression suite. Each has an expected top-result
file fragment used to detect relevance regressions. A match is accepted if the
expected fragment appears in any of the top-3 results.

| # | Query | Expected file fragment | Intent |
|---|---|---|---|
| 1 | "symbol graph BFS expansion" | symbol_graph | definition |
| 2 | "Louvain community detection modularity" | community | definition |
| 3 | "axum middleware concurrency limiter" | concurrency | usage |
| 4 | "redb persistence write transaction" | corpus | usage |
| 5 | "embed batch async worker pool" | embed_pool | usage |
| 6 | "chunker AST tree-sitter code split" | chunker | definition |
| 7 | "HNSW vector similarity search" | search | usage |
| 8 | "auto discover claude code project" | discover | definition |

## Running Regression Tests

```bash
# Start daemon (if not already running)
trusty-search start --foreground &

# Index trusty-tools (only needed first time or after major changes)
trusty-search index /path/to/trusty-tools --name trusty-tools

# Run full baseline regression suite (all 8 tests)
cargo test -p trusty-search --test baseline_trusty_tools -- --include-ignored --nocapture

# Run a single test
cargo test -p trusty-search --test baseline_trusty_tools test_daemon_health -- --include-ignored --nocapture
```

## Test Descriptions

| Test | What it checks |
|---|---|
| `test_daemon_health` | `GET /health` → 200, `status == "ok"` |
| `test_index_exists_and_has_content` | Index registered; `node_count >= 1 000` |
| `test_query_latency_p50_under_threshold` | p50 over regression set <= 500 ms |
| `test_query_latency_p99_under_threshold` | p99 over 3× regression set <= 2 000 ms |
| `test_result_relevance` | Top-3 result contains expected file fragment for all 8 queries |
| `test_graph_scoring_active` | `meta.graph_scoring == true` (communities built) |
| `test_community_detection_quality` | `community_count >= 5`, `modularity >= 0.1` |
| `test_concurrent_queries_no_errors` | 8 parallel queries: all 200, total < 5 s |

## Graph Scoring Formula

Final result score = RRF_score + clamp(0.10 × degree_centrality + 0.05 × is_centroid, 0.0, 0.15)

`meta.community_cohesion` in the search response = fraction of top-10 results sharing
the top hit's Louvain community.

`meta.graph_scoring` is `true` when a `GraphScorer` was successfully built for the
index (requires non-empty symbol graph AND at least one persisted community record).

## Architecture Notes

- Embedder: fastembed (ONNX, all-MiniLM-L6-v2 INT8Q, 384-dim); CoreML auto-detected on Apple Silicon
- ANN index: usearch HNSW (persisted via usearch native format)
- Lexical: BM25 (zero-dep, per-query corpus)
- Fusion: RRF — Reciprocal Rank Fusion (k=60, parameter-free)
- Graph: petgraph DiGraph, persisted in redb `kg_nodes`/`kg_edges` tables
- Communities: Louvain (seeded `StdRng(42)`, max 100 passes), post-reindex pass
- Storage: redb 2.6 (chunks, KG, communities, HNSW checkpoints) per index
- Daemon port: 7878 (default); resolved at runtime from `port.lock` in the data directory

## Updating This Baseline

When a deliberate change causes a threshold to shift:

1. Run the full suite with `--nocapture` and record the new measurements.
2. Update the threshold constants in `baseline_trusty_tools.rs`:
   - `LATENCY_P50_THRESHOLD_MS`
   - `LATENCY_P99_THRESHOLD_MS`
   - `MIN_NODE_COUNT`
   - `MIN_COMMUNITY_COUNT`
   - `MIN_MODULARITY`
3. Update the **Index Characteristics** and **Query Latency Thresholds** tables above.
4. Commit both files together with a message explaining the regression direction
   and its justification (e.g. `perf(search): relax p99 threshold after adding NER pass`).
