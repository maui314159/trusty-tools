//! Performance benchmarks for trusty-memory-core retrieval layers.
//!
//! Why: CLAUDE.md defines hard latency targets (L0+L1 < 5 ms, L2 < 50 ms,
//! L3 < 150 ms, cold-start < 200 ms). These benchmarks give criterion a
//! repeatable harness to verify those targets in CI and on developer machines.
//! What: Exercises retrieve_l0_l1 (pure in-memory), UsearchStore::search for
//! L2/L3 equivalents, KnowledgeGraph::assert, DecayConfig math, and query
//! normalization + FNV hash.
//! Test: Run `cargo bench -p trusty-memory-core` and inspect the report table
//! in the final output section.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::time::Duration;
use tempfile::tempdir;
use trusty_memory_core::{
    analytics::{fnv1a_hash, normalize_query},
    decay::DecayConfig,
    palace::{Drawer, PalaceId},
    retrieval::{retrieve_l0_l1, PalaceHandle},
    store::{kg::KnowledgeGraph, vector::UsearchStore, VectorStore},
};
use uuid::Uuid;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a normalized 384-d unit vector deterministically from a seed.
///
/// Why: We need reproducible vectors without running the full embedder;
/// a sine-based sequence avoids the zero-norm edge case.
/// What: Each element is sin(seed * 384 + j); then L2-normalize.
/// Test: norm of returned vec is approximately 1.0.
fn synthetic_vec(seed: usize) -> Vec<f32> {
    let raw: Vec<f32> = (0..384).map(|j| ((seed * 384 + j) as f32).sin()).collect();
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
    raw.iter().map(|x| x / norm).collect()
}

/// Build a fixed query vector (cosine of index) used across L2/L3 benches.
///
/// Why: A single deterministic vector lets criterion measure the same workload
/// each sample without RNG overhead in the hot loop.
/// What: cos(i) for i in 0..384, then L2-normalize.
/// Test: Inner product with itself should be ~1.0 (cosine sim = 1).
fn query_vec() -> Vec<f32> {
    let raw: Vec<f32> = (0..384).map(|i| (i as f32).cos()).collect();
    let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
    raw.iter().map(|x| x / norm).collect()
}

