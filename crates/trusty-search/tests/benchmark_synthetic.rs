//! Synthetic non-circular benchmark harness for trusty-search hybrid retrieval.
//!
//! Why: benchmarks run against `trusty-tools` itself suffer from BM25 circular
//! bias (issue #123) because the benchmark query strings appear verbatim in
//! the indexed test files (e.g. `core/classifier.rs` assert literals). This
//! harness drives the daemon against a fully synthetic corpus
//! (`tests/benchmark_corpus/synthetic/`) where every symbol name has been
//! verified to appear nowhere else in the repository — so the BM25 lane
//! cannot lift a query just by seeing its own benchmark text.
//!
//! What: reads `GROUND_TRUTH.json`, registers a fresh `synthetic-benchmark`
//! index pointing at the corpus, drives a reindex, polls until every stage
//! is `Ready`, then runs each ground-truth query in three modes (lexical
//! only / full hybrid / KG-leading), records Hit@1 and Hit@5, prints a
//! comparison table, and deletes the index.
//!
//! Test: gated `#[ignore]` so it does not run during default `cargo test`.
//! Run with:
//!   cargo test --test benchmark_synthetic -- --include-ignored --nocapture
//!
//! Prerequisites:
//!   - trusty-search daemon running at `http://127.0.0.1:7878`
//!   - `OPENROUTER_API_KEY` not required (no chat calls)
//!
//! This harness intentionally does NOT spin up its own daemon. It uses the
//! already-running daemon the developer started for everyday work, consistent
//! with the pattern in `baseline_trusty_tools.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{json, Value};

// ── Constants ───────────────────────────────────────────────────────────────

const DAEMON_URL: &str = "http://127.0.0.1:7878";
const INDEX_NAME: &str = "synthetic-benchmark";

/// Maximum time we will wait for the reindex to bring every stage to `Ready`.
const REINDEX_TIMEOUT: Duration = Duration::from_secs(180);

/// Interval between status polls during reindex wait.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)] // category + expected_mode are documented metadata; future
                    // assertions may use them but the harness today reports by
                    // mode tuple.
struct GroundTruthQuery {
    id: String,
    text: String,
    category: String,
    expected_mode: String,
    ground_truth_files: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    /// `?stage=lexical` — BM25 only.
    Lexical,
    /// No stage parameter — full hybrid (BM25 + vector + KG expansion + RRF).
    Hybrid,
    /// `expand_graph=true, use_kg_first=true` — KG-leading retrieval.
    KgLeading,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Lexical => "lexical",
            Mode::Hybrid => "hybrid",
            Mode::KgLeading => "kg-leading",
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // query_text retained for diagnostic prints during failures.
struct QueryResult {
    query_id: String,
    query_text: String,
    mode: Mode,
    top_files: Vec<String>,
    hit_at_1: bool,
    hit_at_5: bool,
    intent: String,
    match_reason: String,
    server_latency_ms: Option<u64>,
    client_latency_ms: u128,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a reqwest client with the timeouts already tuned by
/// `baseline_trusty_tools.rs`.
fn make_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client construction is infallible")
}

/// Absolute path of the synthetic corpus root.
fn corpus_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .join("tests")
        .join("benchmark_corpus")
        .join("synthetic")
}

/// Load and parse `GROUND_TRUTH.json`.
fn load_ground_truth() -> Vec<GroundTruthQuery> {
    let path = corpus_root().join("GROUND_TRUTH.json");
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("failed to read GROUND_TRUTH.json at {path:?}: {e}"));
    let raw: Value = serde_json::from_slice(&bytes).expect("GROUND_TRUTH.json must be valid JSON");
    let queries = raw["queries"].as_array().expect("queries array required");
    queries
        .iter()
        .map(|q| GroundTruthQuery {
            id: q["id"].as_str().unwrap().to_string(),
            text: q["text"].as_str().unwrap().to_string(),
            category: q["category"].as_str().unwrap().to_string(),
            expected_mode: q["expected_mode"].as_str().unwrap().to_string(),
            ground_truth_files: q["ground_truth_files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect(),
        })
        .collect()
}

