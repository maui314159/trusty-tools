//! `ClaudeCodeAgentRunner` — spawn the `claude` CLI as an agent (#60).
//!
//! Why: Claude Max subscribers already pay for unlimited Claude usage through
//! their claude.ai OAuth session. Routing open-mpm agents through the
//! locally-installed `claude` CLI in headless mode lets them run under that
//! subscription with no API key — no `OPENROUTER_API_KEY`, no
//! `ANTHROPIC_API_KEY`. Any agent can opt in by setting
//! `[agent].runner = "claude-code"` in its TOML.
//! What: `ClaudeCodeAgentRunner` implements the `AgentRunner` trait by
//! spawning `claude -p <task> --output-format stream-json ...` and reading
//! NDJSON events from stdout until it sees the final
//! `{"type":"result","result":"...","is_error":false}` line. That event maps
//! directly to open-mpm's `AgentOutput` (content + summary). Model aliases
//! are normalized (`anthropic/claude-sonnet-4-6` → `claude-sonnet-4-6`).
//! Test: `normalize_model_strips_anthropic_prefix`,
//! `normalize_model_openai_untouched`, and `run_parses_stream_json_result`
//! (the last uses a mock shell script standing in for `claude`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::agents::AgentConfig;
use crate::agents::agent_model_env;
use crate::agents::harness_protocol::{BASE_PROTOCOL, CLAUDE_CODE_PROTOCOL, FINISH_TASK_PROTOCOL};
use crate::agents::prompt_builder::LAYER_SEPARATOR;
use crate::ipc::extract_summary;
use crate::perf::TokenUsage;
use crate::tools::traits::{AgentOutput, AgentRunner, RunContext};

/// Runs a task by spawning the `claude` CLI.
///
/// Why: The CLI already handles OAuth/session auth and the Claude Max plan,
/// so we delegate the entire LLM round-trip to it and just parse its
/// stream-json output back into our internal shape.
/// What: Holds the absolute path to the `claude` binary (resolved once at
/// construction) plus a flag for skipping the auth-status check (for tests).
/// Test: See module tests.
pub struct ClaudeCodeAgentRunner {
    pub(crate) claude_bin: PathBuf,
}

impl ClaudeCodeAgentRunner {
    /// Locate `claude` in `PATH` and return a ready-to-use runner.
    ///
    /// Why: Failing fast at construction time gives callers (the
    /// `WorkflowEngine` startup path) a clear error before any agent tries to
    /// execute.
    /// What: Searches each entry in `PATH` for an executable `claude` file.
    /// Returns `Err` with a user-facing message if nothing is found.
    /// Test: Covered indirectly — the happy-path test injects a fake binary
    /// via the pub(crate) field to avoid depending on the host's `claude`.
    pub async fn new() -> Result<Self> {
        let claude_bin = find_claude()?;
        Ok(Self { claude_bin })
    }

    /// Strip the `anthropic/` OpenRouter prefix so the `claude` CLI sees a
    /// model name it recognizes.
    ///
    /// Why: open-mpm agent TOMLs use OpenRouter-style model strings like
    /// `anthropic/claude-sonnet-4-6`; the `claude` CLI expects either a
    /// short alias (`sonnet`) or a bare model name (`claude-sonnet-4-6`).
    /// What: `anthropic/<name>` → `<name>`; other prefixes pass through
    /// unchanged.
    /// Test: `normalize_model_strips_anthropic_prefix`,
    /// `normalize_model_openai_untouched`.
    pub fn normalize_model(model: &str) -> String {
        model.trim_start_matches("anthropic/").to_string()
    }

    /// Strip `finish_task`-related instructions from a prompt before passing
    /// it to the `claude` CLI (#113).
    ///
    /// Why: Agent TOMLs with `use_finish_task = true` include instructions
    /// telling the agent to call `finish_task` to signal completion. That
    /// tool is auto-registered only on the subprocess/IPC path; the `claude`
    /// CLI knows nothing about it. When those instructions leak into a
    /// claude-code agent's system prompt, the model dutifully waits for a
    /// tool that never exists, burns through every turn trying to invent it,
    /// and the CLI eventually terminates with `max_turns`. Stripping the
    /// instructions at the runner boundary makes claude-code agents behave
    /// like any normal Claude chat: they just stop producing text when done.
    /// What: Scans the input line-by-line. Drops any line whose
    /// case-insensitive content mentions `finish_task`. Also removes simple
    /// numbered-list items whose surviving body becomes empty. Keeps the
    /// surrounding structure intact so prose around the instruction is
    /// unaffected.
    /// Test: `strip_finish_task_instructions_removes_callouts`,
    /// `strip_finish_task_instructions_is_idempotent`,
    /// `strip_finish_task_instructions_preserves_unrelated_text`.
    /// Prepend harness-protocol layers to the agent's base system prompt.
    ///
    /// Why: The `claude` CLI path doesn't go through `SystemPromptBuilder`
    /// (it passes `cfg.system_prompt.content` directly via `--system-prompt`),
    /// so we need to apply the same harness-layer injection the subprocess
    /// path gets from `SystemPromptBuilder::add_harness_layer`. Keeping the
    /// loading logic in one helper means we only mirror the selection rules
    /// (base always, claude-code when not use_finish_task, finish-task when
    /// use_finish_task) in one place.
    /// What: Sources harness content from compiled-in constants
    /// (`BASE_PROTOCOL` always, `CLAUDE_CODE_PROTOCOL` when
    /// `use_finish_task == false` since this runner ALWAYS targets
    /// claude-code, `FINISH_TASK_PROTOCOL` when `use_finish_task == true`),
    /// then concatenates them above `base_prompt` using `LAYER_SEPARATOR`.
    /// Test: `prepend_harness_layers_adds_base_and_claude_code`.
    fn prepend_harness_layers(base_prompt: &str, use_finish_task: bool) -> String {
        let mut layers: Vec<&str> = Vec::new();
        if !BASE_PROTOCOL.trim().is_empty() {
            layers.push(BASE_PROTOCOL);
        }
        if !use_finish_task && !CLAUDE_CODE_PROTOCOL.trim().is_empty() {
            layers.push(CLAUDE_CODE_PROTOCOL);
        }
        if use_finish_task && !FINISH_TASK_PROTOCOL.trim().is_empty() {
            layers.push(FINISH_TASK_PROTOCOL);
        }
        if layers.is_empty() {
            return base_prompt.to_string();
        }
        let mut out = String::new();
        for layer in &layers {
            out.push_str(layer.trim_end());
            out.push_str(LAYER_SEPARATOR);
        }
        out.push_str(base_prompt);
        out
    }

