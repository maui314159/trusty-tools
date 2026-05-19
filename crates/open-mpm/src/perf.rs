//! Performance instrumentation — per-phase latency + token/cost capture.
//!
//! Why: (#47) We need to track how long each workflow phase takes, how many
//! tokens it consumed (prompt / completion / Anthropic cache read / cache
//! creation), and the resulting USD cost so we can compare runs build-over-
//! build and catch regressions or prompt-caching wins. Persisting one JSON
//! file per run under `docs/performance/runs/` plus a one-line summary log
//! keeps the data greppable without a database.
//! What: `PerfCollector` is constructed at workflow start, `record_phase`
//! appends a `PhaseRecord` after each phase, and `flush` writes the final
//! JSON + log line. `TokenUsage` is the provider-agnostic shape captured
//! from each LLM call (extended with Anthropic-specific `cache_read` /
//! `cache_creation` fields which are zero for non-Anthropic models).
//! Test: `cost_usd_known_model`, `collector_records_phases`,
//! `collector_totals_sum_phases`, `collector_flush_writes_json_and_log`.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Token usage captured from a single LLM round-trip.
///
/// Why: (#50) Anthropic's OpenRouter responses carry cache_read_input_tokens
/// and cache_creation_input_tokens alongside the standard prompt/completion
/// counts. Exposing those as first-class fields lets `PerfCollector` track
/// cache effectiveness over time. For non-Anthropic models these stay 0.
/// What: Plain struct; additive (`+`-style) accumulation is done inline in
/// `PerfCollector::totals`.
/// Test: `token_usage_default_is_zeros`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
}

impl TokenUsage {
    /// Construct a `TokenUsage` from the four individual counts.
    pub fn new(prompt: u32, completion: u32, cache_read: u32, cache_creation: u32) -> Self {
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        }
    }

    /// Add another usage record into this one (in place).
    ///
    /// Why: Each phase may comprise multiple LLM turns (tool loop); we sum
    /// them into a single `PhaseRecord`.
    /// What: Field-wise saturating add.
    /// Test: `token_usage_accumulates`.
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
    }
}

/// One phase's measured performance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseRecord {
    pub name: String,
    pub duration_ms: u64,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub cost_usd: f64,
}

/// Totals rolled up from every `PhaseRecord` in a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfTotals {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub cost_usd: f64,
}

/// Full performance record persisted to `docs/performance/runs/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfRecord {
    pub build: u64,
    pub version: String,
    pub workflow: String,
    pub task_preview: String,
    pub started_at: String,
    pub total_duration_ms: u64,
    pub phases: Vec<PhaseRecord>,
    pub totals: PerfTotals,
    /// Run outcome (#56): "success" | "partial" | "failed".
    ///
    /// Why: When a phase fails mid-workflow, we still want to persist the
    /// perf record so we can see where/how the run died. A separate `status`
    /// field distinguishes clean completions from partial runs in tooling
    /// that scans `runs/*.json`.
    /// What: Defaults to "success" for back-compat with older fixtures.
    #[serde(default = "default_status")]
    pub status: String,
    /// Name of the phase that failed, when `status != "success"` (#56).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_phase: Option<String>,
    /// Distinct skill names injected into any phase prompt during this run
    /// (#171).
    ///
    /// Why: Post-run persistence needs to know which skills were used so it
    /// can increment `use_count` and refresh `last_used`. Carrying the list
    /// through `PerfRecord` keeps the data in one place that's already
    /// flushed to disk for analysis.
    /// What: Deduplicated, insertion-order-preserved list of skill names.
    /// Defaults to empty for back-compat with older fixtures.
    #[serde(default)]
    pub skills_used: Vec<String>,
    /// Distinct skill names matched by the pre-plan discovery step (#173).
    ///
    /// Why: Discovery surfaces every skill the engine considered relevant for
    /// a task — even if downstream prompt assembly only injected a subset (or
    /// none). Recording the broader candidate set separately from
    /// `skills_used` lets us measure recall vs. precision over time and
    /// audit which signals the discovery step matched on.
    /// What: Deduplicated, insertion-order-preserved list of skill names.
    /// Defaults to empty for back-compat with older fixtures.
    #[serde(default)]
    pub skills_considered: Vec<String>,
    /// Tests passed in the QA phase, when the QA agent emitted a parsable
    /// JSON envelope with a `passed` count.
    ///
    /// Why: Run-over-run comparisons want raw test counts, not just the
    /// pass/fail status. Tracking `tests_passed`/`tests_failed` separately
    /// from `status` lets dashboards show "1118/1118 passing" without
    /// re-parsing QA output.
    /// What: `Some(N)` when QA returned a JSON envelope with `passed`,
    /// `None` otherwise. Defaults to `None` for back-compat with older
    /// fixtures and for runs that skip the QA phase.
    /// Test: `test_run_record_serializes_test_counts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_passed: Option<u64>,
    /// Tests failed in the QA phase, paired with `tests_passed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_failed: Option<u64>,
}

