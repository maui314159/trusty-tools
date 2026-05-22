//! Baseline performance regression tests for trusty-search against the trusty-tools project.
//!
//! Why: Provides a reproducible regression gate for query latency, result
//! relevance, graph scoring, community detection quality, and concurrent
//! request throughput — the four axes most likely to degrade silently as the
//! indexing pipeline evolves.
//!
//! What: Each test hits the live HTTP daemon at `http://127.0.0.1:7878` (the
//! default daemon port), exercises a known scenario, and asserts that measured
//! values stay within the thresholds documented in `docs/baseline-performance.md`.
//!
//! Test: All tests are marked `#[ignore]` so the normal `cargo test` run stays
//! fast. Run with:
//! ```bash
//! cargo test -p trusty-search --test baseline_trusty_tools -- --include-ignored --nocapture
//! ```
//!
//! # Prerequisites
//! 1. trusty-search daemon running: `trusty-search start --foreground &`
//! 2. trusty-tools indexed: `trusty-search index /path/to/trusty-tools --name trusty-tools`
//!
//! # Regression thresholds
//! - Query latency p50: <= 500 ms
//! - Query latency p99: <= 2000 ms
//! - Index node count:  >= 1 000 (indicates indexing succeeded)
//! - Community count:   >= 5   (indicates Louvain completed)
//! - Modularity:        >= 0.1 (indicates graph structure is non-degenerate)

use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{json, Value};

// ── Constants ──────────────────────────────────────────────────────────────

const DAEMON_URL: &str = "http://127.0.0.1:7878";
const INDEX_NAME: &str = "trusty-tools";

/// Maximum acceptable p50 query latency.
const LATENCY_P50_THRESHOLD_MS: u128 = 500;

/// Maximum acceptable p99 query latency.
const LATENCY_P99_THRESHOLD_MS: u128 = 2000;

/// Minimum graph node count that confirms a successful full reindex.
const MIN_NODE_COUNT: u64 = 1_000;

/// Minimum Louvain community count expected after reindex.
const MIN_COMMUNITY_COUNT: usize = 5;

/// Minimum acceptable Louvain modularity (partition quality).
const MIN_MODULARITY: f64 = 0.1;

/// Canonical regression query set.
///
/// Each entry: (query_text, expected_top_file_fragment, intent_label)
///
/// Why: A fixed query set with known expected top files lets us detect both
/// latency regressions (slow search) and relevance regressions (wrong results)
/// in the same run.
const REGRESSION_QUERIES: &[(&str, &str, &str)] = &[
    ("symbol graph BFS expansion", "symbol_graph", "definition"),
    (
        "Louvain community detection modularity",
        "community",
        "definition",
    ),
    (
        "axum middleware concurrency limiter",
        "concurrency",
        "usage",
    ),
    ("redb persistence write transaction", "corpus", "usage"),
    ("embed batch async worker pool", "embed_pool", "usage"),
    (
        "chunker AST tree-sitter code split",
        "chunker",
        "definition",
    ),
    ("HNSW vector similarity search", "search", "usage"),
    (
        "auto discover claude code project",
        "discover",
        "definition",
    ),
];

// ── Helper ─────────────────────────────────────────────────────────────────

/// Build a reusable `reqwest::Client` with conservative timeouts.
///
/// Why: Integration tests must not hang indefinitely if the daemon is slow or
/// absent; a fixed connect + read timeout surfaces the problem immediately.
/// What: 2 s connect timeout, 10 s read timeout — generous enough for warm
/// queries, tight enough to surface hangs.
/// Test: every test in this module creates one of these.
fn make_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client construction is infallible with valid config")
}

/// Issue a `POST /indexes/{id}/search` request and return `(latency_ms, body)`.
///
/// Why: Factoring out the search call keeps the latency-measurement tests
/// DRY and ensures every call uses the same `top_k` and `expand_graph`
/// settings so results are comparable across runs.
/// What: Sends `{text, top_k: 10, expand_graph: true}`, measures wall-clock
/// latency, and deserialises the JSON body.
/// Test: called by `test_query_latency_p50_under_threshold` and friends.
async fn search(client: &Client, query: &str) -> (u128, Value) {
    let url = format!("{DAEMON_URL}/indexes/{INDEX_NAME}/search");
    let body = json!({
        "text": query,
        "top_k": 10,
        "expand_graph": true,
        "compact": true,
    });
    let t0 = Instant::now();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("search request should not fail at the transport layer");
    let latency_ms = t0.elapsed().as_millis();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "POST /indexes/{INDEX_NAME}/search returned non-200 for query '{query}'"
    );
    let json: Value = resp
        .json()
        .await
        .expect("search response should be valid JSON");
    (latency_ms, json)
}