    pub fn strip_finish_task_instructions(prompt: &str) -> String {
        let mut out = String::with_capacity(prompt.len());
        for line in prompt.split_inclusive('\n') {
            // Preserve the trailing newline if present.
            let (body, nl) = match line.strip_suffix('\n') {
                Some(b) => (b, "\n"),
                None => (line, ""),
            };
            if body.to_ascii_lowercase().contains("finish_task") {
                // Drop the whole line; for standalone bullets this also
                // removes the enumeration marker so no orphan "5." remains.
                continue;
            }
            out.push_str(body);
            out.push_str(nl);
        }
        out
    }

    /// Verify that the resolved `claude` CLI is authenticated.
    ///
    /// Why: (#60) If the user has not run `claude auth login`, every spawn
    /// will fail with a confusing error buried in stderr. A one-shot check
    /// at engine startup surfaces the problem with an actionable message.
    /// What: Runs `claude auth status` with stdio suppressed and inspects
    /// the exit code. Exit 0 = authenticated.
    /// Test: Covered at the wiring level; we don't exercise the real CLI in
    /// unit tests since that would require a real OAuth session.
    pub async fn check_auth(&self) -> Result<()> {
        let status = Command::new(&self.claude_bin)
            .args(["auth", "status"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("failed to run `claude auth status`")?;
        if !status.success() {
            anyhow::bail!(
                "claude CLI is not authenticated — run `claude auth login` to connect your Claude Max account"
            );
        }
        Ok(())
    }

    /// Internal: spawn the claude CLI with fully-resolved config and parse
    /// its stream-json output back into `AgentOutput`.
    ///
    /// Why: Extracted so both `run` and `run_with_history` can call it; also
    /// makes the I/O loop easier to test with a mock script.
    /// What: Builds the argv from `cfg`, spawns the process, reads NDJSON
    /// lines until it sees `{"type":"result"}`, returns the parsed content.
    /// Test: `run_parses_stream_json_result`.
    async fn run_with_config(&self, cfg: &AgentConfig, task: &str) -> Result<AgentOutput> {
        self.run_with_config_ctx(cfg, task, &RunContext::default())
            .await
    }

    /// Public re-export of `run_with_config` for #250 ctrl OAuth routing.
    ///
    /// Why: The ctrl orchestrator needs to invoke the claude CLI for its own
    /// LLM call when only `CLAUDE_CODE_OAUTH_TOKEN` is configured. The
    /// existing `run_with_config` is crate-private to keep the runner's
    /// internal trait surface tight, but the ctrl path is also crate-internal
    /// so a thin pub wrapper costs nothing and avoids leaking `RunContext`
    /// into the call site.
    /// What: Forwards to `run_with_config_ctx` with a default context.
    /// Test: Exercised via `ctrl::run_pm_task_via_claude_cli`.
    pub async fn run_with_config_public(
        &self,
        cfg: &AgentConfig,
        task: &str,
    ) -> Result<AgentOutput> {
        self.run_with_config(cfg, task).await
    }

    /// Like `run_with_config` but honors a `RunContext`.
    ///
    /// Why: CRIT-1 / MAJ-1 — previously the parent process mutated
    /// `OPEN_MPM_MAX_TURNS` (unsafe under multi-threaded tokio) and there
    /// was no way to set the subprocess CWD, so the wave loop had to copy
    /// files back out of the parent's cwd. Threading a `RunContext` lets
    /// the runner scope these to the child only and use `out_dir` as CWD.
    /// What: `ctx.working_dir` → `Command::current_dir`; `ctx.max_turns_override`
    /// overrides `cfg.llm.max_turns` (passed as `--max-turns` CLI arg);
    /// `ctx.assigned_file` is surfaced via `OPEN_MPM_ASSIGNED_FILE` on the
    /// child env only.
    /// Test: Exercised end-to-end by the wave-loop tests.
    async fn run_with_config_ctx(
        &self,
        cfg: &AgentConfig,
        task: &str,
        ctx: &RunContext,
    ) -> Result<AgentOutput> {
        // #107: `ctx.model` (from the workflow phase's `model` field) takes
        // precedence over the TOML-resolved model so per-phase overrides
        // actually reach the `claude` CLI. An agent-specific
        // `OPEN_MPM_MODEL_<AGENT>` env var still wins overall because
        // `AgentConfig::load` bakes that into `cfg.agent.model` before we
        // even see `ctx`, preserving the documented priority chain
        // (env > phase override > agent TOML > default).
        let effective_model = match ctx.model.as_deref() {
            Some(m) if !m.is_empty() && agent_model_env(&cfg.agent.name).is_none() => m,
            _ => cfg.agent.model.as_str(),
        };
        let model = Self::normalize_model(effective_model);
        let max_turns_value = ctx.max_turns_override.unwrap_or(cfg.llm.max_turns);
        let max_turns = max_turns_value.to_string();

        tracing::info!(
            agent = %cfg.agent.name,
            model = %model,
            claude_bin = %self.claude_bin.display(),
            max_turns = max_turns_value,
            working_dir = ?ctx.working_dir,
            "spawning claude CLI"
        );

        // #325: The `claude` CLI subprocess does not accept `stop_sequences`
        // via its IPC, so any TOML-declared values are silently dropped on
        // this runner. Warn once per dispatch so operators discover the
        // mismatch instead of wondering why their stop strings don't fire.
        if !cfg.llm.stop_sequences.is_empty() {
            tracing::warn!(
                agent = %cfg.agent.name,
                stop_sequences = ?cfg.llm.stop_sequences,
                "stop_sequences set but not forwarded to claude-code runner — \
                 the claude CLI subprocess does not support this parameter via IPC; \
                 sequences will be ignored"
            );
        }

        // #281: Capture wall-clock start so we can record dispatch duration in
        // the usage log even when the CLI omits a `usage` block (older
        // versions). Read after the CLI exits below.
        let dispatch_started = std::time::Instant::now();

        // #199: signal that the claude-code agent's work loop is starting.
        // Distinct from `AgentSpawned` (which fires when delegation is decided)
        // — this fires at the actual CLI invocation.
        crate::events::publish(crate::events::Event::AgentStarted {
            session_id: std::env::var("OPEN_MPM_RUN_ID").unwrap_or_default(),
            agent_name: cfg.agent.name.clone(),
            runner_type: "claude-code".to_string(),
        });

        // #113: The `claude` CLI does not know about open-mpm's `finish_task`
        // tool, so any TOML / task text that tells the agent to "call
        // finish_task" makes it spin until max_turns. Strip those lines here
        // — proper fix corresponding to option 4 in the bug report.
        let sanitized_task = Self::strip_finish_task_instructions(task);
        // Inject harness-protocol layers (base + claude-code-specific or
        // finish-task depending on cfg.llm.use_finish_task) BEFORE the TOML
        // base prompt, then strip any lingering `finish_task` mentions. The
        // claude-code CLI doesn't speak the finish_task tool, so if the
        // finish-task harness layer was injected it must also be stripped by
        // `strip_finish_task_instructions` — that's fine because the rule is
        // "claude-code agents never call finish_task", which is enforced via
        // the harness injection selector above (we still strip defensively).
        let composed_system =
            Self::prepend_harness_layers(&cfg.system_prompt.content, cfg.llm.use_finish_task);
        let sanitized_system = Self::strip_finish_task_instructions(&composed_system);

        let mut cmd = Command::new(&self.claude_bin);
        cmd.args([
            "-p",
            &sanitized_task,
            "--model",
            &model,
            "--system-prompt",
            &sanitized_system,
            "--output-format",
            "stream-json",
            "--verbose",
            "--dangerously-skip-permissions",
            "--max-turns",
            &max_turns,
        ])
        .stdout(Stdio::piped())
        // #268: Sub-agent `claude` CLI subprocesses must not bleed their own
        // tracing/log output to the parent's TTY. The parent REPL routes its
        // own tracing to `~/.open-mpm/logs/repl.log` when stdin is a TTY, but
        // sub-agent processes detect a non-TTY stdin and default to stderr —
        // which the parent inherits, so log lines clobber the carefully
        // positioned chat scrollback.
        // Detection: when the parent's stdin is a TTY, this is an interactive
        // REPL session and sub-agent stderr must be silenced. When the parent
        // is non-interactive (CI, piped input, --workflow, --api), inherit
        // stderr so existing log-capture tooling continues to work.
        .stderr(if crate::repl::is_tty() {
            Stdio::null()
        } else {
            Stdio::inherit()
        });

        if let Some(wd) = &ctx.working_dir {
            cmd.current_dir(wd);
            // #159: The `claude` CLI anchors write paths to the git root, not
            // the subprocess CWD. Passing `--add-dir` scopes the claude
            // session to the designated output directory so file writes land
            // in `out_dir` rather than the repo root.
            cmd.arg("--add-dir").arg(wd);
        }
        // Pass --allowedTools if the agent config restricts claude CLI tools.
        // Why: Research agents should not write files or run shell — only
        // WebSearch/WebFetch. Empty vec (default) means no restriction.
        if !cfg.llm.claude_allowed_tools.is_empty() {
            cmd.arg("--allowedTools")
                .arg(cfg.llm.claude_allowed_tools.join(","));
        }
        if let Some(path) = &ctx.assigned_file {
            cmd.env("OPEN_MPM_ASSIGNED_FILE", path);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn claude CLI at {}",
                self.claude_bin.display()
            )
        })?;

        // #283 follow-up: latency instrumentation. Record spawn-to-ready
        // wall-clock so we can distinguish "CLI took forever to start" from
        // "model itself was slow".
        let spawn_elapsed_ms = dispatch_started.elapsed().as_millis() as u64;
        tracing::debug!(spawn_ms = spawn_elapsed_ms, "claude CLI spawned");

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("claude CLI stdout was not captured"))?;