fn default_status() -> String {
    "success".to_string()
}

/// Accumulator used during workflow execution.
///
/// Why: Keeping the collector off the workflow context lets us evolve perf
/// schema without churning `WorkflowContext`; the engine just calls
/// `record_phase`/`flush`.
/// What: Holds the build stamp, workflow name, a 120-char preview of the
/// user task, the wall-clock start `Instant`, and a growing `Vec<PhaseRecord>`.
/// `flush(out_dir)` writes `runs/YYYYMMDD-HHMMSS-build<N>.json` and appends a
/// summary line to `runs.log`.
/// Test: see unit tests at bottom of this file.
pub struct PerfCollector {
    build: u64,
    workflow: String,
    task_preview: String,
    started_at: Instant,
    started_at_iso: String,
    phases: Vec<PhaseRecord>,
    model: String,
    /// #56: run status; set via `set_status`. Defaults to "success".
    status: String,
    /// #56: name of the phase that failed, if any.
    failed_phase: Option<String>,
    /// #171: distinct skills injected during the run, insertion-ordered.
    skills_used: Vec<String>,
    /// #173: distinct skills matched by pre-plan discovery, insertion-ordered.
    skills_considered: Vec<String>,
    /// QA-emitted passed test count, when available.
    tests_passed: Option<u64>,
    /// QA-emitted failed test count, when available.
    tests_failed: Option<u64>,
}

impl PerfCollector {
    /// Construct a new collector.
    ///
    /// Why: Called at workflow start so the wall-clock anchor is the true
    /// run start, not a lazy "first phase" moment.
    /// What: Captures the build number, workflow name, truncated task preview,
    /// start `Instant`, and start ISO8601 UTC timestamp.
    /// Test: `collector_records_phases`.
    pub fn new(build: u64, workflow: &str, task: &str) -> Self {
        Self {
            build,
            workflow: workflow.to_string(),
            task_preview: truncate_preview(task, 120),
            started_at: Instant::now(),
            started_at_iso: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            phases: Vec::new(),
            model: String::new(),
            status: "success".to_string(),
            failed_phase: None,
            skills_used: Vec::new(),
            skills_considered: Vec::new(),
            tests_passed: None,
            tests_failed: None,
        }
    }

    /// Record QA test counts (Fix 3 / parity with claude-mpm).
    ///
    /// Why: When the QA agent emits a parsable JSON envelope, we want the
    /// exact `passed`/`failed` counts on the perf record so dashboards can
    /// chart them over time without re-parsing agent output.
    /// What: Stores the two counts; included in the flushed JSON and the
    /// `runs.log` summary line.
    /// Test: `test_run_record_serializes_test_counts`.
    pub fn set_test_counts(&mut self, passed: u64, failed: u64) {
        self.tests_passed = Some(passed);
        self.tests_failed = Some(failed);
    }

    /// Record that `name` was surfaced by pre-plan skill discovery (#173).
    ///
    /// Why: Discovery considers a broader set of skills than what is actually
    /// injected. Tracking the full candidate set separately preserves the
    /// recall side of the recall/precision picture for post-run analysis.
    /// What: Appends `name` to `skills_considered` if not already present
    /// (insertion-ordered de-dup).
    /// Test: `collector_records_skills_considered`.
    pub fn record_skill_considered(&mut self, name: &str) {
        if !self.skills_considered.iter().any(|s| s == name) {
            self.skills_considered.push(name.to_string());
        }
    }