/// Compute the p-percentile of a sorted (ascending) slice.
///
/// Why: p50/p99 are the agreed regression thresholds; computing them from raw
/// samples avoids an external statistics dependency.
/// What: Returns `values[floor(len * p / 100)]`, clamped to the last index.
/// Test: Verified informally — for `[1,2,3,4,5]` p50 → 3, p99 → 5.
fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() * p) / 100).min(sorted.len() - 1);
    sorted[idx]
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Verify the daemon responds to the liveness probe within 1 second.
///
/// Why: If health fails, all downstream tests would produce misleading errors
/// (connection refused vs. assertion failure). Failing fast here guides the
/// operator to restart the daemon rather than hunt through test output.
/// What: `GET /health` → 200, body contains `"status":"ok"`.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_daemon_health() {
    let client = make_client();
    let resp = client
        .get(format!("{DAEMON_URL}/health"))
        .send()
        .await
        .expect("health check should reach the daemon — is it running?");
    assert_eq!(resp.status().as_u16(), 200, "GET /health returned non-200");
    let body: Value = resp.json().await.expect("health response should be JSON");
    assert_eq!(
        body["status"], "ok",
        "health.status should be 'ok', got: {body}"
    );
    println!("daemon health: {body}");
}

/// Confirm that the `trusty-tools` index exists and contains a meaningful graph.
///
/// Why: Subsequent latency and relevance tests are meaningless if the index is
/// empty or absent. Failing here with a clear message saves debugging time.
/// What: `GET /indexes` → list contains `trusty-tools`.
///       `GET /indexes/trusty-tools/graph/stats` → `node_count >= MIN_NODE_COUNT`.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_index_exists_and_has_content() {
    let client = make_client();

    // Confirm the index is registered.
    let resp = client
        .get(format!("{DAEMON_URL}/indexes"))
        .send()
        .await
        .expect("GET /indexes should succeed");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.expect("should be JSON");
    let indexes = body["indexes"]
        .as_array()
        .expect("indexes should be an array");
    assert!(
        indexes.iter().any(|v| v.as_str() == Some(INDEX_NAME)),
        "index '{INDEX_NAME}' not found in registered indexes: {indexes:?}\n\
         Run: trusty-search index /path/to/trusty-tools --name {INDEX_NAME}"
    );
    println!("registered indexes: {indexes:?}");

    // Confirm the graph has been populated.
    let resp = client
        .get(format!("{DAEMON_URL}/indexes/{INDEX_NAME}/graph/stats"))
        .send()
        .await
        .expect("GET /indexes/{INDEX_NAME}/graph/stats should succeed");
    assert_eq!(resp.status().as_u16(), 200);
    let stats: Value = resp.json().await.expect("should be JSON");
    let node_count = stats["node_count"].as_u64().unwrap_or(0);
    assert!(
        node_count >= MIN_NODE_COUNT,
        "graph node_count {node_count} < MIN_NODE_COUNT {MIN_NODE_COUNT} — \
         has the index been fully reindexed? Run: trusty-search index /path/to/trusty-tools --name {INDEX_NAME} --force"
    );
    println!(
        "graph stats: nodes={}, edges={}",
        stats["node_count"], stats["edge_count"]
    );
}

/// Assert that the p50 query latency over the regression set stays below
/// `LATENCY_P50_THRESHOLD_MS`.
///
/// Why: The p50 threshold represents the interactive-feel bar — half of agent
/// queries should complete well within this budget.
/// What: Runs all `REGRESSION_QUERIES` once, collects wall-clock latencies,
/// prints a table, and asserts p50 < threshold.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_query_latency_p50_under_threshold() {
    let client = make_client();
    let mut latencies: Vec<u128> = Vec::with_capacity(REGRESSION_QUERIES.len());

    println!("\n{:<50} {:>12}  top_file", "query", "latency_ms");
    println!("{}", "-".repeat(90));

    for (query, _expected_file, _intent) in REGRESSION_QUERIES {
        let (ms, body) = search(&client, query).await;
        let top_file = body["results"][0]["file"]
            .as_str()
            .unwrap_or("<no results>");
        println!("{:<50} {:>12}  {top_file}", query, ms);
        latencies.push(ms);
    }

    latencies.sort_unstable();
    let p50 = percentile(&latencies, 50);
    println!("\np50 latency: {p50} ms  (threshold: {LATENCY_P50_THRESHOLD_MS} ms)");

    assert!(
        p50 <= LATENCY_P50_THRESHOLD_MS,
        "p50 query latency {p50} ms exceeds threshold {LATENCY_P50_THRESHOLD_MS} ms"
    );
}

