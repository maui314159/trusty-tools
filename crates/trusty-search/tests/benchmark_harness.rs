//! Benchmark harness for entity-enriched KG search quality.
//!
//! Why: ad-hoc spot checks aren't enough to know whether a refactor regressed
//! retrieval. This harness fixes a small per-intent query set against the
//! `trusty-search` crate source tree and reports MRR@5 + Recall@10 + latency.
//! What: indexes `crates/trusty-search/src/` end-to-end (FastEmbedder +
//! UsearchStore + BM25 via the live `CodeIndexer::search` pipeline), runs the
//! query set, prints a per-intent table, and asserts a soft mean MRR@5 floor.
//! Test: each `#[ignore] #[tokio::test]` corresponds to one intent class.
//!
//! Run: cargo test --test benchmark_harness -- --include-ignored --nocapture
//!
//! Soft thresholds, not hard contracts: changes that drop MRR@5 below 0.3 mean
//! warrant scrutiny but the harness itself stays advisory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use trusty_search::core::indexer::{CodeChunk, CodeIndexer, SearchQuery};
use trusty_search::core::store::{UsearchStore, VectorStore};
use trusty_search::core::{Embedder, FastEmbedder};

// ---------------------------------------------------------------------------
// Query corpora — `(query_text, expected_substring)`. The substring is matched
// case-insensitively against the chunk content (for substring queries) or the
// `function_name` field (for symbolic queries).
// ---------------------------------------------------------------------------

const DEFINITION_QUERIES: &[(&str, &str)] = &[
    ("UsearchStore", "UsearchStore"),
    ("CodeChunk struct", "CodeChunk"),
    ("QueryIntent enum", "QueryIntent"),
    ("RRF fusion", "rrf"),
    ("BM25 tokenize", "tokenize"),
    ("SymbolGraph", "SymbolGraph"),
    ("EntityExtractor", "EntityExtractor"),
    ("FastEmbedder", "FastEmbedder"),
    ("FactRecord", "FactRecord"),
    ("EdgeKind score_multiplier", "score_multiplier"),
];

const USAGE_QUERIES: &[(&str, &str)] = &[
    ("where is FastEmbedder called", "embed"),
    ("callers of kg_expand", "kg_expand"),
    ("uses of redb table", "redb"),
    ("search method calls", "search"),
    ("index_file usage", "index_file"),
    ("entity_exact_match called", "entity_exact_match"),
    ("mmr_rerank called", "mmr_rerank"),
    ("bm25 tokenize usage", "tokenize"),
    ("cosine_similarity callers", "cosine"),
    ("fact_hash usage", "fact_hash"),
];

const CONCEPTUAL_QUERIES: &[(&str, &str)] = &[
    ("how does BM25 scoring work", "bm25"),
    ("what handles concurrent reads", "rwlock"),
    ("how is RRF fusion computed", "rrf"),
    ("knowledge graph expansion", "kg"),
    ("how are embeddings cached", "cache"),
    ("intent routing weights", "alpha"),
    ("how are chunks stored", "redb"),
    ("file watching debounce", "debounce"),
    ("MCP tool dispatch", "dispatch"),
    ("entity extraction tree-sitter", "entity"),
];

const BUGDEBT_QUERIES: &[(&str, &str)] = &[
    ("panic sites in indexer", "panic"),
    ("unwrap calls in search path", "unwrap"),
    ("todo in codebase", "todo"),
    ("unimplemented stubs", "unimplemented"),
    ("error handling gaps", "error"),
    ("raises CapacityError", "capacity"),
    ("bail macro usage", "bail"),
    ("anyhow error sites", "anyhow"),
    ("missing test coverage", "test"),
    ("deprecated code", "deprecated"),
];

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Mean Reciprocal Rank at k: 1/(rank+1) of the first relevant hit, else 0.
fn mrr_at_k(results: &[CodeChunk], expected: &str, k: usize) -> f32 {
    let needle = expected.to_lowercase();
    results
        .iter()
        .take(k)
        .enumerate()
        .find(|(_, c)| {
            c.content.to_lowercase().contains(&needle)
                || c.function_name
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&needle)
        })
        .map(|(i, _)| 1.0 / (i + 1) as f32)
        .unwrap_or(0.0)
}