        let mut lines = BufReader::new(stdout).lines();
        // TTFT (time-to-first-token) is the elapsed from `dispatch_started`
        // to the first line emitted on stdout.
        let mut first_line = true;
        let mut final_result: Option<String> = None;
        let mut is_error = false;
        let mut subtype: Option<String> = None;
        // #232: Capture token usage from the terminal `result` event so the
        // perf collector can attribute prompt/completion/cache tokens to
        // claude-code phases instead of always recording zeros.
        let mut result_usage = TokenUsage::default();
        // #113: The `claude` CLI streams incremental assistant text events
        // BEFORE the terminal `result` event. When the CLI hits `max_turns`
        // the final `result` event is `{is_error:true, subtype:"error_max_turns"}`
        // with `result` sometimes empty — but the actual content the agent
        // produced lives in those intermediate `assistant` events. We
        // accumulate that text so we can recover usable output on max_turns.
        let mut assistant_buf = String::new();

        while let Some(line) = lines
            .next_line()
            .await
            .context("failed to read claude CLI stdout")?
        {
            if first_line {
                tracing::debug!(
                    ttft_ms = dispatch_started.elapsed().as_millis() as u64,
                    "time-to-first-token"
                );
                first_line = false;
            }
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                tracing::trace!(line = %line, "skipping non-JSON line from claude");
                continue;
            };