    /// Record that `name` was injected into a phase prompt during this run
    /// (#171).
    ///
    /// Why: Post-run persistence (`update_skill_usage` in `main.rs`) needs the
    /// list of injected skills to update `use_count` and `last_used`. Tracking
    /// here keeps the collector the single source of truth for the run.
    /// What: Appends `name` to `skills_used` if not already present
    /// (insertion-ordered de-dup).
    /// Test: Indirect via `update_skill_usage` integration in main.
    pub fn record_skill_used(&mut self, name: &str) {
        if !self.skills_used.iter().any(|s| s == name) {
            self.skills_used.push(name.to_string());
        }
    }

    /// Set the run status (#56).
    ///
    /// Why: The workflow engine calls this after the phase loop to record
    /// "success" on clean completion and "partial" / "failed" when a phase
    /// returned an error, so the flushed JSON reflects reality.
    /// What: Stores the status string verbatim; `flush` serializes it.
    /// Test: `collector_flush_records_failed_status`.
    pub fn set_status(&mut self, status: &str) {
        self.status = status.to_string();
    }

    /// Record which phase failed (#56).
    ///
    /// Why: Paired with `set_status`, this lets analysis tooling group failed
    /// runs by failing phase without having to parse the last `phases` entry.
    /// What: Stores the phase name; included in the flushed JSON when set.
    /// Test: `collector_flush_records_failed_status`.
    pub fn set_failed_phase(&mut self, phase: &str) {
        self.failed_phase = Some(phase.to_string());
    }

    /// Record the dominant model used by the run for pricing.
    ///
    /// Why: The LLM pricing table is keyed by model substring. The workflow
    /// engine doesn't know the model up front (each phase's agent may use a
    /// different one), so we let the caller set/override as needed.
    /// What: Stores the latest model string; pricing is looked up per
    /// `record_phase` call using the `model` argument passed there, not this
    /// field (this is retained for future aggregate analysis).
    /// Test: implicit via `collector_flush_writes_json_and_log`.
    #[allow(dead_code)]
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// Append a phase's measurements, computing its USD cost from `model`.
    ///
    /// Why: Engines call this once per phase with the agent model and the
    /// summed `TokenUsage` from all LLM turns in that phase.
    /// What: Computes `cost_usd` via `cost_usd(model, ...)`, pushes a
    /// `PhaseRecord`.
    /// Test: `collector_records_phases`.
    pub fn record_phase(&mut self, name: &str, duration_ms: u64, model: &str, usage: &TokenUsage) {
        let cost = cost_usd(
            model,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
        );
        self.phases.push(PhaseRecord {
            name: name.to_string(),
            duration_ms,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cost_usd: cost,
        });
    }

    /// Sum all phase records into a single `PerfTotals`.
    fn totals(&self) -> PerfTotals {
        let mut t = PerfTotals::default();
        for p in &self.phases {
            t.prompt_tokens = t.prompt_tokens.saturating_add(p.prompt_tokens);
            t.completion_tokens = t.completion_tokens.saturating_add(p.completion_tokens);
            t.cache_read_tokens = t.cache_read_tokens.saturating_add(p.cache_read_tokens);
            t.cache_creation_tokens = t
                .cache_creation_tokens
                .saturating_add(p.cache_creation_tokens);
            t.cost_usd += p.cost_usd;
        }
        t
    }

    /// Build the final `PerfRecord` without writing to disk.
    ///
    /// Why: Exposed for tests and for in-process consumers (the #151 JSON
    /// envelope projects this record into a `PmResponse`). `flush` calls
    /// this internally.
    pub fn build_record(&self) -> PerfRecord {
        let total_duration_ms = self.started_at.elapsed().as_millis() as u64;
        PerfRecord {
            build: self.build,
            version: env!("CARGO_PKG_VERSION").to_string(),
            workflow: self.workflow.clone(),
            task_preview: self.task_preview.clone(),
            started_at: self.started_at_iso.clone(),
            total_duration_ms,
            phases: self.phases.clone(),
            totals: self.totals(),
            status: self.status.clone(),
            failed_phase: self.failed_phase.clone(),
            skills_used: self.skills_used.clone(),
            skills_considered: self.skills_considered.clone(),
            tests_passed: self.tests_passed,
            tests_failed: self.tests_failed,
        }
    }