/// Recall@k: did at least one of the top-k results contain the expected substring?
fn recall_at_k(results: &[CodeChunk], expected: &str, k: usize) -> bool {
    let needle = expected.to_lowercase();
    results
        .iter()
        .take(k)
        .any(|c| c.content.to_lowercase().contains(&needle))
}

// ---------------------------------------------------------------------------
// Grep / ripgrep baseline
// ---------------------------------------------------------------------------
//
// Why: a semantic-search benchmark is only meaningful relative to the tool it
// claims to beat. `grep -r` (or ripgrep when available) is the universal
// baseline developers reach for, so a side-by-side MRR@5 / Recall@10 table
// makes "where does semantic search win?" concrete and stable across refactors.
// What: derives a literal search term from each natural-language query, shells
// out to `rg` (preferred) or `grep -r` (fallback), wraps the first 10 ranked
// hits as `CodeChunk`s, and feeds them through the same metric functions used
// for trusty-search. Test: the four `#[ignore] bench_*_grep_baseline` tests, plus
// the merged comparison in each `bench_*_queries` test.

/// Pick the literal term a developer would actually grep for from a
/// natural-language query.
///
/// Why: `grep` cannot interpret prose ("how does BM25 scoring work"); a fair
/// baseline must reduce each query to the single keyword a human would type.
/// What: tokenizes on whitespace, drops common stop words and tokens shorter
/// than 3 chars, and returns the longest surviving token (ties broken by first
/// occurrence). Falls back to the longest raw token if everything is filtered.
/// Test: covered indirectly by the grep baseline tests; the chosen term is
/// printed in the comparison table for inspection.
fn grep_term(query: &str) -> String {
    const STOP: &[&str] = &[
        "how", "does", "what", "where", "the", "are", "is", "in", "of", "to", "called", "calls",
        "uses", "usage", "callers", "handles", "work", "works", "computed", "site", "sites",
        "code", "codebase", "gaps", "missing", "stubs",
    ];
    let best = query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|t| t.len() >= 3 && !STOP.contains(&t.to_lowercase().as_str()))
        .max_by_key(|t| t.len());
    match best {
        Some(t) => t.to_string(),
        // Degenerate query (all stop words / short) — fall back to longest raw token.
        None => query
            .split_whitespace()
            .max_by_key(|t| t.len())
            .unwrap_or(query)
            .to_string(),
    }
}

/// Run the best available grep tool against `root` for a literal `term`,
/// returning matched-line "chunks" in the tool's output order (best rank first).
///
/// Why: trusty-search returns ranked `CodeChunk`s; to compute identical MRR@5 /
/// Recall@10 the grep baseline must produce a comparably ranked list. What:
/// prefers `rg --line-number --no-heading -i <term>`, falls back to
/// `grep -rni --include=*.rs <term>`, parses `file:line:content`, and caps at
/// `top_k` hits (grep has no relevance ranking, so file-walk order is its
/// "ranking"). Test: exercised by the grep baseline tests; returns an empty vec
/// on tool failure so callers degrade to MRR=0 rather than panicking.
fn grep_search(root: &Path, term: &str, top_k: usize) -> Vec<CodeChunk> {
    // Prefer ripgrep; fall back to POSIX grep -r. We probe rg once per call —
    // cheap relative to the embedding work that dominates the bench.
    let output = if Command::new("rg").arg("--version").output().is_ok() {
        Command::new("rg")
            .args([
                "--line-number",
                "--no-heading",
                "--color=never",
                "-i",
                "-t",
                "rust",
                term,
            ])
            .arg(root)
            .output()
    } else {
        Command::new("grep")
            .args(["-rni", "--include=*.rs", term])
            .arg(root)
            .output()
    };

    let out = match output {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);

    text.lines()
        .take(top_k)
        .filter_map(|line| {
            // Format: <path>:<lineno>:<content>
            let mut parts = line.splitn(3, ':');
            let path = parts.next()?;
            let lineno: usize = parts.next()?.trim().parse().ok()?;
            let content = parts.next()?;
            let rel = Path::new(path)
                .strip_prefix(root)
                .unwrap_or_else(|_| Path::new(path))
                .to_string_lossy()
                .into_owned();
            // CodeChunk does not derive Default; spell out every field. We only
            // care about `content` (for the metric match) and ordering; the rest
            // are inert placeholders so the grep result is structurally a chunk.
            Some(CodeChunk {
                id: format!("{rel}:{lineno}"),
                file: rel,
                language: Some("rust".to_string()),
                start_line: lineno,
                end_line: lineno,
                content: content.to_string(),
                function_name: None,
                score: 0.0,
                compact_snippet: None,
                match_reason: "grep".to_string(),
                chunk_type: Default::default(),
                calls: Vec::new(),
                inherits_from: Vec::new(),
                chunk_depth: 0,
                index_id: None,
                on_branch: false,
                community_id: None,
                archive_reason: None,
            })
        })
        .collect()
}