            match event.get("type").and_then(|v| v.as_str()) {
                Some("result") => {
                    is_error = event
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    subtype = event
                        .get("subtype")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    final_result = event
                        .get("result")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if let Some(u) = event.get("usage") {
                        result_usage = TokenUsage {
                            prompt_tokens: u
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32,
                            completion_tokens: u
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32,
                            cache_read_tokens: u
                                .get("cache_read_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32,
                            cache_creation_tokens: u
                                .get("cache_creation_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                                as u32,
                        };
                    }
                    break;
                }
                Some("assistant") => {
                    // Extract any text content blocks from the assistant
                    // message so we can salvage output if the CLI terminates
                    // abnormally (e.g. max_turns). Silently ignore shapes we
                    // don't recognize — these are advisory only.
                    if let Some(blocks) = event
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for block in blocks {
                            if block.get("type").and_then(|v| v.as_str()) == Some("text")
                                && let Some(text) = block.get("text").and_then(|v| v.as_str())
                            {
                                if !assistant_buf.is_empty() {
                                    assistant_buf.push('\n');
                                }
                                assistant_buf.push_str(text);
                            }
                        }
                    }
                }
                Some(other) => {
                    tracing::trace!(event_type = %other, "claude stream event");
                }
                None => {}
            }
        }

        // Reap the child; exit status is advisory (the stream-json `result`
        // event is the authoritative outcome).
        let _ = child.wait().await;

        // #281: Per-dispatch usage log. tokens come from the result event's
        // `usage` block when present (zero otherwise). duration is wall-clock
        // around the entire spawn → result-event loop.
        let dispatch_duration_ms = dispatch_started.elapsed().as_millis() as u64;

        // Publish LlmRequested + LlmResponded so the REPL's token relay (and
        // any other event-bus consumer) receives the same tokens-in /
        // tokens-out signal that OpenRouter / Anthropic-direct / Bedrock
        // produce. The HTTP-based paths in `src/llm/mod.rs` emit these via
        // `emit_llm_requested` / `emit_llm_responded`; the claude-code CLI
        // path didn't, so the input-bar live counter never moved when an
        // agent ran via the CLI. Fire them as a pair after parsing the
        // `result` stream-json event so both prompt and completion totals
        // are known.
        //
        // Why: Live token tracking must work for ALL LLM dispatch paths, not
        // just the REST ones.
        // What: Empty `agent_name` and zero latency are acceptable here —
        // the relay only needs the token counts. We use the agent's name
        // and the dispatch duration to keep the events useful for other
        // consumers too.
        {
            let session_id = std::env::var("OPEN_MPM_RUN_ID").unwrap_or_default();
            let agent_name = cfg.agent.name.clone();
            let model_name = model.clone();
            let prompt_tokens = result_usage.prompt_tokens;
            let completion_tokens = result_usage.completion_tokens;
            crate::events::publish(crate::events::Event::LlmRequested {
                session_id: session_id.clone(),
                agent_name: agent_name.clone(),
                model: model_name.clone(),
                prompt_tokens: if prompt_tokens > 0 {
                    Some(prompt_tokens)
                } else {
                    None
                },
            });
            crate::events::publish(crate::events::Event::LlmResponded {
                session_id,
                agent_name,
                model: model_name,
                completion_tokens: if completion_tokens > 0 {
                    Some(completion_tokens)
                } else {
                    None
                },
                latency_ms: dispatch_duration_ms,
            });
        }
        {
            let agent = cfg.agent.name.clone();
            let model_for_log = model.clone();
            let task_for_prefix = task.to_string();
            let input_tokens = result_usage.prompt_tokens;
            let output_tokens = result_usage.completion_tokens;
            let record = crate::usage::UsageRecord::new(
                agent,
                model_for_log,
                "claude-code",
                input_tokens,
                output_tokens,
                dispatch_duration_ms,
                &task_for_prefix,
            );
            let project_dir = crate::usage::project_dir();
            tokio::spawn(async move {
                crate::usage::append_usage(&project_dir, &record).await;
            });
        }

        // #113: Treat `error_max_turns` with recoverable content as SUCCESS.
        // When `finish_task` slips into a claude-code agent's prompt (or the
        // agent is simply verbose) the CLI can exhaust its turn budget even
        // though the work was completed. If we can recover content from the
        // result or the accumulated assistant stream, prefer that over
        // failing the whole phase. A warning log keeps the occurrence
        // visible so operators notice runaway turn counts.
        let max_turns_recoverable = is_error
            && subtype.as_deref() == Some("error_max_turns")
            && (final_result.as_ref().is_some_and(|s| !s.trim().is_empty())
                || !assistant_buf.trim().is_empty());

        if max_turns_recoverable {
            tracing::warn!(
                agent = %cfg.agent.name,
                max_turns = max_turns_value,
                result_chars = final_result.as_deref().map(str::len).unwrap_or(0),
                assistant_chars = assistant_buf.len(),
                "claude CLI hit max_turns but produced content; treating as success (#113)"
            );
            is_error = false;
            if final_result.as_ref().is_none_or(|s| s.trim().is_empty()) {
                final_result = Some(assistant_buf.clone());
            }
        }

        if is_error {
            let msg = final_result
                .unwrap_or_else(|| "claude CLI returned an error with no message".into());
            let subtype_tag = subtype.as_deref().unwrap_or("<none>");
            return Err(anyhow!("claude CLI error [subtype={subtype_tag}]: {msg}"));
        }

        let content = final_result
            .ok_or_else(|| anyhow!("claude CLI exited without producing a `result` event"))?;

        // Reuse the shared summary extractor so claude-code agents match
        // other runners in how downstream phases consume output.
        let summary_raw = extract_summary(&content);
        let summary = if summary_raw.is_empty() {
            None
        } else {
            Some(summary_raw)
        };