    /// Persist the record to `out_dir/runs/<stamp>.json` and append a
    /// summary line to `out_dir/runs.log`.
    ///
    /// Why: A single JSON per run is easy to diff and feed to tooling; the
    /// log line is a human-readable one-liner suitable for `tail -f`.
    /// What: Computes total duration, serializes to pretty JSON, writes the
    /// file atomically-ish (direct write is fine for append-only telemetry),
    /// then appends a one-line summary to `runs.log`.
    /// Test: `collector_flush_writes_json_and_log`.
    pub async fn flush(&self, out_dir: &Path) -> Result<()> {
        let record = self.build_record();
        let runs_dir = out_dir.join("runs");
        tokio::fs::create_dir_all(&runs_dir)
            .await
            .with_context(|| format!("failed to create {}", runs_dir.display()))?;

        // Stamp filename: YYYYMMDD-HHMMSS-build<N>.json
        // Derived from `started_at_iso` (UTC) so filenames are deterministic
        // given the same start time — useful for test fixtures.
        let stamp = filename_stamp(&self.started_at_iso, self.build);
        let json_path = runs_dir.join(format!("{stamp}.json"));
        let pretty = serde_json::to_vec_pretty(&record).context("serialize PerfRecord")?;
        tokio::fs::write(&json_path, &pretty)
            .await
            .with_context(|| format!("failed to write {}", json_path.display()))?;

        // One-line summary: tab-separated key=value pairs.
        // `tests_passed` / `tests_failed` are appended as the LAST two columns
        // for back-compat with log readers that only parse the first N
        // columns. Render `None` as "-" so the column is always present.
        let fmt_opt = |n: Option<u64>| -> String {
            n.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
        };
        let line = format!(
            "{ts}\tbuild={b}\tworkflow={w}\tdur_ms={d}\tprompt={p}\tcompletion={c}\tcache_r={cr}\tcache_w={cc}\tcost_usd={cost:.6}\ttests_passed={tp}\ttests_failed={tf}\n",
            ts = self.started_at_iso,
            b = self.build,
            w = self.workflow,
            d = record.total_duration_ms,
            p = record.totals.prompt_tokens,
            c = record.totals.completion_tokens,
            cr = record.totals.cache_read_tokens,
            cc = record.totals.cache_creation_tokens,
            cost = record.totals.cost_usd,
            tp = fmt_opt(record.tests_passed),
            tf = fmt_opt(record.tests_failed),
        );
        let log_path = out_dir.join("runs.log");
        // Ensure parent exists (out_dir itself might not yet).
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        // Append.
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
            .with_context(|| format!("failed to open {}", log_path.display()))?;
        f.write_all(line.as_bytes())
            .await
            .context("failed to append perf log line")?;
        f.flush().await.ok();

        tracing::info!(
            path = %json_path.display(),
            build = self.build,
            total_ms = record.total_duration_ms,
            total_cost_usd = record.totals.cost_usd,
            "wrote perf record"
        );
        Ok(())
    }
}

/// Compute USD cost for a single LLM call given the model name and token
/// counts. Unknown models fall back to Sonnet-class pricing.
///
/// Why: (#47) We hard-code the pricing table rather than hit a live endpoint
/// so offline/CI runs still produce comparable cost figures. Pricing is
/// per-million-tokens as published by Anthropic and OpenRouter.
/// What: Substring-matches the model string (e.g. "anthropic/claude-sonnet-4-5"
/// or "claude-haiku-4") and multiplies each token bucket by its rate.
/// Test: `cost_usd_known_model`, `cost_usd_unknown_defaults_to_sonnet`.
pub fn cost_usd(
    model: &str,
    prompt_tokens: u32,
    completion_tokens: u32,
    cache_read: u32,
    cache_creation: u32,
) -> f64 {
    // Rates in USD per token (not per million).
    let (rate_in, rate_out, rate_cache_r, rate_cache_w) = pricing_for(model);
    let to_usd = |tokens: u32, rate: f64| tokens as f64 * rate;
    to_usd(prompt_tokens, rate_in)
        + to_usd(completion_tokens, rate_out)
        + to_usd(cache_read, rate_cache_r)
        + to_usd(cache_creation, rate_cache_w)
}

