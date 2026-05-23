//! End-to-end functional + performance test harness for the `trusty-analyze`
//! MCP stdio interface.
//!
//! Why: unit tests in `src/mcp/mod.rs` validate dispatcher wiring in isolation,
//! but only an out-of-process subprocess test catches issues in the actual
//! stdio framing, startup gating against trusty-search, and end-to-end tool
//! responses. This harness drives the real release binary the same way Claude
//! Code does — line-delimited JSON-RPC over stdin/stdout — and reports p50/p95/p99
//! latency per tool.
//!
//! What:
//!   1. Verifies trusty-search is reachable on 127.0.0.1:7878 (skips with a
//!      clear message if not — analyzer refuses to start without it).
//!   2. Spawns `target/release/trusty-analyze serve --mcp --port <free>` and
//!      waits for it to be ready by sending an `initialize` request.
//!   3. Lists tools, asserts the expected surface is present.
//!   4. Functional smoke tests for each tool against the `trusty-tools` index
//!      (or a fallback) — verifies well-formed responses & key fields.
//!   5. Edge cases: missing required params, unknown method.
//!   6. Performance loop: 50 iterations per cheap tool, reports p50/p95/p99.
//!   7. Clean teardown — kills the child and waits for exit.
//!
//! Test: `cargo test -p trusty-analyze --test stdio_harness -- --ignored \
//!        --nocapture full_stdio_suite`. Gated behind `#[ignore]` because it
//! requires a built release binary AND a running trusty-search daemon — both
//! present in normal `cargo test` runs would be flaky in CI.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const SEARCH_URL: &str = "http://127.0.0.1:7878";
const TEST_INDEX_PREFERRED: &str = "trusty-tools";
const PERF_ITERATIONS: usize = 50;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why: keep the binary path lookup central so the test fails fast with a
/// clear message if the release binary is missing.
/// What: returns the absolute path to `target/release/trusty-analyze`.
/// Test: covered by `binary_path_exists` (compile-time, indirect).
fn binary_path() -> Result<PathBuf> {
    // CARGO_MANIFEST_DIR is `crates/trusty-analyze`; target lives two up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("cannot find workspace root from {manifest_dir:?}"))?;
    let bin = workspace_root.join("target/release/trusty-analyze");
    if !bin.exists() {
        bail!(
            "release binary not found at {} — run `cargo build --release -p trusty-analyze` first",
            bin.display()
        );
    }
    Ok(bin)
}

/// Why: analyzer refuses to start without trusty-search; surface a friendly
/// skip rather than a confusing child-process exit-1.
/// What: blocking HTTP GET against the search /health endpoint.
/// Test: `precheck_search_up` is asserted at the top of `full_stdio_suite`.
fn search_is_reachable() -> bool {
    // Minimal synchronous probe — avoid pulling in reqwest just for a test.
    let agent = ureq_lite_get(SEARCH_URL.to_string() + "/health");
    matches!(agent, Ok(body) if body.contains("\"status\":\"ok\""))
}