/// Sanity-check that the daemon is up.
async fn assert_daemon_healthy(client: &Client) {
    let resp = client
        .get(format!("{DAEMON_URL}/health"))
        .send()
        .await
        .expect("daemon must be reachable at 127.0.0.1:7878 — start it with `trusty-search start`");
    assert_eq!(resp.status().as_u16(), 200, "GET /health returned non-200");
}

/// Create (or re-create) the `synthetic-benchmark` index. Returns immediately
/// after the POST — the caller drives the reindex separately.
async fn register_index(client: &Client) {
    // Delete first if it exists, to start from a clean slate.
    let _ = client
        .delete(format!("{DAEMON_URL}/indexes/{INDEX_NAME}"))
        .send()
        .await;

    let root = corpus_root();
    let body = json!({
        "id": INDEX_NAME,
        "root_path": root.to_string_lossy(),
    });
    let resp = client
        .post(format!("{DAEMON_URL}/indexes"))
        .json(&body)
        .send()
        .await
        .expect("POST /indexes transport failure");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "POST /indexes returned non-200: body = {:?}",
        resp.text().await.ok()
    );
}

/// Trigger a full force-reindex and wait for every stage to reach `Ready`.
async fn reindex_and_wait(client: &Client) {
    let root = corpus_root();
    let body = json!({
        "root_path": root.to_string_lossy(),
        "force": true,
    });
    let resp = client
        .post(format!("{DAEMON_URL}/indexes/{INDEX_NAME}/reindex"))
        .json(&body)
        .send()
        .await
        .expect("POST /reindex transport failure");
    assert_eq!(resp.status().as_u16(), 200, "POST /reindex non-200");

    // Poll status until every stage is Ready.
    let start = Instant::now();
    loop {
        if start.elapsed() > REINDEX_TIMEOUT {
            // Print last status for diagnostics.
            let status = fetch_status(client).await;
            panic!(
                "synthetic-benchmark reindex did not reach Ready within {:?}\nlast status: {status}",
                REINDEX_TIMEOUT
            );
        }

        let status = fetch_status(client).await;
        let stages_ready = status["stages"].is_object()
            && ["lexical", "semantic", "graph"].iter().all(|stage| {
                status["stages"][stage]["status"]
                    .as_str()
                    .map(|s| s == "ready")
                    .unwrap_or(false)
            });
        if stages_ready {
            let chunks = status["chunk_count"].as_u64().unwrap_or(0);
            println!(
                "    reindex complete: chunks={chunks}, elapsed={:.1}s",
                start.elapsed().as_secs_f64()
            );
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Fetch `/indexes/{INDEX_NAME}/status`.
async fn fetch_status(client: &Client) -> Value {
    let resp = client
        .get(format!("{DAEMON_URL}/indexes/{INDEX_NAME}/status"))
        .send()
        .await
        .expect("GET /status transport failure");
    resp.json().await.expect("status JSON parse failure")
}

/// Delete the synthetic-benchmark index. Best-effort.
async fn cleanup_index(client: &Client) {
    let resp = client
        .delete(format!("{DAEMON_URL}/indexes/{INDEX_NAME}"))
        .send()
        .await;
    match resp {
        Ok(r) => {
            println!(
                "  cleanup DELETE /indexes/{INDEX_NAME} → {}",
                r.status().as_u16()
            );
        }
        Err(e) => println!("  cleanup DELETE failed: {e}"),
    }
}

/// Run one query in one mode and report the result.
async fn run_query(client: &Client, query: &GroundTruthQuery, mode: Mode) -> QueryResult {
    let mut body = json!({
        "text": query.text,
        "top_k": 10,
        "compact": false,
    });
    match mode {
        Mode::Lexical => {
            body["stage"] = json!("lexical");
        }
        Mode::Hybrid => {
            // No stage parameter — full hybrid is the default.
        }
        Mode::KgLeading => {
            body["expand_graph"] = json!(true);
            body["use_kg_first"] = json!(true);
        }
    }

    let t0 = Instant::now();
    let resp = client
        .post(format!("{DAEMON_URL}/indexes/{INDEX_NAME}/search"))
        .json(&body)
        .send()
        .await
        .expect("POST /search transport failure");
    let client_latency_ms = t0.elapsed().as_millis();

    let status = resp.status().as_u16();
    let json_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| panic!("search response JSON parse failure ({status}): {e}"));

    let results = json_body["results"].as_array().cloned().unwrap_or_default();
    let top_files: Vec<String> = results
        .iter()
        .filter_map(|r| r["file"].as_str().map(|s| s.to_string()))
        .collect();

    // The daemon may report `file` as either a path relative to the index
    // root or an absolute path; normalise to relative so we can compare
    // against ground_truth_files.
    let normalised: Vec<String> = top_files.iter().map(|f| normalise_path(f)).collect();

    let hit_at_1 = normalised
        .first()
        .map(|f| any_match(f, &query.ground_truth_files))
        .unwrap_or(false);
    let hit_at_5 = normalised
        .iter()
        .take(5)
        .any(|f| any_match(f, &query.ground_truth_files));

    let intent = json_body["intent"].as_str().unwrap_or("?").to_string();
    let match_reason = results
        .first()
        .and_then(|r| r["match_reason"].as_str())
        .unwrap_or("-")
        .to_string();
    let server_latency_ms = json_body["latency_ms"].as_u64();

    QueryResult {
        query_id: query.id.clone(),
        query_text: query.text.clone(),
        mode,
        top_files: normalised,
        hit_at_1,
        hit_at_5,
        intent,
        match_reason,
        server_latency_ms,
        client_latency_ms,
    }
}

/// Normalise an absolute or repo-relative path to the relative-to-corpus form
/// used by ground_truth_files.
fn normalise_path(file: &str) -> String {
    // Strip leading "./".
    let trimmed = file.trim_start_matches("./");
    // If the daemon reported an absolute path, strip everything up to and
    // including the corpus root segment.
    if let Some(idx) = trimmed.find("benchmark_corpus/synthetic/") {
        let after = &trimmed[idx + "benchmark_corpus/synthetic/".len()..];
        return after.to_string();
    }
    trimmed.to_string()
}

/// Returns true if `result_file` matches any entry in `ground_truth_files`.
/// Match is "result_file ends with the truth path" so both absolute and
/// relative result paths are accepted.
fn any_match(result_file: &str, ground_truth_files: &[String]) -> bool {
    ground_truth_files.iter().any(|truth| {
        result_file == truth
            || result_file.ends_with(truth)
            || result_file.ends_with(&format!("/{truth}"))
    })
}

/// Print a markdown-formatted per-query results table.
fn print_per_query_table(results: &[QueryResult]) {
    println!("\n## Per-query results\n");
    println!(
        "| {:<4} | {:<10} | {:<8} | {:>4} | {:>4} | {:>8} | {:>9} | {:<14} | {:<12} |",
        "ID", "Mode", "Cat", "H@1", "H@5", "srv ms", "client ms", "Intent", "MatchReason"
    );
    println!(
        "|{:-<6}|{:-<12}|{:-<10}|{:-<6}|{:-<6}|{:-<10}|{:-<11}|{:-<16}|{:-<14}|",
        "", "", "", "", "", "", "", "", ""
    );
    for r in results {
        let h1 = if r.hit_at_1 { "Y" } else { "-" };
        let h5 = if r.hit_at_5 { "Y" } else { "-" };
        let srv = r
            .server_latency_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "| {:<4} | {:<10} | {:<8} | {:>4} | {:>4} | {:>8} | {:>9} | {:<14} | {:<12} |",
            r.query_id,
            r.mode.label(),
            "",
            h1,
            h5,
            srv,
            r.client_latency_ms,
            r.intent,
            r.match_reason
        );
    }
}

/// Print the aggregate Hit@K table comparing modes.
fn print_aggregate_table(results: &[QueryResult]) {
    println!("\n## Aggregate by mode\n");
    println!(
        "| {:<10} | {:>10} | {:>10} | {:>13} | {:>13} |",
        "Mode", "Hit@1", "Hit@5", "p50 client ms", "p50 server ms"
    );
    println!(
        "|{:-<12}|{:-<12}|{:-<12}|{:-<15}|{:-<15}|",
        "", "", "", "", ""
    );
    for mode in [Mode::Lexical, Mode::Hybrid, Mode::KgLeading] {
        let subset: Vec<&QueryResult> =
            results.iter().filter(|r| matches!(r.mode, m if std::mem::discriminant(&m) == std::mem::discriminant(&mode))).collect();
        if subset.is_empty() {
            continue;
        }
        let total = subset.len() as f64;
        let h1_count = subset.iter().filter(|r| r.hit_at_1).count();
        let h5_count = subset.iter().filter(|r| r.hit_at_5).count();
        let h1_pct = 100.0 * h1_count as f64 / total;
        let h5_pct = 100.0 * h5_count as f64 / total;

        let mut client_latencies: Vec<u128> = subset.iter().map(|r| r.client_latency_ms).collect();
        client_latencies.sort_unstable();
        let p50_client = percentile(&client_latencies, 50);

        let mut server_latencies: Vec<u64> =
            subset.iter().filter_map(|r| r.server_latency_ms).collect();
        server_latencies.sort_unstable();
        let p50_server = if server_latencies.is_empty() {
            0
        } else {
            server_latencies[server_latencies.len() / 2]
        };

        println!(
            "| {:<10} | {:>4}/{:<2} {:>3.0}% | {:>4}/{:<2} {:>3.0}% | {:>13} | {:>13} |",
            mode.label(),
            h1_count,
            subset.len(),
            h1_pct,
            h5_count,
            subset.len(),
            h5_pct,
            p50_client,
            p50_server
        );
    }
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() * p) / 100).min(sorted.len() - 1);
    sorted[idx]
}