/// Populate a fresh PalaceHandle with `n` drawers and their synthetic vectors.
///
/// Why: All vector-layer benchmarks need a consistent initial state; this
/// helper encapsulates the setup so bench functions stay focused on the hot
/// path.
/// What: Creates UsearchStore + KnowledgeGraph in a temp dir, inserts n
/// drawers (each with a deterministic 384-d vector), calls refresh_l1, and
/// returns the handle alongside the TempDir guard so the files survive the
/// benchmark.
/// Test: After setup_palace(100), search returns non-empty results.
fn setup_palace(n: usize) -> (PalaceHandle, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let vs = UsearchStore::new(dir.path().join("idx.usearch"), 384).expect("UsearchStore::new");
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).expect("KnowledgeGraph::open");
    let mut handle = PalaceHandle::new(
        PalaceId::new("bench"),
        "Benchmark palace — system design and architecture knowledge".to_string(),
        vs,
        kg,
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    for i in 0..n {
        let room_id = Uuid::new_v4();
        let mut d = Drawer::new(
            room_id,
            format!(
                "Memory {i}: topic about system design, distributed systems, and Rust architecture"
            ),
        );
        // Spread importance evenly so L1 selection is representative.
        d.importance = (i as f32 / n.max(1) as f32).clamp(0.1, 1.0);
        let drawer_id = d.id;
        let vec = synthetic_vec(i);
        rt.block_on(handle.vector_store.upsert(drawer_id, vec))
            .expect("upsert");
        handle.add_drawer(d);
    }
    handle.refresh_l1();
    (handle, dir)
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

/// Benchmark L0 + L1 retrieval (always-on, fully in-memory).
///
/// Why: This is the baseline every call pays; it must stay sub-5 ms.
/// What: Calls retrieve_l0_l1 on a handle pre-loaded with 100 drawers and
/// black_box-es the result to prevent optimisation.
/// Test: Median should be well under 5 ms on any modern machine.
fn bench_l0_l1(c: &mut Criterion) {
    let (handle, _dir) = setup_palace(100);

    c.bench_function("l0_l1_retrieval_100_drawers", |b| {
        b.iter(|| {
            let results = retrieve_l0_l1(black_box(&handle));
            black_box(results)
        })
    });
}

/// Benchmark L2-equivalent vector search at small scale (100 drawers, top-10).
///
/// Why: Verifies that HNSW search on a tiny palace meets the sub-50 ms target
/// with headroom to spare.
/// What: UsearchStore::search with top_k = 10 on a 100-vector index.
/// Test: Median expected < 5 ms (HNSW on 100 vectors is trivially fast).
fn bench_l2_100(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (handle, _dir) = setup_palace(100);
    let query = query_vec();

    c.bench_function("l2_vector_search_100_drawers_top10", |b| {
        b.iter(|| {
            rt.block_on(async {
                handle
                    .vector_store
                    .search(black_box(&query), 10)
                    .await
                    .expect("search")
            })
        })
    });
}

/// Benchmark L2-equivalent vector search at medium scale (1 000 drawers, top-10).
///
/// Why: A thousand drawers is a realistic project-scale palace; top-10 search
/// must stay sub-50 ms.
/// What: UsearchStore::search with top_k = 10 on a 1 000-vector index.
/// Test: Median expected < 10 ms (HNSW is log-complexity in N).
fn bench_l2_1000(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (handle, _dir) = setup_palace(1000);
    let query = query_vec();

    let mut group = c.benchmark_group("l2_search_1000_drawers");
    group.measurement_time(Duration::from_secs(15));
    group.bench_function("top10", |b| {
        b.iter(|| {
            rt.block_on(async {
                handle
                    .vector_store
                    .search(black_box(&query), 10)
                    .await
                    .expect("search")
            })
        })
    });
    group.finish();
}

/// Benchmark L3-equivalent vector search at medium scale (1 000 drawers, top-50).
///
/// Why: L3 deep search fetches 5x more results; must stay sub-150 ms.
/// What: UsearchStore::search with top_k = 50 on a 1 000-vector index.
/// Test: Median expected < 20 ms (HNSW is still fast at top-50).
fn bench_l3_1000(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (handle, _dir) = setup_palace(1000);
    let query = query_vec();

    let mut group = c.benchmark_group("l3_search_1000_drawers");
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("top50", |b| {
        b.iter(|| {
            rt.block_on(async {
                handle
                    .vector_store
                    .search(black_box(&query), 50)
                    .await
                    .expect("search")
            })
        })
    });
    group.finish();
}

/// Benchmark cold-start UsearchStore open (I/O + index header parse).
///
/// Why: One of the two components of palace cold-start is re-loading the HNSW
/// index from disk. Criterion will re-open the same pre-seeded file each
/// iteration, giving a realistic warm-cache number without thread-pool churn.
/// What: Seeds idx.usearch once, then times UsearchStore::new repeatedly.
/// Test: Median expected < 10 ms on SSD; the load path is a single mmap/read.
fn bench_palace_cold_start(c: &mut Criterion) {
    let dir = tempdir().expect("tempdir");
    let idx_path = dir.path().join("idx.usearch");

    // Seed the index file so the hot path exercises `index.load`, not `reserve`.
    {
        let vs = UsearchStore::new(idx_path.clone(), 384).expect("seed UsearchStore");
        black_box(vs);
    }

    c.bench_function("palace_cold_start_usearch_reopen", |b| {
        b.iter(|| {
            // Re-open a pre-seeded index — the realistic daemon-restart path.
            let vs =
                UsearchStore::new(black_box(idx_path.clone()), 384).expect("UsearchStore::new");
            black_box(vs);
        })
    });

    // Measure KG open once (not per-iter) to verify schema migration cost.
    // We cannot create hundreds of r2d2 pools rapidly on macOS without hitting
    // the OS thread-count limit, so this is a single timed measurement.
    let kg_path = dir.path().join("kg.db");
    let start = std::time::Instant::now();
    let kg = KnowledgeGraph::open(&kg_path).expect("KnowledgeGraph::open");
    let kg_open_ms = start.elapsed().as_micros();
    black_box(kg);
    println!("\nKnowledgeGraph::open (single measurement): {kg_open_ms} µs");
}

/// Benchmark KnowledgeGraph::assert with unique subjects (no prior interval).
///
/// Why: KG writes are on the critical path for memory_remember; they must not
/// become a bottleneck.
/// What: One assert per iteration, each with a unique subject so the UPDATE
/// finds zero rows and the INSERT is the only real work.
/// Test: Median expected < 2 ms on WAL-mode SQLite.
fn bench_kg_assert(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let dir = tempdir().expect("tempdir");
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).expect("KnowledgeGraph::open");

    use chrono::Utc;
    use trusty_memory_core::store::kg::Triple;

    let mut i: u64 = 0;
    c.bench_function("kg_assert_unique_subject", |b| {
        b.iter(|| {
            i += 1;
            rt.block_on(async {
                kg.assert(Triple {
                    subject: format!("entity_{i}"),
                    predicate: "has_property".to_string(),
                    object: format!("value_{i}"),
                    valid_from: Utc::now(),
                    valid_to: None,
                    confidence: 1.0,
                    provenance: None,
                })
                .await
                .expect("assert")
            });
        })
    });
}