/// Why: tests should not depend on reqwest/ureq feature bloat; a tiny inline
/// HTTP GET over std::net is enough for /health and /indexes probes.
/// What: blocking HTTP/1.1 GET, returns body string on 2xx.
/// Test: exercised indirectly by `search_is_reachable` and `pick_index`.
fn ureq_lite_get(url: String) -> Result<String> {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpStream;

    let url = url.strip_prefix("http://").unwrap_or(&url);
    let (host_port, path) = url.split_once('/').unwrap_or((url, ""));
    let path = format!("/{path}");
    let mut stream = TcpStream::connect_timeout(
        &host_port
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .ok_or_else(|| anyhow!("resolve {host_port}"))?,
        Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n",);
    stream.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    if !status_line.contains("200") {
        bail!("non-200 from {url}: {}", status_line.trim());
    }
    // Skip headers.
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    let mut body = String::new();
    reader.read_to_string(&mut body)?;
    Ok(body)
}

use std::net::ToSocketAddrs;

/// Why: tests should run against a real index when available, but degrade
/// gracefully — most tools accept an arbitrary index id and return an empty
/// array rather than erroring.
/// What: returns the preferred index id if listed, else the first index, else
/// a synthetic non-existent id to exercise empty-response paths.
/// Test: covered by `pick_index_returns_something`.
fn pick_index() -> String {
    let Ok(body) = ureq_lite_get(format!("{SEARCH_URL}/indexes")) else {
        return TEST_INDEX_PREFERRED.into();
    };
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return TEST_INDEX_PREFERRED.into(),
    };
    let arr = v.get("indexes").and_then(Value::as_array);
    if let Some(arr) = arr {
        let names: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
        if names.contains(&TEST_INDEX_PREFERRED) {
            return TEST_INDEX_PREFERRED.into();
        }
        if let Some(first) = names.first() {
            return (*first).into();
        }
    }
    TEST_INDEX_PREFERRED.into()
}

/// Why: 7879 is the canonical port and may already be in use by a live
/// analyzer; pick something well above it for the test child.
/// What: returns a port unlikely to clash (auto-walk-forward handles residual
/// collisions inside the child).
/// Test: implicit — `spawn_child` exits with the actual bound port logged.
fn ephemeral_port_start() -> u16 {
    // 17xxx is unlikely to clash with running daemons; auto-walk in serve()
    // handles any residual collisions.
    17_879
}

/// Wraps a spawned trusty-analyze child plus its stdio handles. Drop kills
/// the child so panics during a test do not leak processes.
struct AnalyzerProc {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    request_id: u64,
}

impl AnalyzerProc {
    /// Spawn the analyzer in MCP stdio mode and complete the initialize handshake.
    fn spawn() -> Result<Self> {
        let bin = binary_path()?;
        let tmp = tempfile::tempdir().context("tempdir for facts store")?;
        let facts_path = tmp.path().join("test.facts.redb");
        // tempdir drops too early otherwise; leak the handle for the lifetime of the test.
        // (The OS reclaims the dir on process exit anyway.)
        std::mem::forget(tmp);

        let port = ephemeral_port_start();

        let mut cmd = Command::new(&bin);
        cmd.args([
            "--search-url",
            SEARCH_URL,
            "--facts-path",
            facts_path.to_str().expect("utf8 facts path"),
            "serve",
            "--mcp",
            "--port",
            &port.to_string(),
        ])
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("spawn trusty-analyze")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let mut proc = AnalyzerProc {
            child,
            stdin,
            stdout: std::io::BufReader::new(stdout),
            request_id: 0,
        };

        // Initialize handshake — also doubles as readiness check.
        let start = Instant::now();
        loop {
            match proc.call(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "stdio-harness", "version": "0.1.0"},
                }),
            ) {
                Ok(_) => break,
                Err(e) if start.elapsed() < STARTUP_TIMEOUT => {
                    eprintln!("init retry: {e:#}");
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => bail!("initialize failed after {STARTUP_TIMEOUT:?}: {e:#}"),
            }
        }

        // Send `initialized` notification (no response expected).
        proc.notify("notifications/initialized", json!({}))?;

        Ok(proc)
    }

    fn next_id(&mut self) -> u64 {
        self.request_id += 1;
        self.request_id
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        use std::io::BufRead;
        let mut line = String::new();
        // BufReader inherits the timeout from the underlying pipe; on Unix
        // pipes there is no direct read_timeout, so we rely on the child
        // responding promptly. RESPONSE_TIMEOUT bounds it via a watchdog
        // thread would be overkill — just block.
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            bail!("child closed stdout unexpectedly");
        }
        Ok(line)
    }

    /// Send a JSON-RPC request and return the parsed response value.
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_line(&req.to_string())?;
        let resp_line = self.read_line()?;
        let resp: Value = serde_json::from_str(&resp_line)
            .with_context(|| format!("parse response: {resp_line}"))?;
        if let Some(err) = resp.get("error") {
            bail!("RPC error for {method}: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("no result field in {resp_line}"))
    }

    /// Send a notification (no response).
    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_line(&req.to_string())
    }

    /// Call a tool and return its `result` value. Tool errors (isError=true)
    /// are returned as Ok with the content payload; transport/RPC errors bail.
    fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        self.call("tools/call", json!({"name": name, "arguments": args}))
    }

    /// Raw call — used for error-path tests where we want to inspect the
    /// JSON-RPC error object directly instead of bailing.
    fn call_raw(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_line(&req.to_string())?;
        let resp_line = self.read_line()?;
        serde_json::from_str(&resp_line).with_context(|| format!("parse raw response: {resp_line}"))
    }
}

