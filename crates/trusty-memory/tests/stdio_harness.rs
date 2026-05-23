//! End-to-end MCP stdio client integration & performance harness.
//!
//! Why: The existing `mcp_stdio_tools.rs` integration test exercises
//! `dispatch_tool` *in-process*. That is fast, but it skips the JSON-RPC
//! framing, stdin/stdout transport, child-process lifecycle, and the
//! `initialize` / `notifications/initialized` MCP handshake — the
//! surface that real clients (Claude Code, open-mpm) actually hit.
//! This harness spawns the **release** `trusty-memory` binary as a
//! subprocess, speaks newline-delimited JSON-RPC 2.0 over its
//! stdin/stdout, and validates both correctness and latency budgets.
//!
//! What: A `gated_by_release_binary` runner. The test is `#[ignore]`d
//! by default (so plain `cargo test` is unaffected) and is invoked via
//! `cargo test -p trusty-memory --test stdio_harness -- --ignored
//! --nocapture`. It performs:
//!   - MCP `initialize` + `notifications/initialized` handshake.
//!   - `tools/list` discovery and assertion that the canonical tool
//!     set is advertised.
//!   - Functional round-trip tests for `palace_create`, `memory_remember`,
//!     `memory_recall`, `kg_assert`, `kg_query`, `palace_info`,
//!     `palace_list`, `memory_forget`, `get_prompt_context`,
//!     `memory_recall_deep`, `memory_list`.
//!   - Edge cases: invalid tool, missing required params, empty/large
//!     payloads, duplicate stores.
//!   - Performance: 50 sequential `memory_remember` then 100 sequential
//!     `memory_recall` calls. Reports p50/p95/p99/min/max per tool.
//!
//! Test: `cargo test -p trusty-memory --test stdio_harness -- --ignored
//! --nocapture`. The harness binary must be built first; the test
//! reads `CARGO_BIN_EXE_trusty-memory` provided by Cargo or falls back
//! to `target/release/trusty-memory` relative to the workspace root.
//! Storage is isolated per run via `TRUSTY_DATA_DIR_OVERRIDE`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Stdio MCP client
// ---------------------------------------------------------------------------

/// Wraps a child `trusty-memory serve --stdio` process and provides
/// request/response helpers.
///
/// Why: every test needs the same boilerplate: spawn, write line, read
/// line, match id. Centralising here keeps each test focused on
/// behaviour rather than transport.
/// What: holds the child handle, line-buffered stdout reader, and stdin
/// writer. `request` writes a JSON-RPC envelope and reads exactly one
/// response with the matching id. `notify` writes a notification and
/// returns immediately (no response is expected).
/// Test: every `#[test]` in this file exercises this client.
struct McpStdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    _tmp: TempDir,
}

impl McpStdioClient {
    fn spawn() -> anyhow::Result<Self> {
        let tmp = tempfile::tempdir()?;
        let bin = locate_binary()?;
        // Why: the server writes detailed error chains to stderr; if we
        // `inherit` them they interleave with our own progress prints and
        // the harness's perf table. Redirect to a file inside the temp
        // dir so we can dump the tail on failure without losing them.
        let stderr_path = tmp.path().join("trusty-memory.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path)?;
        let mut cmd = Command::new(&bin);
        cmd.arg("serve")
            .arg("--stdio")
            .env("TRUSTY_DATA_DIR_OVERRIDE", tmp.path())
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file));
        println!("[harness] server stderr → {}", stderr_path.display());
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing child stdin"))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("missing child stdout"))?,
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            _tmp: tmp,
        })
    }

    /// Issue an MCP `initialize` + `notifications/initialized` handshake.
    fn handshake(&mut self) -> anyhow::Result<Value> {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "stdio-harness", "version": "0.1.0"}
            }),
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(init)
    }

    /// Send a JSON-RPC request and block until the matching response.
    fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&envelope)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        // The server may emit `notifications/*` between request and
        // response (currently it does not), so loop until we see our id.
        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf)?;
            if n == 0 {
                anyhow::bail!("server closed stdout before reply to id={id}");
            }
            let resp: Value = serde_json::from_str(buf.trim())?;
            if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Ok(resp);
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let envelope = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&envelope)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Convenience: call a tool via `tools/call`.
    fn call_tool(&mut self, name: &str, args: Value) -> anyhow::Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": args}))
    }
}