/// Returns (input, output, cache_read, cache_creation) rates per token.
fn pricing_for(model: &str) -> (f64, f64, f64, f64) {
    let m = model.to_ascii_lowercase();
    // Claude Sonnet 4.x — $3 in, $15 out, $0.30 cache read, $3.75 cache write
    if m.contains("sonnet-4") || m.contains("claude-sonnet-4") {
        return (
            per_million(3.0),
            per_million(15.0),
            per_million(0.30),
            per_million(3.75),
        );
    }
    // Claude Haiku 3/4 — $0.80 in, $4 out, $0.08 cache read, $1 cache write
    if m.contains("haiku-3") || m.contains("haiku-4") || m.contains("claude-haiku") {
        return (
            per_million(0.80),
            per_million(4.0),
            per_million(0.08),
            per_million(1.0),
        );
    }
    // Claude Opus 4 — $15 in, $75 out (cache rates not published here, use
    // conservative 10% / 125% of input, matching Anthropic convention).
    if m.contains("opus-4") || m.contains("claude-opus") {
        return (
            per_million(15.0),
            per_million(75.0),
            per_million(1.50),
            per_million(18.75),
        );
    }
    // Default: Sonnet-class rates.
    (
        per_million(3.0),
        per_million(15.0),
        per_million(0.30),
        per_million(3.75),
    )
}

fn per_million(usd: f64) -> f64 {
    usd / 1_000_000.0
}

/// Build the canonical filename stamp from an ISO8601 timestamp + build #.
///
/// Why: Deterministic filenames let tests assert exact paths and let humans
/// sort runs chronologically with `ls`.
/// What: Converts `2026-04-22T17:31:30Z` + build=42 to `20260422-173130-build42`.
/// Test: `filename_stamp_format`.
fn filename_stamp(iso: &str, build: u64) -> String {
    // Strip non-digit chars to keep only YYYYMMDDHHMMSS, then reinsert the dash.
    let digits: String = iso.chars().filter(|c| c.is_ascii_digit()).collect();
    // Expected layout: YYYYMMDDHHMMSS (14 digits). If shorter, just pad.
    let date = digits.get(0..8).unwrap_or("00000000");
    let time = digits.get(8..14).unwrap_or("000000");
    format!("{date}-{time}-build{build}")
}