/// Compute mean MRR@5 + Recall@10 for the grep baseline over a query set,
/// returning `(mean_mrr, recall_hits, per_query)` for the comparison table.
fn grep_metrics(root: &Path, queries: &[(&str, &str)]) -> (f32, usize, Vec<(String, f32, bool)>) {
    let mut mrr_sum = 0.0_f32;
    let mut recall_hits = 0_usize;
    let mut per_query = Vec::with_capacity(queries.len());
    for (q, expected) in queries {
        let term = grep_term(q);
        let results = grep_search(root, &term, 10);
        let mrr = mrr_at_k(&results, expected, 5);
        let rec = recall_at_k(&results, expected, 10);
        mrr_sum += mrr;
        if rec {
            recall_hits += 1;
        }
        per_query.push(((*q).to_string(), mrr, rec));
    }
    (mrr_sum / queries.len() as f32, recall_hits, per_query)
}

// ---------------------------------------------------------------------------
// Indexing fixture — shared across the four benchmark tests.
// ---------------------------------------------------------------------------

/// Locate the crate's own `src/` directory so the harness works regardless of
/// which directory `cargo test` was launched in.
///
/// Why: the previous path pointed at `crates/trusty-search-core/src/`, a
/// sub-crate that was folded into `trusty-search` at v0.3.0 and no longer
/// exists. The stale path silently indexed zero files, so every benchmark
/// reported MRR@5 = 0.000.
/// What: returns `<crate-root>/src`. `CARGO_MANIFEST_DIR` for a test in this
/// crate resolves to `crates/trusty-search/`, so `.join("src")` is the real
/// consolidated source tree (core / service / mcp modules).
/// Test: exercised by the four benchmark tests in this file, which now index a
/// non-empty corpus; also verifiable with `ls crates/trusty-search/src/`.
fn core_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Build a fresh `CodeIndexer` populated with every `.rs` file under the
/// crate's `src/`. Returns the indexer once population
/// completes (synchronous from the caller's perspective).
async fn build_indexer() -> CodeIndexer {
    let embedder: Arc<dyn Embedder> = Arc::new(
        FastEmbedder::new()
            .await
            .expect("init FastEmbedder (downloads model on first run)"),
    );
    let store: Arc<dyn VectorStore> = Arc::new(UsearchStore::new(384).expect("init UsearchStore"));
    let indexer = CodeIndexer::new("bench", core_src_dir()).with_components(embedder, store);

    // Walk core src and feed every .rs file through index_file.
    let src = core_src_dir();
    for entry in walkdir::WalkDir::new(&src)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let rel = path
            .strip_prefix(&src)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Err(e) = indexer.index_file(&rel, &content).await {
            eprintln!("index_file({rel}) failed: {e}");
        }
    }

    indexer
}