// ── The actual test ─────────────────────────────────────────────────────────

/// Index the synthetic corpus, run every ground-truth query in three modes,
/// print the comparison tables, and clean up.
///
/// Why: this is the FIRST measurement of trusty-search hybrid retrieval that
/// is provably free of BM25 circular bias (issue #123). The numbers it prints
/// land in docs/regression-testing/synthetic-corpus-baseline-*.md.
/// What: the steps documented in the file-level comment.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn benchmark_synthetic_corpus_all_modes() {
    let client = make_client();
    assert_daemon_healthy(&client).await;
    println!("\n=== synthetic-benchmark corpus, three-mode evaluation ===");
    println!("corpus root: {}", corpus_root().display());

    let queries = load_ground_truth();
    println!("loaded {} ground-truth queries", queries.len());

    println!("\nregistering index '{INDEX_NAME}'...");
    register_index(&client).await;

    println!("triggering force-reindex and waiting for Ready...");
    reindex_and_wait(&client).await;

    // Capture chunk count from status for the diagnostics footer.
    let status = fetch_status(&client).await;
    let chunk_count = status["chunk_count"].as_u64().unwrap_or(0);
    let stages: Value = status["stages"].clone();

    let mut all_results: Vec<QueryResult> = Vec::with_capacity(queries.len() * 3);
    for mode in [Mode::Lexical, Mode::Hybrid, Mode::KgLeading] {
        println!("\n--- mode = {} ---", mode.label());
        for q in &queries {
            let result = run_query(&client, q, mode).await;
            println!(
                "  {} [{}]: H@1={} H@5={} top1={}",
                q.id,
                q.text,
                if result.hit_at_1 { "Y" } else { "-" },
                if result.hit_at_5 { "Y" } else { "-" },
                result
                    .top_files
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "<none>".into())
            );
            all_results.push(result);
        }
    }

    print_per_query_table(&all_results);
    print_aggregate_table(&all_results);

    println!("\n## Diagnostics");
    println!("- chunk_count: {chunk_count}");
    println!("- stages: {stages}");

    // Sanity asserts so the test fails loudly if the daemon misbehaved
    // (e.g. zero results across all queries).
    let total_hits = all_results.iter().filter(|r| r.hit_at_5).count();
    assert!(
        total_hits > 0,
        "every single query missed at H@5 — daemon may be misconfigured"
    );

    cleanup_index(&client).await;
}