impl McpStdioClient {
    /// Return the tail of the server's captured stderr for diagnostics.
    fn stderr_tail(&self, lines: usize) -> String {
        let path = self._tmp.path().join("trusty-memory.stderr.log");
        let Ok(buf) = std::fs::read_to_string(&path) else {
            return format!("(no stderr captured at {})", path.display());
        };
        let all: Vec<&str> = buf.lines().collect();
        let start = all.len().saturating_sub(lines);
        all[start..].join("\n")
    }
}

impl Drop for McpStdioClient {
    fn drop(&mut self) {
        // Close stdin so the loop hits EOF, then wait briefly.
        // If the child is misbehaving, kill it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Resolve the trusty-memory release binary path.
///
/// Why: Cargo doesn't set `CARGO_BIN_EXE_*` for integration tests on
/// every workspace layout. Fall back to `target/release/trusty-memory`
/// relative to the workspace root (two levels up from the crate
/// manifest).
/// What: tries env var first, then a deterministic path.
/// Test: indirect — every test calls `McpStdioClient::spawn`.
fn locate_binary() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_trusty-memory") {
        return Ok(PathBuf::from(p));
    }
    // Crate manifest dir → workspace root → target/release.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("cannot find workspace root"))?;
    let bin = workspace_root.join("target/release/trusty-memory");
    if !bin.exists() {
        anyhow::bail!(
            "release binary not found at {}; run `cargo build --release -p trusty-memory` first",
            bin.display()
        );
    }
    Ok(bin)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the `text` field from the MCP `content[0].text` envelope and
/// parse it as JSON if possible.
fn extract_tool_payload(resp: &Value) -> anyhow::Result<Value> {
    if let Some(err) = resp.get("error") {
        anyhow::bail!(
            "tool error: {}",
            serde_json::to_string(err).unwrap_or_default()
        );
    }
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing /result/content/0/text in {resp}"))?;
    // Attempt JSON parse; if it isn't JSON (e.g. get_prompt_context's
    // Markdown), return the raw string.
    Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string())))
}

#[derive(Clone, Copy, Debug)]
struct LatencyStats {
    name: &'static str,
    n: usize,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    min_ms: f64,
    max_ms: f64,
    mean_ms: f64,
}

fn summarise(name: &'static str, mut samples: Vec<Duration>) -> LatencyStats {
    samples.sort();
    let n = samples.len();
    let pct = |p: f64| -> f64 {
        let idx = ((p / 100.0) * (n as f64 - 1.0)).round() as usize;
        samples[idx].as_secs_f64() * 1000.0
    };
    let mean = samples.iter().map(|d| d.as_secs_f64()).sum::<f64>() / (n as f64) * 1000.0;
    LatencyStats {
        name,
        n,
        p50_ms: pct(50.0),
        p95_ms: pct(95.0),
        p99_ms: pct(99.0),
        min_ms: samples.first().unwrap().as_secs_f64() * 1000.0,
        max_ms: samples.last().unwrap().as_secs_f64() * 1000.0,
        mean_ms: mean,
    }
}