/// Benchmark KG assert with a repeated subject (exercises prior-interval close).
///
/// Why: The UPDATE + INSERT path is the common case for ongoing fact updates
/// (e.g. project status changes). It must be benchmarked separately from the
/// unique-subject case because the UPDATE adds one extra SQLite round-trip.
/// What: Same (s,p) each iteration so the UPDATE always closes one row.
/// Test: Median expected < 3 ms.
fn bench_kg_assert_update(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let dir = tempdir().expect("tempdir");
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).expect("KnowledgeGraph::open");

    use chrono::Utc;
    use trusty_memory_core::store::kg::Triple;

    // Seed the first row so the very first bench iteration also triggers an UPDATE.
    rt.block_on(async {
        kg.assert(Triple {
            subject: "fixed_entity".to_string(),
            predicate: "status".to_string(),
            object: "initial".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .expect("seed")
    });

    let mut i: u64 = 0;
    c.bench_function("kg_assert_repeated_subject", |b| {
        b.iter(|| {
            i += 1;
            rt.block_on(async {
                kg.assert(Triple {
                    subject: "fixed_entity".to_string(),
                    predicate: "status".to_string(),
                    object: format!("updated_{i}"),
                    valid_from: Utc::now(),
                    valid_to: None,
                    confidence: 1.0,
                    provenance: None,
                })
                .await
                .expect("assert")
            });
        })
    });
}

/// Benchmark DecayConfig::effective_importance (pure arithmetic).
///
/// Why: Decay is computed on every ranked result in L2/L3; it must be
/// nanosecond-order so it does not inflate retrieval latency.
/// What: One effective_importance call per iteration with fixed inputs.
/// Test: Median expected < 100 ns.
fn bench_decay(c: &mut Criterion) {
    let cfg = DecayConfig::default();
    c.bench_function("decay_effective_importance", |b| {
        b.iter(|| cfg.effective_importance(black_box(0.8), black_box(45.0), black_box(0.1)))
    });
}

/// Benchmark query normalization + FNV-1a hash (analytics hot path).
///
/// Why: Every recall call optionally hashes the query for the RecallLog; this
/// must be sub-microsecond to remain invisible in the tail-latency budget.
/// What: normalize_query then fnv1a_hash on a realistic English sentence.
/// Test: Median expected < 500 ns.
fn bench_analytics_hash(c: &mut Criterion) {
    c.bench_function("query_normalize_and_hash", |b| {
        b.iter(|| {
            let normalized =
                normalize_query(black_box("The quick brown fox jumps over the lazy dog"));
            fnv1a_hash(black_box(&normalized))
        })
    });
}

/// Benchmark L2-equivalent search across palace sizes (parametric scaling).
///
/// Why: Shows how HNSW search scales from 100 to 5 000 drawers so we can
/// predict whether large future palaces stay within the sub-50 ms budget.
/// What: top-10 search for N in [100, 500, 1000, 5000].
/// Test: All should be < 50 ms even at N = 5 000.
fn bench_l2_scaling(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let query = query_vec();

    let mut group = c.benchmark_group("l2_search_scaling_top10");
    group.measurement_time(Duration::from_secs(10));

    for &n in &[100usize, 500, 1000, 5000] {
        let (handle, _dir) = setup_palace(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                rt.block_on(async {
                    handle
                        .vector_store
                        .search(black_box(&query), 10)
                        .await
                        .expect("search")
                })
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_l0_l1,
    bench_l2_100,
    bench_l2_1000,
    bench_l3_1000,
    bench_palace_cold_start,
    bench_kg_assert,
    bench_kg_assert_update,
    bench_decay,
    bench_analytics_hash,
    bench_l2_scaling,
);
criterion_main!(benches);