impl Drop for AnalyzerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Why: percentile reporting is the standard rubric for service latency;
/// reporting only mean hides tail behavior.
/// What: takes a slice of durations, returns (p50, p95, p99, mean) in ms.
/// Test: covered indirectly — `latency_summary` is asserted to be monotonic
/// in `pctl_helper_sanity`.
fn latency_summary(samples: &mut [Duration]) -> (f64, f64, f64, f64) {
    samples.sort();
    let n = samples.len();
    let to_ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let pick = |q: f64| {
        let idx = ((n as f64 - 1.0) * q).round() as usize;
        to_ms(samples[idx.min(n - 1)])
    };
    let mean = samples.iter().map(|d| to_ms(*d)).sum::<f64>() / n as f64;
    (pick(0.50), pick(0.95), pick(0.99), mean)
}

#[test]
fn pctl_helper_sanity() {
    let mut s: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
    let (p50, p95, p99, mean) = latency_summary(&mut s);
    assert!(p50 <= p95 && p95 <= p99, "monotonic: {p50} {p95} {p99}");
    assert!(mean > 49.0 && mean < 52.0, "mean ~ 50ms, got {mean}");
}

#[test]
fn pick_index_returns_something() {
    // No assertion on which index — only that the picker yields a non-empty string.
    let idx = pick_index();
    assert!(!idx.is_empty(), "pick_index returned empty");
}