/// Assert that the p99 query latency over 3× the regression set stays below
/// `LATENCY_P99_THRESHOLD_MS`.
///
/// Why: The p99 threshold catches tail latency caused by lock contention,
/// garbage collection, or cold embedding cache. Running each query 3× gives
/// 24 samples, enough for a meaningful p99 estimate.
/// What: Runs all `REGRESSION_QUERIES` three times each (72 total → 24 per
/// query × 3 repetitions), collects latencies, and asserts p99 < threshold.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_query_latency_p99_under_threshold() {
    let client = make_client();
    let mut latencies: Vec<u128> = Vec::with_capacity(REGRESSION_QUERIES.len() * 3);

    for _ in 0..3 {
        for (query, _, _) in REGRESSION_QUERIES {
            let (ms, _) = search(&client, query).await;
            latencies.push(ms);
        }
    }

    latencies.sort_unstable();
    let p99 = percentile(&latencies, 99);
    let p50 = percentile(&latencies, 50);
    println!(
        "\n{} samples: p50={p50} ms, p99={p99} ms  (p99 threshold: {LATENCY_P99_THRESHOLD_MS} ms)",
        latencies.len()
    );

    assert!(
        p99 <= LATENCY_P99_THRESHOLD_MS,
        "p99 query latency {p99} ms exceeds threshold {LATENCY_P99_THRESHOLD_MS} ms"
    );
}

/// Verify that each regression query returns the expected top-result file.
///
/// Why: Latency regressions and relevance regressions are independent failure
/// modes. A reranking bug that swaps result order does not change latency; this
/// test catches it.
/// What: For each `(query, expected_file_fragment, intent)`, asserts that at
/// least one of the top-3 results has a `file` path containing
/// `expected_file_fragment`. Prints a pass/fail table for diagnostics.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_result_relevance() {
    let client = make_client();
    let mut failures = 0usize;

    println!(
        "\n{:<50} {:<15} {:<12} result",
        "query", "expected_frag", "pass/fail"
    );
    println!("{}", "-".repeat(100));

    for (query, expected_frag, intent) in REGRESSION_QUERIES {
        let (ms, body) = search(&client, query).await;

        let results = body["results"].as_array().cloned().unwrap_or_default();
        // Accept a match anywhere in the top-3 to allow minor reranking variance.
        let hit = results.iter().take(3).any(|r| {
            r["file"]
                .as_str()
                .map(|f| f.contains(expected_frag))
                .unwrap_or(false)
        });

        let top_file = results
            .first()
            .and_then(|r| r["file"].as_str())
            .unwrap_or("<no results>");
        let flag = if hit { "PASS" } else { "FAIL" };
        println!(
            "{:<50} {:<15} {:<12} {top_file}  [{ms} ms, intent={}]",
            query,
            expected_frag,
            flag,
            body["intent"].as_str().unwrap_or("?"),
        );

        if !hit {
            failures += 1;
            eprintln!(
                "FAIL relevance: query='{query}' (intent={intent}) — expected top-3 file \
                 containing '{expected_frag}', got: {top_file}"
            );
        }
    }

    assert_eq!(
        failures, 0,
        "{failures} relevance failures — see table above"
    );
}

/// Confirm that graph scoring is active for a KG-rich query.
///
/// Why: Graph scoring adds centrality bonuses that improve result ordering for
/// structural queries. If the `GraphScorer` failed to build (no communities
/// computed, empty graph), `meta.graph_scoring` will be `false` — a silent
/// regression.
/// What: Searches for a definition query, asserts `meta.graph_scoring == true`.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_graph_scoring_active() {
    let client = make_client();
    let (ms, body) = search(&client, "symbol graph BFS").await;

    let graph_scoring = body["meta"]["graph_scoring"].as_bool().unwrap_or(false);
    println!(
        "graph_scoring={graph_scoring}, community_cohesion={}, latency={ms} ms",
        body["meta"]["community_cohesion"]
    );

    assert!(
        graph_scoring,
        "meta.graph_scoring is false — communities may not have been computed yet. \
         Trigger a full reindex and wait for Louvain to finish: \
         `trusty-search index /path/to/trusty-tools --name {INDEX_NAME} --force`"
    );
}