/// Run a query set, print a results table, and return the mean MRR@5 plus the
/// per-query rows so the caller can assert on the soft floor and build a
/// side-by-side grep comparison.
async fn run_bench(
    label: &str,
    indexer: &CodeIndexer,
    queries: &[(&str, &str)],
) -> (f32, Vec<(String, f32, bool)>) {
    println!("\n=== {label} ===");
    println!(
        "| {:<40} | {:>6} | {:>9} | {:>10} |",
        "query", "MRR@5", "Recall@10", "latency_ms"
    );
    println!("|{:-<42}|{:-<8}|{:-<11}|{:-<12}|", "", "", "", "");

    let mut mrr_sum = 0.0_f32;
    let mut recall_hits = 0_usize;
    let mut per_query = Vec::with_capacity(queries.len());
    for (q, expected) in queries {
        let sq = SearchQuery {
            text: (*q).to_string(),
            top_k: 10,
            expand_graph: true,
            compact: false,
            ..Default::default()
        };
        let started = Instant::now();
        let results = indexer
            .search(&sq)
            .await
            .expect("search must not fail in bench harness");
        let latency_ms = started.elapsed().as_millis();
        let mrr = mrr_at_k(&results, expected, 5);
        let rec = recall_at_k(&results, expected, 10);
        mrr_sum += mrr;
        if rec {
            recall_hits += 1;
        }
        per_query.push(((*q).to_string(), mrr, rec));
        println!(
            "| {:<40} | {:>6.3} | {:>9} | {:>10} |",
            truncate(q, 40),
            mrr,
            if rec { "yes" } else { "no" },
            latency_ms
        );
    }

    let n = queries.len() as f32;
    let mean_mrr = mrr_sum / n;
    let recall_pct = (recall_hits as f32 / n) * 100.0;
    println!(
        "mean MRR@5 = {:.3}  |  Recall@10 = {:.0}% ({}/{})",
        mean_mrr,
        recall_pct,
        recall_hits,
        queries.len()
    );
    (mean_mrr, per_query)
}

/// Print a per-query side-by-side comparison table of trusty-search vs grep and
/// a winner summary, then return the grep mean MRR@5 for context.
///
/// Why: the whole point of the baseline is the *comparison*; a single combined
/// table makes regressions and grep-wins obvious at a glance. What: greps the
/// crate source for each query's literal term, aligns the per-query rows with
/// the trusty-search rows, and tallies how many queries each tool wins on MRR@5.
/// Test: invoked from each `bench_*_queries` test under `--nocapture`.
fn print_comparison(label: &str, search_rows: &[(String, f32, bool)], queries: &[(&str, &str)]) {
    let root = core_src_dir();
    let (grep_mean, grep_recall, grep_rows) = grep_metrics(&root, queries);

    println!("\n--- {label}: trusty-search vs grep ---");
    println!(
        "| {:<34} | {:<12} | {:>9} | {:>9} | {:<8} |",
        "query", "grep term", "ts MRR@5", "gp MRR@5", "winner"
    );
    println!(
        "|{:-<36}|{:-<14}|{:-<11}|{:-<11}|{:-<10}|",
        "", "", "", "", ""
    );

    let mut ts_wins = 0_usize;
    let mut grep_wins = 0_usize;
    let mut ties = 0_usize;
    let mut ts_mrr_sum = 0.0_f32;

    for (i, (q, _expected)) in queries.iter().enumerate() {
        let (_, ts_mrr, _) = &search_rows[i];
        let (_, gp_mrr, _) = &grep_rows[i];
        ts_mrr_sum += *ts_mrr;
        let winner = if (ts_mrr - gp_mrr).abs() < f32::EPSILON {
            ties += 1;
            "tie"
        } else if ts_mrr > gp_mrr {
            ts_wins += 1;
            "trusty"
        } else {
            grep_wins += 1;
            "grep"
        };
        println!(
            "| {:<34} | {:<12} | {:>9.3} | {:>9.3} | {:<8} |",
            truncate(q, 34),
            truncate(&grep_term(q), 12),
            ts_mrr,
            gp_mrr,
            winner
        );
    }

    let n = queries.len() as f32;
    println!(
        "trusty mean MRR@5 = {:.3}  |  grep mean MRR@5 = {:.3}  |  grep Recall@10 = {:.0}% ({}/{})",
        ts_mrr_sum / n,
        grep_mean,
        (grep_recall as f32 / n) * 100.0,
        grep_recall,
        queries.len()
    );
    println!("winners — trusty: {ts_wins}  grep: {grep_wins}  ties: {ties}");
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

const SOFT_MRR_FLOOR: f32 = 0.3;

// ---------------------------------------------------------------------------
// Tests — `#[ignore]` so they don't run in `cargo test --workspace`.
// ---------------------------------------------------------------------------

#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_definition_queries() {
    let indexer = build_indexer().await;
    let (mean, rows) = run_bench("definition", &indexer, DEFINITION_QUERIES).await;
    print_comparison("definition", &rows, DEFINITION_QUERIES);
    assert!(
        mean >= SOFT_MRR_FLOOR,
        "definition mean MRR@5 = {mean:.3} below soft floor {SOFT_MRR_FLOOR}"
    );
}