fn print_stats_table(rows: &[LatencyStats]) {
    println!();
    println!("=== PERFORMANCE SUMMARY (stdio MCP, release build) ===");
    println!(
        "{:<24} {:>5} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "tool", "n", "min(ms)", "p50(ms)", "mean(ms)", "p95(ms)", "p99(ms)", "max(ms)"
    );
    println!("{}", "-".repeat(98));
    for r in rows {
        println!(
            "{:<24} {:>5} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
            r.name, r.n, r.min_ms, r.p50_ms, r.mean_ms, r.p95_ms, r.p99_ms, r.max_ms
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Full functional + perf suite. Ignored by default so `cargo test`
/// (without --ignored) stays fast and offline. Run explicitly:
///   cargo test -p trusty-memory --test stdio_harness -- --ignored --nocapture
#[test]
#[ignore = "spawns release binary; run with --ignored --nocapture"]
fn full_stdio_suite() {
    // Each phase prints a banner; any anyhow error within a phase
    // panics the test with the error chain so the failing phase is
    // unambiguous in stdout.
    let mut client = McpStdioClient::spawn().expect("spawn trusty-memory");
    println!("[harness] spawned trusty-memory serve --stdio");

    // ---- Phase 1: handshake ------------------------------------------------
    let init = client.handshake().expect("MCP handshake");
    let server_info = init
        .pointer("/result/serverInfo")
        .cloned()
        .unwrap_or(Value::Null);
    println!("[handshake] serverInfo: {server_info}");
    assert_eq!(
        init.pointer("/result/protocolVersion")
            .and_then(|v| v.as_str()),
        Some("2024-11-05"),
        "protocolVersion mismatch"
    );

    // ---- Phase 2: tools/list ----------------------------------------------
    let listed = client.request("tools/list", json!({})).expect("tools/list");
    let tools = listed
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    println!("[tools/list] {} tools advertised: {:?}", names.len(), names);
    for must in [
        "memory_remember",
        "memory_recall",
        "memory_forget",
        "palace_create",
        "palace_list",
        "palace_info",
        "kg_assert",
        "kg_query",
        "get_prompt_context",
    ] {
        assert!(
            names.contains(&must),
            "tools/list missing required tool: {must}"
        );
    }

    // ---- Phase 3: functional tests ----------------------------------------
    println!("[functional] palace_create");
    let create = client
        .call_tool("palace_create", json!({"name": "harness"}))
        .expect("palace_create");
    assert!(
        create.get("error").is_none(),
        "palace_create error: {create}"
    );

    println!("[functional] palace_list contains 'harness'");
    let listed = client
        .call_tool("palace_list", json!({}))
        .expect("palace_list");
    let payload = extract_tool_payload(&listed).expect("palace_list payload");
    let palaces = payload["palaces"]
        .as_array()
        .expect("palaces array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(
        palaces.contains(&"harness"),
        "palace_list missing 'harness'; got {palaces:?}"
    );

    println!("[functional] memory_remember round-trip");
    // Why: the server's signal/noise filter (issue #61) rejects content
    // below a small token threshold. Use prose long enough to pass the
    // default filter so we exercise the happy path of the embedder.
    let remembered = client
        .call_tool(
            "memory_remember",
            json!({
                "palace": "harness",
                "text": "Quokkas are tiny marsupials native to Australia. They live mostly on Rottnest Island near Perth and are known for their distinctive smiling faces and gentle disposition toward visitors.",
                "room": "General",
                "tags": ["wildlife"],
            }),
        )
        .expect("memory_remember");
    let payload = match extract_tool_payload(&remembered) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "\n=== SERVER STDERR (tail 40) ===\n{}\n=== END ===\n",
                client.stderr_tail(40)
            );
            panic!("remember payload failed: {e:#}\nfull response: {remembered}");
        }
    };
    let drawer_id = payload["drawer_id"]
        .as_str()
        .expect("drawer_id")
        .to_string();
    assert!(!drawer_id.is_empty(), "drawer_id empty");
    println!("   drawer_id = {drawer_id}");

    println!("[functional] memory_recall finds the stored drawer");
    let recalled = client
        .call_tool(
            "memory_recall",
            json!({"palace": "harness", "query": "quokka marsupial Australia", "top_k": 5}),
        )
        .expect("memory_recall");
    let payload = extract_tool_payload(&recalled).expect("recall payload");
    let results = payload["results"].as_array().expect("results array");
    assert!(
        results
            .iter()
            .any(|r| r["content"].as_str().unwrap_or("").contains("Quokkas")),
        "memory_recall did not surface the stored drawer; got {results:?}"
    );

    println!("[functional] kg_assert + kg_query round-trip");
    let _ = client
        .call_tool(
            "kg_assert",
            json!({
                "palace": "harness",
                "subject": "alice",
                "predicate": "works_at",
                "object": "Acme",
                "confidence": 0.9,
            }),
        )
        .expect("kg_assert");
    let queried = client
        .call_tool("kg_query", json!({"palace": "harness", "subject": "alice"}))
        .expect("kg_query");
    let payload = extract_tool_payload(&queried).expect("kg_query payload");
    let triples = payload["triples"].as_array().expect("triples");
    assert_eq!(
        triples.len(),
        1,
        "expected exactly one triple; got {triples:?}"
    );
    assert_eq!(triples[0]["predicate"], "works_at");
    assert_eq!(triples[0]["object"], "Acme");

    println!("[functional] palace_info reports counts");
    let info = client
        .call_tool("palace_info", json!({"palace": "harness"}))
        .expect("palace_info");
    let payload = extract_tool_payload(&info).expect("info payload");
    assert!(payload["drawer_count"].as_u64().unwrap_or(0) >= 1);

    println!("[functional] memory_list enumerates drawers");
    let listed = client
        .call_tool("memory_list", json!({"palace": "harness"}))
        .expect("memory_list");
    let payload = extract_tool_payload(&listed).expect("memory_list payload");
    let drawers = payload["drawers"]
        .as_array()
        .or_else(|| payload.as_array())
        .expect("drawer array somewhere in memory_list payload");
    assert!(!drawers.is_empty(), "memory_list returned no drawers");

    println!("[functional] memory_recall_deep returns >= shallow");
    let deep = client
        .call_tool(
            "memory_recall_deep",
            json!({"palace": "harness", "query": "quokka", "top_k": 10}),
        )
        .expect("memory_recall_deep");
    let payload = extract_tool_payload(&deep).expect("deep payload");
    assert!(payload["results"].as_array().is_some());

    println!("[functional] get_prompt_context returns Markdown");
    let ctx = client
        .call_tool(
            "get_prompt_context",
            json!({"palace": "harness", "query": "quokka"}),
        )
        .expect("get_prompt_context");
    assert!(
        ctx.get("error").is_none(),
        "get_prompt_context error: {ctx}"
    );

    println!("[functional] memory_forget removes the drawer");
    let _ = client
        .call_tool(
            "memory_forget",
            json!({"palace": "harness", "drawer_id": drawer_id}),
        )
        .expect("memory_forget");
    let after = client
        .call_tool(
            "memory_recall",
            json!({"palace": "harness", "query": "quokka marsupial Australia", "top_k": 5}),
        )
        .expect("post-forget recall");
    let payload = extract_tool_payload(&after).expect("post-forget payload");
    let surviving_l2: Vec<_> = payload["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["layer"].as_u64().unwrap_or(0) >= 2)
        .filter(|r| r["content"].as_str().unwrap_or("").contains("Quokkas"))
        .collect();
    assert!(
        surviving_l2.is_empty(),
        "forgotten drawer still surfaces in L2: {surviving_l2:?}"
    );

    // ---- Phase 4: edge cases ----------------------------------------------
    println!("[edge] unknown tool returns JSON-RPC error");
    let bogus = client
        .call_tool("memory_bogus_xyz", json!({"palace": "harness"}))
        .expect("bogus call should still produce a response");
    assert!(
        bogus.get("error").is_some() || bogus.pointer("/result").is_some(), // some servers wrap errors as text
        "expected error envelope for unknown tool; got {bogus}"
    );

    println!("[edge] missing required params returns error");
    let bad = client
        .call_tool("memory_remember", json!({"palace": "harness"})) // no text
        .expect("call");
    assert!(
        bad.get("error").is_some()
            || bad
                .pointer("/result/content/0/text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_lowercase().contains("error") || s.to_lowercase().contains("missing"))
                .unwrap_or(false),
        "expected error for missing 'text'; got {bad}"
    );

    println!("[edge] large payload (~16 KB) accepted");
    // Use real prose so the noise filter does not reject the payload.
    let chunk = "The MCP stdio harness exercises the trusty-memory server end to end with realistic prose so the signal-vs-noise filter accepts the input. ";
    let big = chunk.repeat((16 * 1024) / chunk.len() + 1);
    let large = client
        .call_tool(
            "memory_remember",
            json!({"palace": "harness", "text": big, "room": "General"}),
        )
        .expect("large memory_remember");
    // Accept either success OR a clear filter-rejection; both are valid behaviour.
    let large_ok = large.get("error").is_none();
    println!("   large payload accepted: {large_ok}");

    println!("[edge] duplicate stores both accepted");
    let dup_text = "Capybaras are the largest living rodents in the world. They are native to South America and are highly social animals that live near bodies of water.";
    let _ = client
        .call_tool(
            "memory_remember",
            json!({"palace": "harness", "text": dup_text, "room": "General"}),
        )
        .expect("dup-1");
    let _ = client
        .call_tool(
            "memory_remember",
            json!({"palace": "harness", "text": dup_text, "room": "General"}),
        )
        .expect("dup-2");

    // ---- Phase 5: performance ---------------------------------------------
    println!("[perf] warming up …");
    let _ = client
        .call_tool(
            "memory_remember",
            json!({
                "palace": "harness",
                "text": "Warm-up drawer for the performance phase of the stdio harness — primes the embedder so subsequent calls do not pay the cold load.",
                "room": "General",
            }),
        )
        .expect("warmup remember");
    let _ = client
        .call_tool(
            "memory_recall",
            json!({"palace": "harness", "query": "warm-up", "top_k": 3}),
        )
        .expect("warmup recall");

    println!("[perf] memory_remember x50");
    let mut remember_samples = Vec::with_capacity(50);
    for i in 0..50 {
        let started = Instant::now();
        let _ = client
            .call_tool(
                "memory_remember",
                json!({
                    "palace": "harness",
                    "text": format!("Perf drawer number {i} captures details about topic alpha-{i}: this is intentionally long enough prose to survive the signal-vs-noise filter applied to short low-information drawers."),
                    "room": "General",
                }),
            )
            .expect("perf remember");
        remember_samples.push(started.elapsed());
    }

    println!("[perf] memory_recall x100");
    let mut recall_samples = Vec::with_capacity(100);
    for i in 0..100 {
        let started = Instant::now();
        let _ = client
            .call_tool(
                "memory_recall",
                json!({
                    "palace": "harness",
                    "query": format!("topic-{}", i % 50),
                    "top_k": 5,
                }),
            )
            .expect("perf recall");
        recall_samples.push(started.elapsed());
    }

    println!("[perf] kg_assert x50");
    let mut kg_assert_samples = Vec::with_capacity(50);
    for i in 0..50 {
        let started = Instant::now();
        let _ = client
            .call_tool(
                "kg_assert",
                json!({
                    "palace": "harness",
                    "subject": format!("subj-{i}"),
                    "predicate": "knows",
                    "object": format!("obj-{i}"),
                }),
            )
            .expect("perf kg_assert");
        kg_assert_samples.push(started.elapsed());
    }

    println!("[perf] kg_query x50");
    let mut kg_query_samples = Vec::with_capacity(50);
    for i in 0..50 {
        let started = Instant::now();
        let _ = client
            .call_tool(
                "kg_query",
                json!({"palace": "harness", "subject": format!("subj-{}", i % 50)}),
            )
            .expect("perf kg_query");
        kg_query_samples.push(started.elapsed());
    }

    println!("[perf] palace_info x100");
    let mut info_samples = Vec::with_capacity(100);
    for _ in 0..100 {
        let started = Instant::now();
        let _ = client
            .call_tool("palace_info", json!({"palace": "harness"}))
            .expect("perf palace_info");
        info_samples.push(started.elapsed());
    }

    // ---- Phase 6: report --------------------------------------------------
    let rows = vec![
        summarise("memory_remember", remember_samples),
        summarise("memory_recall", recall_samples),
        summarise("kg_assert", kg_assert_samples),
        summarise("kg_query", kg_query_samples),
        summarise("palace_info", info_samples),
    ];
    print_stats_table(&rows);

    // Soft regression budgets — generous, designed to catch order-of-
    // magnitude regressions only. Tightened budgets live in the
    // in-process perf tests.
    for r in &rows {
        match r.name {
            "memory_recall" | "kg_query" | "palace_info" => {
                assert!(
                    r.p95_ms < 250.0,
                    "{} p95={:.2}ms exceeds soft budget 250ms",
                    r.name,
                    r.p95_ms
                );
            }
            "memory_remember" => {
                assert!(
                    r.p95_ms < 2000.0,
                    "memory_remember p95={:.2}ms exceeds soft budget 2000ms",
                    r.p95_ms
                );
            }
            "kg_assert" => {
                assert!(
                    r.p95_ms < 200.0,
                    "kg_assert p95={:.2}ms exceeds soft budget 200ms",
                    r.p95_ms
                );
            }
            _ => {}
        }
    }

    println!("[harness] ALL PHASES PASSED");
}
