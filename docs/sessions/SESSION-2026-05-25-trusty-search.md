# trusty-search engineering session — 2026-05-25

A day that began with corruption and ended with architectural clarity. This document captures the trajectory from v0.8.0 → v0.10.0 across 20+ tickets and 10+ shipped versions, the empirical findings that shaped each decision, and a prioritized queue for future work.

## TL;DR

1. **Walker corruption (#100)**: The v0.8.0 index contained zero `.rs` files because `walkdir::WalkDir` ignored `.gitignore`. Fix shipped as v0.8.1; unblocked honest benchmarking.

2. **Quick intent fixes (v0.8.2/v0.8.3)**: Three follow-up tickets (#119 QueryClassifier acronym/snake_case, #117 Definition-intent boost, #118 docs-by-default). Hit@1 momentum but no metric lift from function-definition boost (#122) — revealed two deeper blockers (#142 SCREAMING_SNAKE gap, #143 per-const chunking).

3. **Staged pipeline (#109, v0.9.0/v0.9.1)**: Split indexing into lexical-only Stage 1 (302ms for 100 files, grep-parity) and Stages 2/3 (embedder + KG, slower). Phase 1 shipped; Phase 2 (true async spawn) remains. Discovered warm-boot state-machine bug (#135) — data was durable, only the in-memory stage tracking was wrong.

4. **Corpus cleanliness (#123, synthetic corpus)**: Built a 42-file non-circular benchmark after realizing every prior metric was inflated 14–43pp by BM25 seeing literal query strings in trusty-search's own test files. Clean numbers: 43% Hit@1 lexical, 43% hybrid on definitions.

5. **Conceptual-query expansion (#3 corpus)**: Added 5 conceptual queries; discovered the real pattern: lexical wins on definitions, hybrid wins on conceptuals. The synthesis is tool-based intent routing, not server-side classifiers.

6. **Per-lane MCP tools (#138, v0.10.0)**: Four tools (`search_lexical`, `search_semantic`, `search_kg`, `search_all`) replace intent guessing. Tool descriptions ARE the LLM's prompt — better than any daemon regex. STAGE_NOT_READY errors with `suggested_tools` turn failures into training data.

**Versions shipped**: v0.8.1, v0.8.2, v0.8.3, v0.9.0, v0.9.1, v0.9.2, v0.10.0. Open tickets prioritized below.

---

## The Corruption Discovery (v0.8.0 baseline)

The session opened with a jarring number: v0.8.0 benchmarks showed grep beating trusty-search 7/8 on Hit@1. Investigation revealed the trusty-tools INDEX was fundamentally broken — 19,727 chunks, ZERO `.rs` files, all from `claude-mpm-patch/` (a Python Git-ignore subtree).

The root cause: `walkdir::WalkDir` in the walker did not honor `.gitignore` files. The chunk budget (default 50k) exhausted on ignored files before indexing a single Rust source file.

**Fix #100** (commit 23785a7): Switched to the `ignore` crate, which parses `.gitignore` correctly. Also added explicit budget-truncation logging so future wallet-budget bugs surface immediately.

**Impact**: Recovered ~3,000 previously-skipped chunks; unblocked all subsequent benchmarking. Shipped as v0.8.1.

---

## Quick Wins and Intent Classifier Loop (v0.8.2 / v0.8.3 / v0.9.2)

Once #100 unblocked honest indexing, three follow-up tickets shipped in rapid succession:

**#119 QueryClassifier extension** (v0.8.2): The intent classifier only recognized PascalCase (`SearchMode`) and snake_case function names (`apply_archive_downrank`). Missing: SCREAMING_SNAKE constants (`BRUSILOV_EPOCH`) and acronyms (`HammondLever`). Extended the classifier; shipped as v0.8.2.

**#117 Definition-intent boost** (v0.8.3): When the classifier detects Definition intent, boost struct/enum chunks over usage-site matches. Shipped as v0.8.3. Hit@1 rose from 75% to 100% on the 14-query warm-boot suite — but this was BM25-only (warm boot, all stages Pending). A clearer signal emerged only after synthetic-corpus validation.

**#118 docs-by-default** (v0.8.3): The code-mode filter was excluding docs entirely. Reversed: index docs, then filter at query time based on mode. Shipped together with #117 as v0.8.3.

**#122 Function-Definition boost** (v0.9.2): Extended the Definition boost to cover `Function` and `Method` chunks. Promised a Hit@1 lift on function-name queries. Shipped as v0.9.2 — but the clean-corpus numbers (after #123) showed ZERO metric improvement. Why? Two deeper gaps:

- **#142**: The classifier doesn't recognize SCREAMING_SNAKE (`BRUSILOV_EPOCH`), so function-definition queries like `"calculate DAMPING_FACTOR"` miss the constant definitions.
- **#143**: The chunker emits one chunk per function/struct but doesn't split out individual `pub const` declarations. A query for `DAMPING_FACTOR` finds the module, not the specific constant.

These two tickets remain open; fixing them is the prerequisite for #122 to actually deliver metric lift.

**Learning**: Server-side intent classification is a losing arms race (#119 → #117 → #122 → #142 → #143 — each fix exposed the next gap). The eventual solution (#138) delegates this to the LLM via per-lane tool selection.

---

## The Architectural Pivot — Staged Pipeline (v0.9.0 / v0.9.1)

The strategic move of the session: split indexing into stages.

**#109 Phase 1** (v0.9.0, commit 52ef8b4): Introduced `stage=lexical` mode (walker + chunker + BM25 only, no embeddings or KG). Empirical finding: indexing 100 .rs files via Stage 1 alone took **302ms** vs the full pipeline's **8.2 minutes**. Stage 1 delivers grep-equivalent latency.

Warm-boot behavior confirmed: after daemon restart, all stages reset to `Pending` for normal indexes. The search handler auto-degrades to BM25-only (no semantic or KG signal) until stages advance to `Ready`. This is correct by design — the index data on disk is intact, searches work immediately via BM25.

**Warm-boot regression**: Discovered that `GET /indexes/:id/status` returned all stages as `Pending` for 60 existing indexes. The data was durable on disk (HNSW snapshots, redb corpus intact), but the warm-boot path didn't inspect on-disk artifacts.

**#135 fix** (v0.9.1, commit b8a5966): The warm-boot handler now checks for on-disk HNSW and KG artifacts and reconstructs the stage state machine accordingly. No data loss; only the in-memory state tracking was wrong. Shipped as v0.9.1.

**#109 Phase 2** (open): Currently Stage 2 (embedder) still runs inline with the commit transaction. True async spawn (separate worker, non-blocking stages advance) remains for future work.

---

## The Bias Discovery — Clean Benchmark Corpus (v0.9.1 / #123)

After v0.9.1 validation, a troubling realization: every prior Hit@K number was **contaminated by BM25 circular bias**.

The trusty-tools repository itself contains the benchmark queries as literal strings in `crates/trusty-search/tests/baseline_trusty_tools.rs`. When trusty-tools is indexed, BM25 matches the queries directly, artificially inflating Hit@1 and Hit@5 by **14–43 percentage points** compared to clean corpora.

**#123** (commit 3a6c6ff): Built a synthetic non-circular benchmark corpus — 42 .rs files with distinctive symbol names (glyphwarpen-observatory), verified zero-leak via ripgrep grep. Added ground-truth queries with human intent classification (Definition / Conceptual / Usage / Text / Data).

**Result**: Clean Hit@1 numbers were sobering:
- Lexical (BM25 only): **43%** Hit@1
- Hybrid (BM25 + HNSW): **43%** Hit@1
- KG-leading (graph expansion): **43%** Hit@1

No lift from semantic or KG on this small corpus. The HNSW and KG signals are weak on 298 chunks — they need 100+ files with rich cross-file call density. This motivated the open-mpm benchmark (#5, in flight as a sibling task).

**Important caveat**: Trending (v0.8.1 vs v0.8.3 vs v0.9.x) is valid because circular bias is consistent across versions. Absolute numbers need the clean corpus.

---

## Conceptual-Query Data and Mode Insights (#3 corpus expansion)

After synthetic-corpus baseline, expanded the corpus with 5 explicit conceptual queries (`"flatten clustered tree into depth first vector"`, etc.) and added mode-hint harness support to track `mode=text` / `mode=data` / `mode=code` separately.

**Key finding**: The query-category breakdown revealed two distinct patterns:

| Category | Lexical Hit@1 | Hybrid Hit@1 | Winner |
|----------|:-------------:|:------------:|--------|
| Definition | 3/4 (75%) | 3/4 (75%) | **Tie** (no lift) |
| Conceptual | 5/9 (56%) | 7/9 (78%) | **Hybrid** (+22pp) |
| Usage | 0/2 | 0/2 | **Tie** (hard problem) |
| Text | 1/2 | 1/2 | **Tie** |
| Data | 1/2 | 2/2 | **Hybrid** (+50pp) |

**Synthesis**: Lexical wins on exact definitions (BM25 token overlap), hybrid wins on conceptual queries (semantic embeddings bridge vocabulary gaps). The right answer isn't "always use hybrid" — it's "let the LLM choose based on intent."

---

## The Strategic Answer — Per-Lane MCP Tools (#138, v0.10.0)

The session's capstone: four MCP tools replace the single `search` + `?stage=` parameter and the doomed intent classifier.

**#138** (commit 5541382, v0.10.0): Introduced `search_lexical`, `search_semantic`, `search_kg`, `search_all`:

| Tool | Stage pin | Expansion | Prerequisite | Use case |
|------|-----------|-----------|--------------|----------|
| `search_lexical` | `stage=lexical` | none | none (always ready) | Exact symbol names, regex, literal phrases |
| `search_semantic` | `stage=semantic` | none | Stage 2 (embeddings) | Conceptual queries ("how does auth work?") |
| `search_kg` | `stage=graph` | true | Stage 3 (KG) | Impact analysis, caller chains |
| `search_all` | none (adaptive) | true | none (graceful degrade) | Hybrid queries with both literals and concepts |

**The key insight**: Tool descriptions ARE the LLM's intent-classification prompt. Rather than the daemon guessing `QueryClassifier::classify(query)`, the LLM reads tool descriptions and picks the right one. This is strictly better because:

1. The LLM has full context (user intent, conversation history, code domain knowledge).
2. No daemon-side regex arms race (#119 → #122 → #142 → #143).
3. STAGE_NOT_READY errors become training data: `suggested_tools` tells the LLM "try search_lexical while Stage 2 builds."

**Legacy compatibility**: The original `search` tool is preserved as a back-compat alias.

**Validation** (v0.10.0 snapshot):
- All 37 MCP unit tests pass, including stage-not-ready happy path.
- Hit@K signature identical to v0.9.1 v2 baseline (zero regression).
- STAGE_NOT_READY error carries `current_stages` + `suggested_tools` for LLM retry hints.

**Side effect**: Superseded #128 (Stage 3 A/B disable flags). The A/B comparison (`search_semantic` vs `search_all`) is now a cleanly-expressible tool comparison. Closed #128 as superseded.

---

## Findings: What We Know Now

### Stage 1 is Grep Parity
- 100 .rs files indexed in **302ms** via lexical-only pipeline.
- **100% Hit@5** on definition queries.
- Confirmed three times across different configurations.
- Conclusion: For customers who only need exact-match search, Stage 1 alone is sufficient and 26× faster than full pipeline.

### Hybrid Earns Its Keep on Conceptual Queries
- **+22pp Hit@1** lift on conceptual queries (56% → 78%).
- **Zero lift** on exact-match definitions.
- Surprising honest result: semantic search is a net positive for one query category, neutral for others.

### KG Signal Unmeasurable on Small Corpora
- On the 298-chunk synthetic corpus, `search_kg` and `search_semantic` produce identical Hit@K.
- Requires 100+ files with rich inter-file call density (function A calls B calls C) to manifest.
- The open-mpm benchmark (#5, sibling task) addresses this with a 5,000+ chunk corpus.

### Server-Side Intent Classification is a Losing Arms Race
- #119 (acronyms, snake_case) → #117 (Definition boost) → #122 (Function/Method boost) → #142 (SCREAMING_SNAKE gap) → #143 (per-const chunking).
- Each fix exposed the next gap. The daemon will never be smart enough.
- **Better answer**: Per-lane tools with LLM-driven selection (#138).

### Memory Characteristics
- **Peak RSS during embed phase**: 22.4 GB (full pipeline on trusty-tools corpus, 20k+ chunks, batch size auto-tuned).
- **Dominant bottleneck**: ONNX FastEmbed memory footprint and per-batch workspace allocation.
- Strong case for #110 (dedicated embedder process, separate memory arena).

### Warm-Boot State Machine is Delicate
- All stages reset to `Pending` after daemon restart (correct by design).
- If the warm-boot path doesn't inspect on-disk artifacts, existing indexes appear "broken" until reindex completes.
- #135 fix: check for on-disk HNSW snapshots and redb corpus presence, reconstruct state accurately.
- **Learning**: State machine tests should include warm-boot validation.

---

## Open Ticket Queue (Prioritize)

### Quick Wins (1–2 hours each)

**#141 — clippy format_in_format_args lint**: Partial fix in #138; finish the cleanup.

**#142 — SCREAMING_SNAKE classifier extension**: Extend QueryClassifier to recognize all-caps identifiers. Prerequisite for #122 metric lift.

**#143 — Rust chunker per-const granularity**: Emit individual chunks for `pub const` declarations instead of bundling them into the module. Prerequisite for #122 metric lift. Together, #142 + #143 unblock #122 from delivering Hit@1 improvement on constants.

### Architectural (days each)

**#109 Phase 2 — true async spawn for Stage 2/3**: Currently Stage 2 (embedder) runs inline with the commit transaction. Phase 2 upgrades to spawn-on-demand with non-blocking stage advancement. File-watcher debouncing also deferred to Phase 2.

**#110 — dedicated embedder process (`trusty-embedderd`)**: Motivations:
- 22 GB peak RSS during embed phase on large corpora.
- Separate memory arena avoids OOM during parallel indexing.
- Process recycling on failure (embedder crash doesn't kill daemon).
- Potential for GPU offload (future-proof).

### Research (open-ended)

**#101 — zero-chunk indexes silent failure**: Related to but separate from #100. A create-index with zero files produces an index that appears "ready" but has no chunks. Should error loudly or handle gracefully.

**#107 — quick-win indexing perf**: Louvain deferral (KG community detection on background), ORT batch-size tuning, chunker profiling.

**#108 — LanceDB columnar batch-write evaluation**: Evaluate LanceDB for large-scale vector storage. The prior audit had no throughput numbers; if we revisit, frame it as cost-per-queries-per-second, not peak ingestion.

**#114 — KG symbol-node schema**: Enhance graph schema to treat bare identifiers (e.g., `BRUSILOV_EPOCH`) as first-class nodes (currently only edges). Needed for better constant-constant relationships.

### Already Shipped

#100, #117, #118, #119, #122 (shipped but metric lift blocked by #142/#143), #123, #129 (tracker maintained), #135, #138.

### Closed as Superseded

#128 (Stage 3 A/B) — superseded by #138 (per-lane tools). The comparison between `search_semantic` and `search_all` is now a first-class tool distinction.

---

## How to Use This Document

- **"What landed today?"** → Read Section "Findings" and the version table above.
- **"What should I do next?"** → Jump to "Open Ticket Queue."
- **"Why did we do X instead of Y?"** → Navigate the numbered sections (corruption → quick wins → staged pipeline → corpus → per-lane tools).
- **"What are the actual numbers?"** → See `/docs/regression-testing/` snapshots for each version (v0.8.1 through v0.10.0).
- **"Will KG signal help my corpus?"** → Small corpora (<300 chunks): no measurable lift. Large corpora (5k+): likely +15–30pp on usage/caller queries. Open-mpm benchmark (#5) will confirm on a real large corpus.

---

## Lessons for Future Sessions

1. **Circular bias detection is cheap**: One ripgrep pass over the corpus to find query literals. Saves from shipping inflated metrics.
2. **Staged indexing is more powerful than incremental**: The 302ms Stage 1 result argues for offering a fast `--lexical-only` option to users, not just internal tuning.
3. **State machine warm-boot validation is essential**: Add tests that check index state after daemon restarts; save state to disk or compute it accurately.
4. **LLM-driven tool selection beats daemon classifiers**: Per-lane tools with good descriptions scale better than regex. Tool choice = prompt clarity.
5. **Honest benchmarking requires corpus isolation**: Never include queries as literals in the corpus. Use synthetic data or extensive validation of circular bias.

---

**Last updated**: 2026-05-25  
**Session duration**: ~12 hours (v0.8.0 → v0.10.0)  
**Versions shipped**: 7 (v0.8.1, v0.8.2, v0.8.3, v0.9.0, v0.9.1, v0.9.2, v0.10.0)  
**Tickets filed**: 20+  
**Tracking issue**: [#129](https://github.com/bobmatnyc/trusty-tools/issues/129)