fn truncate_preview(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(max_chars + 3);
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= max_chars {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_default_is_zeros() {
        let u = TokenUsage::default();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.cache_read_tokens, 0);
        assert_eq!(u.cache_creation_tokens, 0);
    }

    #[test]
    fn token_usage_accumulates() {
        let mut a = TokenUsage::new(10, 5, 2, 1);
        a.add(&TokenUsage::new(3, 7, 0, 4));
        assert_eq!(a, TokenUsage::new(13, 12, 2, 5));
    }

    #[test]
    fn cost_usd_known_sonnet() {
        // 1M prompt tokens of sonnet = $3.00
        let c = cost_usd("anthropic/claude-sonnet-4-5", 1_000_000, 0, 0, 0);
        assert!((c - 3.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn cost_usd_known_haiku() {
        // 1M output tokens of haiku = $4.00
        let c = cost_usd("anthropic/claude-haiku-4", 0, 1_000_000, 0, 0);
        assert!((c - 4.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn cost_usd_cache_read_is_cheaper() {
        // Cache-read is 10x cheaper than fresh input on Sonnet.
        let fresh = cost_usd("claude-sonnet-4-6", 1_000_000, 0, 0, 0);
        let cached = cost_usd("claude-sonnet-4-6", 0, 0, 1_000_000, 0);
        assert!(cached < fresh);
        assert!((cached - 0.30).abs() < 1e-9);
    }

    // --- #29: Cost calculation tests for cache hits ---

    #[test]
    fn cost_usd_sonnet_cache_creation_matches_spec() {
        // #29: Cache write rate for claude-sonnet-4-6 is $3.75/MTok.
        // 1M cache_creation tokens should cost exactly $3.75.
        let c = cost_usd("anthropic/claude-sonnet-4-6", 0, 0, 0, 1_000_000);
        assert!(
            (c - 3.75).abs() < 1e-9,
            "expected $3.75 for 1M cache write tokens on sonnet-4-6, got {c}"
        );
    }

    #[test]
    fn cost_usd_sonnet_cache_read_is_one_tenth_of_input() {
        // #29: Cache read is $0.30/MTok vs input $3.00/MTok — exactly 10%.
        // This is the headline savings figure for prompt caching.
        let fresh_input = cost_usd("claude-sonnet-4-6", 1_000_000, 0, 0, 0);
        let cache_read = cost_usd("claude-sonnet-4-6", 0, 0, 1_000_000, 0);
        let ratio = cache_read / fresh_input;
        assert!(
            (ratio - 0.10).abs() < 1e-9,
            "cache_read should be 10% of input cost, got {ratio} (fresh={fresh_input}, cached={cache_read})"
        );
    }

    #[test]
    fn cost_usd_sonnet_mixed_cache_hit_scenario() {
        // #29: Realistic scenario — a turn where most input is a cache hit:
        // 100 fresh prompt tokens + 50 completion + 9000 cache_read + 1000 cache_creation.
        // Sonnet rates: $3/MTok in, $15/MTok out, $0.30/MTok cache_r, $3.75/MTok cache_w.
        let c = cost_usd("anthropic/claude-sonnet-4-6", 100, 50, 9_000, 1_000);
        // 100 * 3e-6 = 0.0003, 50 * 15e-6 = 0.00075, 9000 * 0.30e-6 = 0.0027,
        // 1000 * 3.75e-6 = 0.00375. Total = 0.00750.
        let expected = 0.0003 + 0.00075 + 0.0027 + 0.00375;
        assert!((c - expected).abs() < 1e-9, "expected {expected}, got {c}");
    }

    #[test]
    fn cost_usd_unknown_defaults_to_sonnet() {
        let u = cost_usd("some/unknown-model", 1_000_000, 0, 0, 0);
        assert!((u - 3.0).abs() < 1e-9);
    }

    #[test]
    fn collector_records_phases() {
        let mut c = PerfCollector::new(7, "prescriptive", "write x");
        c.record_phase(
            "research",
            500,
            "claude-sonnet-4-5",
            &TokenUsage::new(1000, 500, 0, 0),
        );
        c.record_phase(
            "code",
            1200,
            "claude-sonnet-4-5",
            &TokenUsage::new(2000, 1000, 500, 200),
        );
        let r = c.build_record();
        assert_eq!(r.phases.len(), 2);
        assert_eq!(r.phases[0].prompt_tokens, 1000);
        assert_eq!(r.totals.prompt_tokens, 3000);
        assert_eq!(r.totals.completion_tokens, 1500);
        assert_eq!(r.totals.cache_read_tokens, 500);
        assert_eq!(r.totals.cache_creation_tokens, 200);
        // Sanity: some cost accrued.
        assert!(r.totals.cost_usd > 0.0);
        assert_eq!(r.build, 7);
        assert_eq!(r.workflow, "prescriptive");
    }

    #[test]
    fn truncate_preview_respects_limit() {
        let s = "x".repeat(200);
        let p = truncate_preview(&s, 120);
        assert_eq!(p.chars().count(), 123, "120 chars + ellipsis");
        assert!(p.ends_with("..."));
    }

    #[test]
    fn truncate_preview_short_string_unchanged() {
        let p = truncate_preview("hi", 120);
        assert_eq!(p, "hi");
    }

    #[test]
    fn filename_stamp_format() {
        let s = filename_stamp("2026-04-22T17:31:30Z", 42);
        assert_eq!(s, "20260422-173130-build42");
    }

    #[tokio::test]
    async fn collector_flush_records_failed_status() {
        // #56: When a workflow phase fails, the engine sets status=partial
        // and records the failing phase, then flushes. Verify the JSON round-
        // trips those fields.
        let tmp = tempfile::tempdir().unwrap();
        let mut c = PerfCollector::new(11, "test-wf", "broken task");
        c.record_phase(
            "research",
            50,
            "claude-sonnet-4-6",
            &TokenUsage::new(100, 50, 0, 0),
        );
        c.set_status("partial");
        c.set_failed_phase("plan");
        c.flush(tmp.path()).await.unwrap();

        let runs = tmp.path().join("runs");
        let mut entries = tokio::fs::read_dir(&runs).await.unwrap();
        let mut rec: Option<PerfRecord> = None;
        while let Some(e) = entries.next_entry().await.unwrap() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("json") {
                let bytes = tokio::fs::read(e.path()).await.unwrap();
                rec = Some(serde_json::from_slice(&bytes).unwrap());
            }
        }
        let rec = rec.expect("perf json written");
        assert_eq!(rec.status, "partial");
        assert_eq!(rec.failed_phase.as_deref(), Some("plan"));
    }

    #[test]
    fn collector_default_status_is_success() {
        // #56: New collectors default to status=success so clean runs don't
        // need to explicitly call set_status.
        let c = PerfCollector::new(1, "wf", "task");
        let r = c.build_record();
        assert_eq!(r.status, "success");
        assert!(r.failed_phase.is_none());
    }

    /// Why: Fix 3 / claude-mpm parity — when the QA agent emits structured
    /// pass/fail counts, those counts must round-trip through the perf
    /// record's JSON form AND appear in the `runs.log` summary line.
    /// What: Constructs a `PerfCollector`, sets test counts, flushes, and
    /// asserts both the JSON file and the log line carry "42" and "0".
    /// Test: this function (`test_run_record_serializes_test_counts`).
    #[tokio::test]
    async fn test_run_record_serializes_test_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = PerfCollector::new(99, "test-wf", "task with tests");
        c.record_phase("qa", 10, "claude-sonnet-4-6", &TokenUsage::new(10, 5, 0, 0));
        c.set_test_counts(42, 0);
        c.flush(tmp.path()).await.unwrap();

        // JSON round-trip retains the counts.
        let runs = tmp.path().join("runs");
        let mut entries = tokio::fs::read_dir(&runs).await.unwrap();
        let mut rec: Option<PerfRecord> = None;
        while let Some(e) = entries.next_entry().await.unwrap() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("json") {
                let bytes = tokio::fs::read(e.path()).await.unwrap();
                rec = Some(serde_json::from_slice(&bytes).unwrap());
            }
        }
        let rec = rec.expect("perf json written");
        assert_eq!(rec.tests_passed, Some(42));
        assert_eq!(rec.tests_failed, Some(0));

        // Raw JSON bytes contain the literal "42" and "0" values.
        let json_str = serde_json::to_string(&rec).unwrap();
        assert!(json_str.contains("\"tests_passed\":42"));
        assert!(json_str.contains("\"tests_failed\":0"));

        // runs.log line ends with the new columns.
        let log = tokio::fs::read_to_string(tmp.path().join("runs.log"))
            .await
            .unwrap();
        assert!(
            log.contains("tests_passed=42"),
            "runs.log missing tests_passed=42: {log}"
        );
        assert!(
            log.contains("tests_failed=0"),
            "runs.log missing tests_failed=0: {log}"
        );
    }

    /// Why: Back-compat — when QA didn't run or emitted no JSON envelope,
    /// the counts must be `None`, omitted from JSON via `skip_serializing_if`,
    /// and rendered as `-` in `runs.log`.
    #[tokio::test]
    async fn run_record_omits_test_counts_when_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = PerfCollector::new(100, "test-wf", "no qa");
        c.record_phase(
            "research",
            5,
            "claude-sonnet-4-6",
            &TokenUsage::new(10, 5, 0, 0),
        );
        c.flush(tmp.path()).await.unwrap();

        let log = tokio::fs::read_to_string(tmp.path().join("runs.log"))
            .await
            .unwrap();
        assert!(
            log.contains("tests_passed=-"),
            "expected '-' placeholder: {log}"
        );
        assert!(
            log.contains("tests_failed=-"),
            "expected '-' placeholder: {log}"
        );
    }

    #[tokio::test]
    async fn collector_flush_writes_json_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = PerfCollector::new(5, "test-wf", "hello task");
        c.record_phase(
            "plan",
            100,
            "claude-sonnet-4-5",
            &TokenUsage::new(100, 50, 0, 0),
        );
        c.flush(tmp.path()).await.unwrap();

        // A runs/ dir was created with at least one .json inside.
        let runs = tmp.path().join("runs");
        assert!(runs.exists());
        let mut entries = tokio::fs::read_dir(&runs).await.unwrap();
        let mut found_json = false;
        while let Some(e) = entries.next_entry().await.unwrap() {
            if e.path().extension().and_then(|s| s.to_str()) == Some("json") {
                found_json = true;
                let bytes = tokio::fs::read(e.path()).await.unwrap();
                let rec: PerfRecord = serde_json::from_slice::<PerfRecord>(&bytes).unwrap();
                assert_eq!(rec.build, 5);
                assert_eq!(rec.workflow, "test-wf");
                assert_eq!(rec.phases.len(), 1);
                assert_eq!(rec.phases[0].name, "plan");
            }
        }
        assert!(found_json, "expected at least one .json run file");

        // runs.log exists and contains the build tag.
        let log = tokio::fs::read_to_string(tmp.path().join("runs.log"))
            .await
            .unwrap();
        assert!(log.contains("build=5"));
        assert!(log.contains("workflow=test-wf"));
    }
}