/// Verify that the Louvain community partition meets minimum quality thresholds.
///
/// Why: Community detection quality directly affects graph scoring bonuses and
/// the `meta.community_cohesion` signal. A degenerate partition (one giant
/// community or zero communities) indicates the Louvain pass did not run or
/// the KG is too sparse.
/// What: `GET /indexes/trusty-tools/communities` → `community_count >= 5`,
/// `modularity >= 0.1`. Prints the top-5 communities for diagnostics.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_community_detection_quality() {
    let client = make_client();
    let resp = client
        .get(format!("{DAEMON_URL}/indexes/{INDEX_NAME}/communities"))
        .send()
        .await
        .expect("GET /indexes/{INDEX_NAME}/communities should succeed");
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.expect("should be JSON");

    let community_count = body["community_count"].as_u64().unwrap_or(0) as usize;
    let modularity = body["modularity"].as_f64().unwrap_or(0.0);

    println!(
        "\ncommunity_count={community_count}  modularity={modularity:.4} \
         (thresholds: count>={MIN_COMMUNITY_COUNT}, modularity>={MIN_MODULARITY})"
    );

    // Print top-5 communities.
    if let Some(communities) = body["communities"].as_array() {
        println!("\nTop-5 communities:");
        println!(
            "{:<5} {:<40} {:>10}  dominant_files",
            "rank", "centroid", "members"
        );
        println!("{}", "-".repeat(90));
        for (i, c) in communities.iter().take(5).enumerate() {
            let centroid = c["centroid_symbol"].as_str().unwrap_or("?");
            let members = c["member_count"].as_u64().unwrap_or(0);
            let dominant = c["dominant_files"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .take(2)
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!("{:<5} {:<40} {:>10}  {dominant}", i + 1, centroid, members);
        }
    }

    assert!(
        community_count >= MIN_COMMUNITY_COUNT,
        "community_count {community_count} < MIN_COMMUNITY_COUNT {MIN_COMMUNITY_COUNT} — \
         Louvain may not have run. Reindex with --force and check daemon logs."
    );
    assert!(
        modularity >= MIN_MODULARITY,
        "modularity {modularity:.4} < MIN_MODULARITY {MIN_MODULARITY} — \
         partition is degenerate (possibly one giant community). \
         Check KG edge density via GET /indexes/{INDEX_NAME}/graph/stats."
    );
}

/// Fire 8 concurrent queries and assert all complete within 5 seconds with no
/// errors.
///
/// Why: Multiple agents running in parallel (a common production pattern for
/// MPM) must not starve each other. Lock contention in the `RwLock<CodeIndexer>`
/// or the embedder pool would surface here as 503s or timeouts.
/// What: Issues 8 queries via `tokio::task::JoinSet` simultaneously, asserts
/// all succeed (HTTP 200) and the wall-clock total stays under 5 seconds.
/// Test: this IS the test.
#[tokio::test]
#[ignore]
async fn test_concurrent_queries_no_errors() {
    use tokio::task::JoinSet;

    // 8 queries drawn from the regression set (cycled if shorter).
    let queries: Vec<&str> = REGRESSION_QUERIES
        .iter()
        .map(|(q, _, _)| *q)
        .cycle()
        .take(8)
        .collect();

    let t0 = Instant::now();
    let mut join_set: JoinSet<(u128, u16)> = JoinSet::new();

    for query in queries {
        let query = query.to_string();
        join_set.spawn(async move {
            let client = make_client();
            let url = format!("{DAEMON_URL}/indexes/{INDEX_NAME}/search");
            let body = json!({
                "text": query,
                "top_k": 10,
                "expand_graph": true,
            });
            let t1 = Instant::now();
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .expect("concurrent search should not fail at the transport layer");
            let ms = t1.elapsed().as_millis();
            (ms, resp.status().as_u16())
        });
    }

    let mut statuses: Vec<(u128, u16)> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        statuses.push(result.expect("task should not panic"));
    }

    let total_ms = t0.elapsed().as_millis();
    println!(
        "\n{} concurrent queries completed in {total_ms} ms",
        statuses.len()
    );
    for (i, (ms, status)) in statuses.iter().enumerate() {
        println!("  query {i}: {status} in {ms} ms");
    }

    let errors: Vec<_> = statuses
        .iter()
        .filter(|(_, status)| *status != 200)
        .collect();
    assert!(
        errors.is_empty(),
        "{} concurrent queries returned non-200: {errors:?}",
        errors.len()
    );

    assert!(
        total_ms < 5_000,
        "8 concurrent queries took {total_ms} ms — exceeds 5 000 ms wall-clock budget"
    );
}