        // #199: emit `ReportGenerated` so the UI shows the agent's report
        // size before the engine moves on to the next phase.
        crate::events::publish(crate::events::Event::ReportGenerated {
            session_id: std::env::var("OPEN_MPM_RUN_ID").unwrap_or_default(),
            agent_name: cfg.agent.name.clone(),
            word_count: content.split_whitespace().count(),
            status: "success".to_string(),
        });

        Ok(AgentOutput {
            content,
            summary,
            // #232: Token usage is parsed from the `usage` field of the
            // terminal `{"type":"result"}` event in stream-json output. When
            // the field is absent (older CLI versions) this falls back to
            // zeros, matching the previous behavior.
            usage: result_usage,
        })
    }
}

#[async_trait]
impl AgentRunner for ClaudeCodeAgentRunner {
    async fn run(&self, agent_name: &str, task: &str) -> Result<AgentOutput> {
        // #96: Load the agent config via the tokio-aware loader so we don't
        // block the async runtime on disk I/O mid-dispatch.
        let cfg = AgentConfig::by_name_async(agent_name)
            .await
            .with_context(|| format!("failed to load agent config for '{agent_name}'"))?;
        self.run_with_config(&cfg, task).await
    }

    async fn run_with_context(
        &self,
        agent_name: &str,
        task: &str,
        ctx: &RunContext,
    ) -> Result<AgentOutput> {
        let cfg = AgentConfig::by_name_async(agent_name)
            .await
            .with_context(|| format!("failed to load agent config for '{agent_name}'"))?;
        self.run_with_config_ctx(&cfg, task, ctx).await
    }
}

/// Dispatching runner that selects a concrete `AgentRunner` per-call based
/// on the agent's TOML `runner` field (#60).
///
/// Why: The `WorkflowEngine` holds a single `Arc<dyn AgentRunner>` for the
/// whole run, but individual agents can now opt into either the subprocess
/// (API-key) path or the claude-code (OAuth) path. A thin dispatcher keeps
/// the engine untouched — it still sees one runner — while picking the
/// right underlying implementation per agent.
/// What: Holds an `Arc<dyn AgentRunner>` fallback (the normal subprocess
/// runner) plus an optional `Arc<ClaudeCodeAgentRunner>`. On each call it
/// loads the agent config by name; `runner = "claude-code"` routes to the
/// Claude runner, everything else falls through to the fallback.
/// Test: `dispatcher_routes_subprocess_by_default`,
/// `dispatcher_routes_claude_code`.
pub struct DispatchingAgentRunner {
    fallback: std::sync::Arc<dyn AgentRunner>,
    claude_code: Option<std::sync::Arc<ClaudeCodeAgentRunner>>,
    /// Optional in-process runner (#198 / Phase C). When `Some`, agents whose
    /// TOML declares `runner = "in-process"` are dispatched here instead of
    /// the subprocess fallback, eliminating the per-call startup overhead.
    in_process: Option<std::sync::Arc<dyn AgentRunner>>,
}

impl DispatchingAgentRunner {
    /// Build a dispatcher. Pass `None` for `claude_code` when no agent in
    /// the workflow opts into it — the dispatcher then behaves as a thin
    /// passthrough to `fallback`.
    pub fn new(
        fallback: std::sync::Arc<dyn AgentRunner>,
        claude_code: Option<std::sync::Arc<ClaudeCodeAgentRunner>>,
    ) -> Self {
        Self {
            fallback,
            claude_code,
            in_process: None,
        }
    }

    /// Builder-style setter for the in-process runner (#198).
    ///
    /// Why: Lets the workflow runner wire in a single shared
    /// `InProcessAgentRunner` (carrying the shared LLM client) without
    /// changing every existing call site that constructs a dispatcher.
    /// What: Stores the `Arc<dyn AgentRunner>`; route based on the agent's
    /// declared `RunnerKind::InProcess` happens in `run` / `run_with_context`
    /// / `run_with_history`. When `None`, in-process agents fall through to
    /// the subprocess fallback (with a warn log).
    /// Test: `dispatcher_routes_in_process`.
    pub fn with_in_process(mut self, in_process: Option<std::sync::Arc<dyn AgentRunner>>) -> Self {
        self.in_process = in_process;
        self
    }
}

#[async_trait]
impl AgentRunner for DispatchingAgentRunner {
    async fn run(&self, agent_name: &str, task: &str) -> Result<AgentOutput> {
        // Resolve the runner kind. If the config can't be loaded, fall back
        // to subprocess semantics — matches the engine's existing "missing
        // TOML shouldn't be fatal" behavior elsewhere. #96: use the async
        // loader so we don't block the runtime worker on the config read.
        let kind = AgentConfig::by_name_async(agent_name)
            .await
            .map(|c| c.agent.runner)
            .unwrap_or_default();
        match kind {
            crate::agents::RunnerKind::ClaudeCode => {
                let Some(cc) = &self.claude_code else {
                    anyhow::bail!(
                        "agent '{agent_name}' requires runner=\"claude-code\" but no ClaudeCodeAgentRunner was constructed"
                    );
                };
                cc.run(agent_name, task).await
            }
            crate::agents::RunnerKind::InProcess => match &self.in_process {
                Some(ip) => ip.run(agent_name, task).await,
                None => {
                    tracing::warn!(
                        agent = %agent_name,
                        "in-process runner not configured; falling back to subprocess"
                    );
                    self.fallback.run(agent_name, task).await
                }
            },
            _ => self.fallback.run(agent_name, task).await,
        }
    }