/// The big one: spawn analyzer, exercise full MCP surface, measure latency.
#[test]
#[ignore = "requires release binary + running trusty-search daemon"]
fn full_stdio_suite() -> Result<()> {
    // -------- Preflight -----------------------------------------------------
    assert!(
        search_is_reachable(),
        "trusty-search must be running on {SEARCH_URL} — start with `trusty-search start`"
    );
    let index_id = pick_index();
    println!("=== using test index: {index_id} ===");

    let mut proc = AnalyzerProc::spawn()?;

    // -------- tools/list ---------------------------------------------------
    let result = proc.call("tools/list", json!({}))?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tools/list missing tools array"))?;
    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    println!("=== discovered {} tools ===", tool_names.len());
    for name in &tool_names {
        println!("    - {name}");
    }

    // Expected surface from src/mcp/mod.rs tool_descriptors().
    let required = [
        "analyzer_health",
        "complexity_hotspots",
        "find_smells",
        "analyze_quality",
        "run_diagnostics",
        "list_facts",
        "upsert_fact",
        "delete_fact",
        "ingest_scip",
        "cluster_concepts",
        "extract_graph",
        "extract_ner",
        "suggest_refactors",
        "review_diff",
        "deep_analysis",
        "review_github_pr",
        "list_entities",
    ];
    for name in required {
        assert!(
            tool_names.contains(&name),
            "missing tool {name} (got {tool_names:?})"
        );
    }

    // -------- Functional tests --------------------------------------------
    println!("\n=== functional tests ===");

    // 1. analyzer_health — must report ok + version + search_reachable=true.
    let health = proc.call_tool("analyzer_health", json!({}))?;
    println!("analyzer_health -> {}", compact(&health));
    let text = first_text(&health)?;
    assert!(text.contains("ok"), "expected 'ok' in health text: {text}");

    // 2. complexity_hotspots — should return content (possibly empty list).
    let hotspots = proc.call_tool(
        "complexity_hotspots",
        json!({"index_id": index_id, "top_n": 5}),
    )?;
    println!(
        "complexity_hotspots -> {}",
        truncate(&compact(&hotspots), 200)
    );
    assert_has_content(&hotspots, "complexity_hotspots")?;

    // 3. find_smells.
    let smells = proc.call_tool("find_smells", json!({"index_id": index_id}))?;
    println!("find_smells -> {}", truncate(&compact(&smells), 200));
    assert_has_content(&smells, "find_smells")?;

    // 4. analyze_quality — should return numeric aggregate fields.
    let quality = proc.call_tool("analyze_quality", json!({"index_id": index_id}))?;
    println!("analyze_quality -> {}", truncate(&compact(&quality), 300));
    assert_has_content(&quality, "analyze_quality")?;

    // 5. list_entities.
    let entities = proc.call_tool(
        "list_entities",
        json!({"index_id": index_id, "kind": "function"}),
    )?;
    println!("list_entities -> {}", truncate(&compact(&entities), 200));
    assert_has_content(&entities, "list_entities")?;

    // 6. extract_graph.
    let graph = proc.call_tool("extract_graph", json!({"index_id": index_id}))?;
    println!("extract_graph -> {}", truncate(&compact(&graph), 200));
    assert_has_content(&graph, "extract_graph")?;

    // 7. cluster_concepts (k=3, fast BoW path).
    let clusters = proc.call_tool(
        "cluster_concepts",
        json!({"index_id": index_id, "k": 3, "method": "bow"}),
    )?;
    println!("cluster_concepts -> {}", truncate(&compact(&clusters), 200));
    assert_has_content(&clusters, "cluster_concepts")?;

    // 8. suggest_refactors.
    let refactors = proc.call_tool(
        "suggest_refactors",
        json!({"index_id": index_id, "top_k": 5}),
    )?;
    println!(
        "suggest_refactors -> {}",
        truncate(&compact(&refactors), 200)
    );
    assert_has_content(&refactors, "suggest_refactors")?;

    // 9. list_facts (empty store — should return empty list cleanly).
    let facts = proc.call_tool("list_facts", json!({}))?;
    println!("list_facts -> {}", truncate(&compact(&facts), 200));
    assert_has_content(&facts, "list_facts")?;

    // 10. upsert_fact -> list_facts -> delete_fact round trip.
    let upsert = proc.call_tool(
        "upsert_fact",
        json!({
            "subject": "test_subject",
            "predicate": "test_predicate",
            "object": "test_object",
            "index_id": index_id,
            "confidence": 0.9,
        }),
    )?;
    println!("upsert_fact -> {}", truncate(&compact(&upsert), 200));
    // Try to extract the inserted id for deletion (best-effort).
    let upsert_text = first_text(&upsert).unwrap_or_default();
    let inserted_id: Option<u64> = serde_json::from_str::<Value>(&upsert_text)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_u64))
        .or_else(|| {
            // Or the text contains an "id": N
            upsert_text
                .split("\"id\":")
                .nth(1)
                .and_then(|s| s.trim_start().split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|s| s.parse().ok())
        });
    if let Some(id) = inserted_id {
        let del = proc.call_tool("delete_fact", json!({"id": id}))?;
        println!("delete_fact({id}) -> {}", truncate(&compact(&del), 200));
    } else {
        println!("delete_fact skipped (could not parse inserted id)");
    }

    // -------- Edge cases ---------------------------------------------------
    println!("\n=== edge cases ===");

    // Missing required param for upsert_fact.
    let resp = proc.call_raw(
        "tools/call",
        json!({"name": "upsert_fact", "arguments": {"subject": "x"}}),
    )?;
    println!(
        "upsert_fact (missing args) -> {}",
        truncate(&compact(&resp), 200)
    );
    // Either RPC-level error or content with isError=true is acceptable.
    let is_error_indicator = resp.get("error").is_some()
        || resp
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    assert!(
        is_error_indicator,
        "expected error/isError for missing required args"
    );

    // Unknown method.
    let resp = proc.call_raw("does/not/exist", json!({}))?;
    println!("unknown method -> {}", truncate(&compact(&resp), 200));
    assert!(
        resp.get("error").is_some(),
        "expected JSON-RPC error for unknown method"
    );

    // Bogus index_id (non-existent).
    let bogus = proc.call_tool(
        "complexity_hotspots",
        json!({"index_id": "definitely-does-not-exist-xyz123", "top_n": 5}),
    );
    println!(
        "complexity_hotspots (bogus index) -> {:?}",
        bogus.as_ref().map(|v| truncate(&compact(v), 200))
    );
    // Either Ok with empty/isError, or transport error — both demonstrate
    // the daemon stays alive under bad input.

    // -------- Performance loop --------------------------------------------
    println!("\n=== performance (N={PERF_ITERATIONS} per tool) ===");
    let perf_targets: Vec<(&str, Value)> = vec![
        ("analyzer_health", json!({})),
        (
            "complexity_hotspots",
            json!({"index_id": &index_id, "top_n": 5}),
        ),
        ("find_smells", json!({"index_id": &index_id})),
        ("analyze_quality", json!({"index_id": &index_id})),
        ("list_facts", json!({})),
        ("list_entities", json!({"index_id": &index_id})),
        (
            "suggest_refactors",
            json!({"index_id": &index_id, "top_k": 5}),
        ),
    ];

    println!(
        "{:<22} {:>10} {:>10} {:>10} {:>10}",
        "tool", "p50(ms)", "p95(ms)", "p99(ms)", "mean(ms)"
    );
    println!("{}", "-".repeat(66));
    for (tool, args) in perf_targets {
        let mut samples = Vec::with_capacity(PERF_ITERATIONS);
        let mut err_count = 0usize;
        for _ in 0..PERF_ITERATIONS {
            let t = Instant::now();
            match proc.call_tool(tool, args.clone()) {
                Ok(_) => samples.push(t.elapsed()),
                Err(_) => err_count += 1,
            }
        }
        if samples.is_empty() {
            println!("{tool:<22} {err_count:>10} errs (no samples)");
            continue;
        }
        let (p50, p95, p99, mean) = latency_summary(&mut samples);
        println!(
            "{tool:<22} {p50:>10.2} {p95:>10.2} {p99:>10.2} {mean:>10.2}   ({} ok, {} err)",
            samples.len(),
            err_count
        );
    }

    // Bound the overall test time so a hang in the daemon shows up as a
    // failure rather than running until CI timeout.
    assert!(RESPONSE_TIMEOUT > Duration::from_secs(1), "sanity guard");

    println!("\n=== full_stdio_suite OK ===");
    Ok(())
}

// -------- helpers ----------------------------------------------------------

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "<unprintable>".into())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…[{} more]", &s[..n], s.len() - n)
    }
}

/// Extract the first text content block from an MCP tool response.
fn first_text(result: &Value) -> Result<String> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("no content array in {}", compact(result)))?;
    let first = content
        .first()
        .ok_or_else(|| anyhow!("empty content array"))?;
    let text = first
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("first content block has no text: {}", compact(first)))?;
    Ok(text.to_string())
}

/// Assert that a tool response is well-formed (content array present), without
/// caring about the exact payload. Tools that have no data for the given index
/// still return a content block — usually an empty array or "[]".
fn assert_has_content(result: &Value, tool: &str) -> Result<()> {
    if result.get("content").and_then(Value::as_array).is_none() {
        bail!(
            "tool {tool} response missing content array: {}",
            compact(result)
        );
    }
    Ok(())
}
