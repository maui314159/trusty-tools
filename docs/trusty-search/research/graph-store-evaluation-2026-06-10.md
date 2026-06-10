# Graph store evaluation for the knowledge graph — corrected research (#692)

**Date:** 2026-06-10
**Scope:** Evaluates candidate stores for the KG (`kg_nodes`/`kg_edges`) tier of
issue #692 ("Evaluate backing stores"). Covers RuVector, Lance Graph, LadybugDB,
and a broad sweep of the 2026 embedded-graph landscape, adversarially verified.
This is the **corrected** version: an adversarial review pass retracted one
recommendation (Grafeo) and fixed two flawed supporting arguments; corrections
are inlined and flagged.

**TL;DR:** Keep the incumbent architecture — redb-persisted, in-RAM
`petgraph` traversal — which is what `trusty-search` already ships. Ten
research passes found nothing in the 2026 landscape that beats it under the
stated constraints (embedded, pure-Rust, crash-safe, incremental mutation,
MSRV 1.91, small footprint). The future trigger to watch is **aggregate graph
RAM across resident indexes** (#705), not query expressiveness.

---

## 1. What the workload actually is (verified from code)

- `trusty-search`'s KG is a `petgraph::DiGraph` held in `Arc<SymbolGraph>`
  (lock-free concurrent reads), persisted to redb `kg_nodes`/`kg_edges` and
  rebuilt at warm-boot (`crates/trusty-search/src/core/symbol_graph.rs`).
- Default traversal depth is **one hop**: `KG_EXPAND_HOPS = 1`
  (`crates/trusty-search/src/core/indexer/mod.rs`).
- The node count is capped at 100k (`TRUSTY_MAX_KG_NODES`) following a
  180 GB-RSS incident; a 1M-chunk monorepo graph can pin 3–5 GB of RAM.
- Therefore: the binding constraint is **RAM across ~100 resident indexes**
  (issue #705's axis), not traversal speed or query language. Per-hop reads
  never touch redb — traversal is in-memory — so KV read-concurrency concerns
  (`begin_read()` serialization) apply to corpus-payload reads, **not** to KG
  traversal.

## 2. Evidence that a dedicated graph DB is not warranted here

**Benchmarks.** Pacaci et al. (GRADES'17, LDBC): relational/KV beats Neo4j ~10×
at 1–2 hop traversals; native graph wins ~100× only on shortest-path / deep
(≳4–5 hop) traversal. *Caveat (from the adversarial pass):* this is 2017
client-server data; modern embedded engines (Kuzu/Ladybug) have none of
Neo4j's overhead, so the honest claim is narrower — no benchmark shows a
dedicated engine beating **in-RAM petgraph** at this scale/depth, and that is
what we run.

**Production precedent.** No mature code-intelligence system uses a graph DB
as its store of record (all primary-source verified):

| System | Store | Model |
|---|---|---|
| GitHub stack-graphs | SQLite (WAL) | serialized graph blobs in relational rows |
| Meta Glean | RocksDB (LMDB experimental) | immutable fact DAG, Datalog (Angle) |
| Google Kythe | LevelDB | `(source, edge_kind, target)` triples |
| Sourcegraph SCIP | Postgres + blobstore | relational + protobuf blobs |
| rust-analyzer | in-memory (salsa) | recomputed incrementally |

stack-graphs — the closest precedent (typed code graph, incremental
mutation) — stores serialized graph fragments in SQLite. Architecturally that
is "embedded KV/relational as checkpoint store for an in-memory graph", i.e.
**the same shape as our redb + petgraph design**.

## 3. Candidate survey (scored against #692 constraints)

Constraints: embedded/in-process · pure-Rust (no CMake/clang) · crash-safe ·
incremental node/edge mutation · MSRV 1.91 · small footprint · permissive
license · sits behind the `KvStore`/adapter trait.

| Candidate | Pure-Rust | Embedded | Crash-safe | Maintained | Verdict |
|---|---|---|---|---|---|
| **redb** (incumbent) | ✅ | ✅ | ✅ ACID, format-frozen, fuzzed, huge dependent base | ✅ v4.1 (2026) | ✅ **keep** |
| **fjall** | ✅ | ✅ | ✅ LSM/MVCC | ✅ | ✅ escape hatch if corpus-read concurrency ever bottlenecks (not KG traversal) |
| **heed/LMDB** | ❌ C | ✅ | ✅ MVCC lock-free reads | ✅ | ⚠️ best read concurrency, but C dep |
| **agdb** | ✅ | ✅ | ✅ undo-WAL; no fuzzing; one unresolved corruption anecdote (#1058) | ✅ 3.8 yrs, no yanks | ⚠️ only viable graph-native **pilot**; self-only reverse-deps, 1–2 person bus factor |
| **Grafeo** | ✅ | ✅ | ❌ **two field data-loss incidents in first 4 months** (#252 WAL-not-replayed total loss; #323 patch-release format break bricked a user DB; #316 silent schema corruption) | 4.5 months old; release every ~1.7 days; author-acknowledged largely AI-generated | ❌ **RETRACTED** (was provisionally recommended; adversarial pass inverted it) |
| **LadybugDB** (`lbug`, Kuzu successor) | ❌ **verified C++ FFI** | ✅ | ✅ serializable ACID | ✅ most active Kuzu fork, MIT | ❌ on no-C++ rule (see §4) |
| **Kuzu** | ❌ C++ | ✅ | ✅ | ❌ archived Oct 2025 (team to Apple) | ❌ |
| **Cozo** | ⚠️ pure path = sled ("may not be stable enough" per upstream) | ✅ | ⚠️ | ❌ dormant since Dec 2023 | ❌ now; Datalog query-layer idea worth remembering |
| **IndraDB** | ⚠️ persistent = RocksDB | ✅ | ⚠️ backend-dependent | ✅ v5 (2025) | ❌ MPL-2.0 + C backend |
| **Oxigraph** | ❌ RocksDB on disk (verified `oxrocksdb-sys`; pure-Rust mode is memory-only) | ✅ | ✅ via RocksDB | ✅ active | ❌ C++ + RDF impedance |
| **HelixDB** | ❌ C LMDB | ❌ **server-only** (HTTP :6969) | ⚠️ default in-memory | ✅ YC-backed | ❌ + AGPL-3.0 engine |
| **SurrealDB** embedded | ⚠️ SurrealKV is vendor-labeled **beta**; prod path = RocksDB C++ | ✅ (tokio required) | ⚠️ synced writes default only ≥3.0 | ✅ funded | ❌ BSL core + heavy |
| **RuVector** (`ruvector-*`) | ✅ default | ✅ | ⚠️ rides on redb | solo+AI, no independent users, audited author track record ("99% theater" findings on sibling projects) | ❌ wrong category (vector+GNN platform **wrapping redb+hnsw_rs**); KG/Cypher claims unverified |
| **Lance Graph** | ✅ core | ✅ | ❌ **no mutation API** (read-only Cypher→DataFusion over Lance datasets; persistence helper is Python-side) | ✅ LanceDB org, pre-1.0 | ❌ Arrow+DataFusion+Lance(+Delta/S3 default) footprint; wrong shape (read-side analytics, not store of record) |
| **SQLite** (rusqlite) | ❌ bundled C — but **already in this workspace** (tga, cto-db, trusty-review) | ✅ | ✅ | ✅ | ⚠️ legitimate option the original pass overlooked; closest production precedent (stack-graphs). Not chosen — redb already does the persistence job without SQL surface — but the "pure-Rust" rule should be acknowledged as *chosen*, since the workspace already ships bundled-C SQLite |

## 4. LadybugDB detail (verified, since it is the strongest engine surveyed)

Kuzu was archived 2025-10 (team acqui-hired by Apple); LadybugDB is the most
active successor fork (MIT, Cypher property graph, FTS + vector indices,
serializable ACID). Source-level verification of the `lbug` crate
(v0.17.x, repo `LadybugDB/ladybug-rust`):

- crate keywords include `ffi`; build-deps are `cmake` + `cxx-build`
  (pinned with the comment "the last version that builds on clang 15");
- `build.rs` compiles the **C++20 core via CMake**, with a fallback tier that
  **downloads source tarballs or prebuilt native libraries from GitHub at
  build time** — a supply-chain/reproducibility concern for locked builds on
  top of the toolchain requirement.

So it fails the no-C++ constraint exactly as Kuzu did, plus a build-time
network dependency. It is the **first candidate to revisit if the no-C++ rule
is ever deliberately relaxed** (author-run benchmark: ~435× Neo4j on
path-finding at 2.4M edges — unreplicated, treat as vendor-run).

## 5. Corrections from the adversarial review pass

1. **Grafeo retracted** (see table). The original pass applied three agents of
   scrutiny to RuVector but recommended Grafeo off one agent's table row;
   equal scrutiny found field data loss, patch-level format breaks, and
   RuVector-grade release churn.
2. **The "redb + petgraph hybrid" option in the original report is the
   architecture that already ships.** The research initially characterized the
   workload (1–3 hops, tens of thousands of nodes) without reading the code;
   the actual default is 1 hop with a 100k node cap. Conclusion unchanged,
   premise now verified.
3. **The fjall/heed concurrency lever was misdirected for the KG** — traversal
   never touches redb at query time. It remains valid for corpus-payload
   reads.
4. **Solo-maintainer risk restated correctly:** redb and fjall are also
   solo-maintained. The real criterion is **exposure-hours + independent
   adversarial validation** (redb: 1.0+, frozen format, fuzzing, thousands of
   independent dependents). Maintainer headcount alone disqualifies nothing.
5. **Sourcing caveats:** some quotes (Kuzu/Apple, Grafeo HN admission) are
   search-snippet-sourced (pages 403'd); vendor benchmark multipliers
   (Ladybug 435×, HelixDB 5–20×, RuVector sub-ms) are author-run and
   unreplicated.

## 6. Decision framework

Keep redb-persisted in-RAM petgraph **unless** two or more of these become
true:

1. Traversals routinely exceed ~4–5 hops / shortest-path / whole-graph
   algorithms.
2. Edge counts grow ≫10⁵–10⁶ per index with deep queries.
3. First-class variable-length path-pattern queries (Cypher-style) are needed.
4. Declarative recursive analyses (Datalog rules) are needed.

**The trigger most likely to fire first in this codebase is none of the
above** — it is aggregate graph RAM across ~100 resident indexes (#705). If
that bites, the design conversation is paging/offload or lazy per-index graph
rebuild, not a query-language migration. If a dedicated engine is ever truly
needed: Cozo-style Datalog over a KV backend behind the same adapter first;
LadybugDB if (and only if) the no-C++ rule is deliberately relaxed.

## 7. Recommendation

1. **Keep redb + in-RAM petgraph** (current architecture) behind the #692
   `KvStore` adapter; maintain forward + reverse edge tables; keep multi-hop
   BFS in application code; keep path-query semantics out of the trait.
2. **fjall** = pure-Rust escape hatch for corpus-read concurrency, *if
   measured*.
3. **agdb** = only graph-native pilot candidate, with explicit caveats.
4. Rejected: RuVector, Lance Graph, Grafeo, HelixDB, SurrealDB, Oxigraph,
   Cozo (dormant), Kuzu (archived), LadybugdB-under-current-constraints.

The adapter-first plan in #692 is the load-bearing decision: it makes every
future candidate a low-stakes adapter, not a migration.

## Sources

Benchmarks/precedent: Pacaci et al. GRADES'17
(<https://cs.uwaterloo.ca/~jimmylin/publications/Pacaci_etal_2017.pdf>) ·
stack-graphs (<https://github.blog/open-source/introducing-stack-graphs/>) ·
Glean (<https://engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/>) ·
Kythe (<https://kythe.io/docs/kythe-storage.html>) ·
SCIP (<https://sourcegraph.com/blog/announcing-scip>) ·
rust-analyzer (<https://rust-analyzer.github.io/blog/2023/07/24/durable-incrementality.html>) ·
Kuzu/Ladybug/lance-graph bench (<https://github.com/prrao87/graph-benchmark>)

Engines: redb (<https://github.com/cberner/redb>) ·
fjall (<https://github.com/fjall-rs/fjall>) ·
agdb (<https://github.com/agnesoft/agdb>) ·
Grafeo + incidents (<https://github.com/GrafeoDB/grafeo>, issues #252/#316/#323) ·
LadybugDB (<https://github.com/LadybugDB/ladybug>,
<https://github.com/LadybugDB/ladybug-rust>, <https://crates.io/crates/lbug>) ·
Kuzu archived (<https://github.com/kuzudb/kuzu>) ·
Cozo (<https://github.com/cozodb/cozo>) ·
IndraDB (<https://github.com/indradb/indradb>) ·
Oxigraph (<https://github.com/oxigraph/oxigraph>) ·
HelixDB (<https://github.com/HelixDB/helix-db>) ·
SurrealKV (<https://github.com/surrealdb/surrealkv>) +
durability critique (<https://blog.cf8.gg/surrealdbs-ch/>) ·
RuVector (<https://github.com/ruvnet/ruvector>) + author-track-record audits
(<https://gist.github.com/roman-rr/ed603b676af019b8740423d2bb8e4bf6>,
<https://github.com/hesreallyhim/awesome-claude-code/issues/1338>) ·
Lance Graph (<https://github.com/lancedb/lance-graph>)