#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_usage_queries() {
    let indexer = build_indexer().await;
    let (mean, rows) = run_bench("usage", &indexer, USAGE_QUERIES).await;
    print_comparison("usage", &rows, USAGE_QUERIES);
    assert!(
        mean >= SOFT_MRR_FLOOR,
        "usage mean MRR@5 = {mean:.3} below soft floor {SOFT_MRR_FLOOR}"
    );
}

#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_conceptual_queries() {
    let indexer = build_indexer().await;
    let (mean, rows) = run_bench("conceptual", &indexer, CONCEPTUAL_QUERIES).await;
    print_comparison("conceptual", &rows, CONCEPTUAL_QUERIES);
    assert!(
        mean >= SOFT_MRR_FLOOR,
        "conceptual mean MRR@5 = {mean:.3} below soft floor {SOFT_MRR_FLOOR}"
    );
}

#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_bugdebt_queries() {
    let indexer = build_indexer().await;
    let (mean, rows) = run_bench("bugdebt", &indexer, BUGDEBT_QUERIES).await;
    print_comparison("bugdebt", &rows, BUGDEBT_QUERIES);
    assert!(
        mean >= SOFT_MRR_FLOOR,
        "bugdebt mean MRR@5 = {mean:.3} below soft floor {SOFT_MRR_FLOOR}"
    );
}

// ---------------------------------------------------------------------------
// Grep / ripgrep baseline tests — `#[ignore]`-gated like the trusty-search
// benches so `cargo test --workspace` stays fast. These run grep alone (no
// embedder) so the baseline can be inspected even when the model is absent.
// ---------------------------------------------------------------------------

/// Shared body: grep one intent class and print its MRR@5 / Recall@10 table.
fn run_grep_baseline(label: &str, queries: &[(&str, &str)]) {
    let root = core_src_dir();
    let (mean, recall_hits, rows) = grep_metrics(&root, queries);

    println!("\n=== grep baseline: {label} ===");
    println!(
        "| {:<40} | {:<14} | {:>6} | {:>9} |",
        "query", "grep term", "MRR@5", "Recall@10"
    );
    println!("|{:-<42}|{:-<16}|{:-<8}|{:-<11}|", "", "", "", "");
    for ((q, mrr, rec), (orig_q, _)) in rows.iter().zip(queries.iter()) {
        debug_assert_eq!(q, orig_q);
        println!(
            "| {:<40} | {:<14} | {:>6.3} | {:>9} |",
            truncate(q, 40),
            truncate(&grep_term(orig_q), 14),
            mrr,
            if *rec { "yes" } else { "no" }
        );
    }
    let n = queries.len() as f32;
    println!(
        "grep mean MRR@5 = {:.3}  |  Recall@10 = {:.0}% ({}/{})",
        mean,
        (recall_hits as f32 / n) * 100.0,
        recall_hits,
        queries.len()
    );
}

#[ignore]
#[test]
fn bench_definition_grep_baseline() {
    run_grep_baseline("definition", DEFINITION_QUERIES);
}

#[ignore]
#[test]
fn bench_usage_grep_baseline() {
    run_grep_baseline("usage", USAGE_QUERIES);
}

#[ignore]
#[test]
fn bench_conceptual_grep_baseline() {
    run_grep_baseline("conceptual", CONCEPTUAL_QUERIES);
}

#[ignore]
#[test]
fn bench_bugdebt_grep_baseline() {
    run_grep_baseline("bugdebt", BUGDEBT_QUERIES);
}