    async fn run_with_history(
        &self,
        agent_name: &str,
        task: &str,
        history: &[crate::session::HistoryMessage],
        ctx: &RunContext,
    ) -> Result<AgentOutput> {
        // Bug #122: thread ctx through so working_dir / model reach
        // the underlying runner for persistent-session calls.
        let kind = AgentConfig::by_name_async(agent_name)
            .await
            .map(|c| c.agent.runner)
            .unwrap_or_default();
        match kind {
            crate::agents::RunnerKind::ClaudeCode => {
                // Claude CLI has its own session concept we don't bridge yet;
                // history is silently dropped for claude-code agents. A
                // warning makes this visible during runs that mix runners.
                if !history.is_empty() {
                    tracing::warn!(
                        agent = %agent_name,
                        turns = history.len(),
                        "claude-code runner does not thread session history; ignoring"
                    );
                }
                let Some(cc) = &self.claude_code else {
                    anyhow::bail!(
                        "agent '{agent_name}' requires runner=\"claude-code\" but no ClaudeCodeAgentRunner was constructed"
                    );
                };
                cc.run_with_context(agent_name, task, ctx).await
            }
            crate::agents::RunnerKind::InProcess => match &self.in_process {
                Some(ip) => ip.run_with_history(agent_name, task, history, ctx).await,
                None => {
                    tracing::warn!(
                        agent = %agent_name,
                        "in-process runner not configured; falling back to subprocess"
                    );
                    self.fallback
                        .run_with_history(agent_name, task, history, ctx)
                        .await
                }
            },
            _ => {
                self.fallback
                    .run_with_history(agent_name, task, history, ctx)
                    .await
            }
        }
    }

    async fn run_with_context(
        &self,
        agent_name: &str,
        task: &str,
        ctx: &RunContext,
    ) -> Result<AgentOutput> {
        let kind = AgentConfig::by_name_async(agent_name)
            .await
            .map(|c| c.agent.runner)
            .unwrap_or_default();
        match kind {
            crate::agents::RunnerKind::ClaudeCode => {
                let Some(cc) = &self.claude_code else {
                    anyhow::bail!(
                        "agent '{agent_name}' requires runner=\"claude-code\" but no ClaudeCodeAgentRunner was constructed"
                    );
                };
                cc.run_with_context(agent_name, task, ctx).await
            }
            crate::agents::RunnerKind::InProcess => match &self.in_process {
                Some(ip) => ip.run_with_context(agent_name, task, ctx).await,
                None => {
                    tracing::warn!(
                        agent = %agent_name,
                        "in-process runner not configured; falling back to subprocess"
                    );
                    self.fallback.run_with_context(agent_name, task, ctx).await
                }
            },
            _ => self.fallback.run_with_context(agent_name, task, ctx).await,
        }
    }
}

/// Find an executable named `claude` in the user's `PATH`.
///
/// Why: Avoid pulling in the `which` crate for a single tiny helper; the
/// project already has zero-dep discipline elsewhere.
/// What: Walks `PATH` entries in order, returning the first one that
/// contains a regular file named `claude`. On non-unix platforms also
/// considers `claude.exe`.
/// Test: Indirectly exercised by `ClaudeCodeAgentRunner::new` on dev
/// machines; unit tests inject the path directly.
fn find_claude() -> Result<PathBuf> {
    let path_var = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH env var is not set"))?;
    for dir in std::env::split_paths(&path_var) {
        if candidate_exists(&dir, "claude") {
            return Ok(dir.join("claude"));
        }
        #[cfg(windows)]
        if candidate_exists(&dir, "claude.exe") {
            return Ok(dir.join("claude.exe"));
        }
    }
    Err(anyhow!(
        "`claude` CLI not found in PATH — install Claude Code (https://claude.ai/download) \
         to use `runner = \"claude-code\"`"
    ))
}

fn candidate_exists(dir: &Path, name: &str) -> bool {
    let p = dir.join(name);
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{
        AgentConfig, AgentInfo, LlmParams, RunnerKind, SystemPrompt, ToolChoice, ToolsConfig,
    };
    use std::sync::Arc;

    #[test]
    fn prepend_harness_layers_adds_base_and_claude_code() {
        // With use_finish_task=false, expect BASE_PROTOCOL + CLAUDE_CODE_PROTOCOL
        // prepended above the supplied TOML base in that order.
        let out = ClaudeCodeAgentRunner::prepend_harness_layers("TOML BASE", false);
        let base_pos = out
            .find("Output Directory")
            .expect("BASE_PROTOCOL (Output Directory) present");
        let cc_pos = out
            .find("write_file Protocol")
            .expect("CLAUDE_CODE_PROTOCOL (write_file Protocol) present");
        let toml_pos = out.find("TOML BASE").expect("TOML base present");
        assert!(
            base_pos < cc_pos,
            "base protocol precedes claude-code layer"
        );
        assert!(cc_pos < toml_pos, "claude-code layer precedes TOML base");
    }

    #[test]
    fn prepend_harness_layers_finish_task_branch() {
        // With use_finish_task=true, expect BASE_PROTOCOL + FINISH_TASK_PROTOCOL,
        // and the CLAUDE_CODE_PROTOCOL write_file block must be absent.
        let out = ClaudeCodeAgentRunner::prepend_harness_layers("TOML BASE", true);
        assert!(out.contains("Output Directory"), "BASE_PROTOCOL injected");
        assert!(out.contains("finish_task"), "FINISH_TASK_PROTOCOL injected");
        assert!(
            !out.contains("write_file Protocol"),
            "CLAUDE_CODE_PROTOCOL skipped when use_finish_task=true"
        );
    }

    /// Build a minimal `AgentConfig` for unit tests. Kept here (not in the
    /// main module) because nothing else in the codebase needs to fabricate
    /// one from scratch — production code always goes through
    /// `AgentConfig::load`.
    fn test_agent_config(name: &str, model: &str, system_prompt: &str) -> AgentConfig {
        AgentConfig {
            agent: AgentInfo {
                name: name.to_string(),
                role: "engineer".to_string(),
                model: model.to_string(),
                description: "test".to_string(),
                persistent_session: false,
                runner: RunnerKind::ClaudeCode,
                capabilities: None,
                display_name: None,
                prompt_label: None,
            },
            llm: LlmParams {
                temperature: 0.0,
                max_tokens: 1024,
                model_override: None,
                enable_prompt_caching: true,
                max_turns: 5,
                tool_choice: ToolChoice::Auto,
                use_finish_task: false,
                use_anthropic_direct: false,
                claude_allowed_tools: Vec::new(),
                aws_profile: None,
                aws_region: None,
                elevation_threshold: None,
                elevation_model: None,
                stop_sequences: Vec::new(),
                routing_model: None,
                thinking_enabled: None,
            },
            system_prompt: SystemPrompt {
                content: system_prompt.to_string(),
                skills: None,
            },
            tools: ToolsConfig::default(),
            compress: crate::agents::AgentCompressConfig::default(),
            runner_config: crate::agents::RunnerConfig::default(),
            session: crate::agents::SessionCompressionConfig::default(),
            plugins: crate::agents::AgentPluginsConfig::default(),
            rbac: crate::agents::RbacConfig::default(),
            adapter: Arc::new(crate::llm::adapter::GenericAdapter),
        }
    }

    #[test]
    fn normalize_model_strips_anthropic_prefix() {
        assert_eq!(
            ClaudeCodeAgentRunner::normalize_model("anthropic/claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(ClaudeCodeAgentRunner::normalize_model("sonnet"), "sonnet");
        assert_eq!(
            ClaudeCodeAgentRunner::normalize_model("claude-haiku-4"),
            "claude-haiku-4"
        );
    }

    #[test]
    fn normalize_model_openai_untouched() {
        assert_eq!(
            ClaudeCodeAgentRunner::normalize_model("openai/gpt-4.1"),
            "openai/gpt-4.1"
        );
    }

    #[test]
    fn strip_finish_task_instructions_removes_callouts() {
        // #113: Lines that mention `finish_task` must vanish so the claude
        // CLI doesn't spin looking for a tool it doesn't have.
        let input = "\
Steps:
1. Do the thing
2. Write the file
3. Call finish_task when done
4. Exit
";
        let out = ClaudeCodeAgentRunner::strip_finish_task_instructions(input);
        assert!(!out.contains("finish_task"), "should strip: {out}");
        assert!(out.contains("Do the thing"));
        assert!(out.contains("Write the file"));
        assert!(out.contains("Exit"));
    }

    #[test]
    fn strip_finish_task_instructions_is_idempotent() {
        let input = "plain text with no tool reference";
        let once = ClaudeCodeAgentRunner::strip_finish_task_instructions(input);
        let twice = ClaudeCodeAgentRunner::strip_finish_task_instructions(&once);
        assert_eq!(once, twice);
        assert_eq!(once, input);
    }

    #[test]
    fn strip_finish_task_instructions_preserves_unrelated_text() {
        // Multi-line prose that does NOT mention finish_task should pass
        // through unchanged, preserving newlines.
        let input = "line one\nline two\nline three\n";
        let out = ClaudeCodeAgentRunner::strip_finish_task_instructions(input);
        assert_eq!(out, input);
    }

    #[test]
    fn strip_finish_task_instructions_case_insensitive() {
        let input = "Call Finish_Task now\nor FINISH_TASK\nkeep this";
        let out = ClaudeCodeAgentRunner::strip_finish_task_instructions(input);
        assert!(
            !out.to_ascii_lowercase().contains("finish_task"),
            "got: {out}"
        );
        assert!(out.contains("keep this"));
    }

    #[test]
    fn find_claude_errors_when_not_in_path() {
        // Point PATH at an empty temp dir and confirm we get a useful error.
        // SAFETY: this mutates a process-global env var. The test is marked
        // serial-ish by running it as a separate #[test] with no concurrent
        // env writes elsewhere in this module.
        let tmp = tempfile::tempdir().unwrap();
        let old = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", tmp.path());
        }
        let err = find_claude().unwrap_err();
        assert!(
            err.to_string().contains("not found in PATH"),
            "unexpected error: {err}"
        );
        unsafe {
            match old {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// Mock-`claude` integration test: write a tiny shell script that emits
    /// a stream-json transcript and confirm we parse it into `AgentOutput`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_parses_stream_json_result() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("claude");
        // Script emits two non-result events (system init, assistant) then a
        // final result. Stderr swallows argv so we don't pollute test output.
        // Use `printf %s\\n` to emit each JSON object as one line without
        // shell-specific `echo` differences expanding `\n` escapes (bash vs
        // dash behave differently on macOS/Linux; printf is portable).
        // The result's `result` field embeds literal \n escapes so the
        // JSON parser sees them as newlines inside the string.
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"test\"}'\n\
             printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"thinking\"}]}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"result\":\"Hello from mock claude\\n\\n## Summary\\nMock OK\",\"is_error\":false}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        let cfg = test_agent_config("mock-agent", "anthropic/claude-sonnet-4-6", "test prompt");

        let out = runner.run_with_config(&cfg, "say hi").await.expect("runs");
        assert!(
            out.content.starts_with("Hello from mock claude"),
            "unexpected content: {:?}",
            out.content
        );
        assert_eq!(out.summary.as_deref(), Some("Mock OK"));
        assert_eq!(out.usage, TokenUsage::default());
    }

    /// #107: `RunContext.model` must take precedence over
    /// `cfg.agent.model` when passing `--model` to the `claude` CLI so a
    /// workflow phase's `model` actually reaches the CLI.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_honors_ctx_model_override() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let argv_log = tmp.path().join("argv.txt");
        let script = tmp.path().join("claude");
        // Record argv then emit a valid result so the runner doesn't error.
        let script_body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {log}\n\
             printf '%s\\n' '{{\"type\":\"result\",\"result\":\"ok\",\"is_error\":false}}'\n",
            log = argv_log.display()
        );
        std::fs::write(&script, script_body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        // TOML says sonnet, ctx overrides to opus.
        let cfg = test_agent_config("mock-agent", "anthropic/claude-sonnet-4-6", "test prompt");
        let ctx = RunContext {
            model: Some("anthropic/claude-opus-4-6".to_string()),
            ..RunContext::default()
        };

        let _out = runner
            .run_with_config_ctx(&cfg, "say hi", &ctx)
            .await
            .expect("runs");

        let argv = std::fs::read_to_string(&argv_log).expect("argv log written");
        // Expect the normalized override (prefix stripped) next to --model.
        assert!(
            argv.contains("--model\nclaude-opus-4-6\n"),
            "expected --model claude-opus-4-6 in argv, got:\n{argv}"
        );
        assert!(
            !argv.contains("claude-sonnet-4-6"),
            "TOML model should have been overridden, got:\n{argv}"
        );
    }

    /// When `is_error: true`, the runner returns an error whose message
    /// surfaces the CLI's complaint.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_propagates_is_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' '{\"type\":\"result\",\"result\":\"rate limit exceeded\",\"is_error\":true}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        let cfg = test_agent_config("mock-agent", "sonnet", "test");
        let err = runner
            .run_with_config(&cfg, "try")
            .await
            .expect_err("should propagate is_error");
        assert!(err.to_string().contains("rate limit"), "unexpected: {err}");
    }

    /// #113: `error_max_turns` with a non-empty `result` is treated as
    /// success — the agent produced work, the CLI just happened to exhaust
    /// its turn budget before emitting a clean terminator.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_recovers_from_max_turns_with_content() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial work\"}]}}'\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"result\":\"partial work\",\"is_error\":true}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        let cfg = test_agent_config("mock-agent", "sonnet", "test");
        let out = runner
            .run_with_config(&cfg, "try")
            .await
            .expect("max_turns with content should be treated as success");
        assert!(out.content.contains("partial work"));
    }

    /// #113: `error_max_turns` with completely empty content must still
    /// propagate as an error — we don't want to hide a truly failed run.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_max_turns_without_content_still_errors() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"result\":\"\",\"is_error\":true}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        let cfg = test_agent_config("mock-agent", "sonnet", "test");
        let err = runner
            .run_with_config(&cfg, "try")
            .await
            .expect_err("empty max_turns should still error");
        assert!(
            err.to_string().contains("error_max_turns"),
            "unexpected: {err}"
        );
    }

    /// #113: Both `task` and `system prompt` are sanitized before being
    /// handed to the CLI; argv should not contain `finish_task`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_sanitizes_finish_task_from_cli_args() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let argv_log = tmp.path().join("argv.txt");
        let script = tmp.path().join("claude");
        let script_body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {log}\n\
             printf '%s\\n' '{{\"type\":\"result\",\"result\":\"ok\",\"is_error\":false}}'\n",
            log = argv_log.display()
        );
        std::fs::write(&script, script_body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        let cfg = test_agent_config(
            "mock-agent",
            "sonnet",
            "You are a test agent.\nWhen done, call finish_task with a summary.",
        );
        let _ = runner
            .run_with_config(&cfg, "Do the work.\nThen call finish_task.")
            .await
            .expect("runs");

        let argv = std::fs::read_to_string(&argv_log).expect("argv log written");
        assert!(
            !argv.contains("finish_task"),
            "finish_task leaked into CLI args:\n{argv}"
        );
        // Surrounding content should still be present.
        assert!(argv.contains("Do the work."));
        assert!(argv.contains("You are a test agent."));
    }

    /// Dispatcher: when no claude-code runner is wired and the agent's TOML
    /// doesn't request `claude-code`, calls fall through to the fallback.
    #[tokio::test]
    async fn dispatcher_routes_subprocess_by_default() {
        struct FakeFallback;
        #[async_trait]
        impl AgentRunner for FakeFallback {
            async fn run(&self, _agent: &str, _task: &str) -> Result<AgentOutput> {
                Ok(AgentOutput {
                    content: "fallback-ran".into(),
                    summary: None,
                    usage: TokenUsage::default(),
                })
            }
        }
        // Use an agent name with no TOML on disk. The dispatcher's config
        // lookup fails and `unwrap_or_default()` yields `RunnerKind::Subprocess`,
        // which routes to the fallback. This keeps the test independent of
        // which runner any shipped agent TOML happens to declare today (e.g.
        // python-engineer migrated to runner="claude-code", which used to
        // break this test).
        let dispatcher = DispatchingAgentRunner::new(Arc::new(FakeFallback), None);
        let out = dispatcher
            .run("__nonexistent_test_agent__", "x")
            .await
            .unwrap();
        assert_eq!(out.content, "fallback-ran");
    }

    /// Dispatcher: when claude-code is requested but not wired, the call
    /// errors with a clear message instead of silently falling through.
    #[tokio::test]
    async fn dispatcher_errors_when_claude_code_requested_but_unwired() {
        struct FakeFallback;
        #[async_trait]
        impl AgentRunner for FakeFallback {
            async fn run(&self, _agent: &str, _task: &str) -> Result<AgentOutput> {
                unreachable!("fallback should not be called for claude-code agents");
            }
        }
        // We need an agent TOML with runner="claude-code". The test for
        // claude-code-engineer.toml (added in this change) serves as that.
        let dispatcher = DispatchingAgentRunner::new(Arc::new(FakeFallback), None);
        let err = dispatcher
            .run("claude-code-engineer", "x")
            .await
            .expect_err("should error");
        assert!(err.to_string().contains("claude-code"), "unexpected: {err}");
    }

    /// When the CLI exits without ever emitting a `result` event, the runner
    /// surfaces a clear error rather than hanging.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_errors_when_no_result_event() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let runner = ClaudeCodeAgentRunner { claude_bin: script };
        let cfg = test_agent_config("mock-agent", "sonnet", "test");
        let err = runner
            .run_with_config(&cfg, "try")
            .await
            .expect_err("should error without result event");
        assert!(
            err.to_string().contains("without producing a `result`"),
            "unexpected: {err}"
        );
    }
}
