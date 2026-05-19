//! Python subprocess tool plugin (#446).
//!
//! Why: Agents need to expose domain-specific tooling (custom reports, data
//! lookups, internal APIs) without baking that logic into the open-mpm core
//! binary. A subprocess plugin lets operators ship a Python script alongside
//! their agent TOML — the harness spawns `python3 <script>`, hands it
//! JSON-encoded parameters on stdin, and reads back a structured result on
//! stdout. Same NDJSON envelope used elsewhere in the harness.
//! What: `PythonToolPlugin` holds the static metadata (name, schema, script
//! path, timeout, RBAC tier guard) parsed from `[[plugins.python]]` agent TOML
//! entries. `execute(params)` spawns one short-lived `python3` child per call,
//! writes a single `{"type":"tool_call",...}` line, reads back one
//! `{"type":"tool_result",...}` line, and returns a `ToolResult`. Timeouts kill
//! the child; non-zero exits and malformed JSON become recoverable errors.
//! Test: See `tests` at the bottom of this file — happy path uses an inline
//! `python3 -c "..."` script that echoes a fixed result; timeout, non-zero
//! exit, and malformed stdout all produce structured `ToolResult::Error`s.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::tools::traits::{ToolExecutor, ToolResult};

/// Default per-call timeout (seconds) when the agent TOML omits `timeout_secs`.
///
/// Why: 30s is generous enough for I/O-bound scripts (HTTP fetches, DB
/// queries) without letting a runaway child block the agent loop forever.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Raw TOML shape for one `[[plugins.python]]` entry.
///
/// Why: Kept distinct from the runtime `PythonToolPlugin` so we can parse the
/// loose, file-shaped form (either `schema_file` or inline `schema`) and then
/// normalise it into a single owned `Value` schema for execution.
/// What: All fields except `name`, `description`, and `script` have defaults.
/// `schema` and `schema_file` are mutually optional — at least one should be
/// supplied; both missing yields an empty object schema.
/// Test: See `python_plugin_config_parses_inline_schema` and
/// `python_plugin_config_parses_schema_file`.
#[derive(Debug, Clone, Deserialize)]
pub struct PythonPluginConfig {
    pub name: String,
    pub description: String,
    pub script: PathBuf,
    /// JSON Schema for the tool's `parameters`, supplied inline in TOML.
    #[serde(default)]
    pub schema: Option<Value>,
    /// Path to a JSON file containing the parameters schema. Resolved relative
    /// to the agent TOML directory by `PythonToolPlugin::from_config`.
    #[serde(default)]
    pub schema_file: Option<PathBuf>,
    /// Per-call timeout in seconds. Defaults to `DEFAULT_TIMEOUT_SECS`.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// RBAC tier guard (#446). Empty list = unrestricted. When a caller's
    /// effective tier is in this list, the plugin refuses to execute.
    #[serde(default)]
    pub restricted_tiers: Vec<String>,
}

/// Runtime-ready Python subprocess tool.
///
/// Why: Splits the loose TOML config (`PythonPluginConfig`) from a normalised
/// runtime value with a resolved schema and absolute script path. This way the
/// hot path (`execute`) never has to redo IO that should happen once at load.
/// What: Fields are the minimum the executor needs — name, description, script
/// path, schema, timeout, and RBAC tier guard.
/// Test: See `tests` below — `from_config` resolves schemas; `execute` covers
/// the success / timeout / exit / parse-error paths.
#[derive(Debug, Clone)]
pub struct PythonToolPlugin {
    pub name: String,
    pub description: String,
    pub script: PathBuf,
    pub schema: Value,
    pub timeout: Duration,
    pub restricted_tiers: Vec<String>,
}

impl PythonToolPlugin {
    /// Build a runtime plugin from a TOML config plus the agent TOML's
    /// directory (used to resolve relative `script` / `schema_file` paths).
    ///
    /// Why: Relative paths in `[[plugins.python]]` are intuitively relative to
    /// the agent TOML, not the process CWD. Resolving them once at load time
    /// avoids surprising "file not found" errors deep in agent execution.
    /// What: Joins `base_dir` with `script` (and `schema_file` if present) when
    /// the path is relative; reads the schema file when supplied; falls back to
    /// inline `schema`, then to an empty object schema if neither is set.
    /// Test: `python_tool_plugin_from_config_resolves_relative_paths`,
    /// `python_tool_plugin_from_config_loads_schema_file`.
    pub fn from_config(cfg: PythonPluginConfig, base_dir: &Path) -> anyhow::Result<Self> {
        let script = if cfg.script.is_absolute() {
            cfg.script
        } else {
            base_dir.join(&cfg.script)
        };

        let schema = match (cfg.schema, cfg.schema_file) {
            (Some(inline), _) => inline,
            (None, Some(path)) => {
                let resolved = if path.is_absolute() {
                    path
                } else {
                    base_dir.join(&path)
                };
                let raw = std::fs::read_to_string(&resolved).map_err(|e| {
                    anyhow::anyhow!(
                        "python plugin '{}': failed to read schema_file {}: {}",
                        cfg.name,
                        resolved.display(),
                        e
                    )
                })?;
                serde_json::from_str(&raw).map_err(|e| {
                    anyhow::anyhow!(
                        "python plugin '{}': schema_file {} is not valid JSON: {}",
                        cfg.name,
                        resolved.display(),
                        e
                    )
                })?
            }
            (None, None) => json!({"type": "object", "properties": {}}),
        };

        let timeout = Duration::from_secs(cfg.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        Ok(Self {
            name: cfg.name,
            description: cfg.description,
            script,
            schema,
            timeout,
            restricted_tiers: cfg.restricted_tiers,
        })
    }

    /// Render this plugin as an OpenAI-compatible tool spec (the `{"type":
    /// "function", ...}` envelope the LLM sees).
    ///
    /// Why: Plugin tools must look indistinguishable from native tools at the
    /// wire level so the LLM can call them with the same JSON tool-call shape.
    /// What: Wraps `description` + `schema` in the standard envelope. Clones
    /// the schema so callers can keep using `&self`.
    /// Test: `python_tool_plugin_to_tool_spec_shape`.
    pub fn to_tool_spec(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.schema,
            }
        })
    }

    /// Whether a caller with the given tier is forbidden from invoking this
    /// plugin (RBAC guard, #446).
    ///
    /// Why: Some plugins expose sensitive data (priority email, financials)
    /// and should not be visible to lower-privilege tiers. Returning the guard
    /// decision as a separate method lets the dispatch layer choose between
    /// "hide from tool list" and "return error on invocation".
    /// What: True when `tier` matches any string in `restricted_tiers`. Empty
    /// `restricted_tiers` => always returns false.
    /// Test: `python_tool_plugin_rbac_guard`.
    pub fn is_restricted_for_tier(&self, tier: &str) -> bool {
        self.restricted_tiers.iter().any(|t| t == tier)
    }

    /// Execute the plugin with the given JSON parameters.
    ///
    /// Why: This is the hot path — every LLM tool call lands here. We isolate
    /// the spawn / write / read / kill logic so a single helper handles all
    /// failure modes uniformly.
    /// What: Spawns `python3 <script>`, writes one NDJSON `tool_call` line to
    /// stdin, closes stdin, reads one NDJSON line from stdout, and returns the
    /// parsed `content`. Timeout, non-zero exit, missing stdout, and malformed
    /// JSON all become recoverable `ToolResult::Error`s.
    /// Test: `execute_happy_path_inline_python`, `execute_returns_timeout_error`,
    /// `execute_returns_error_on_nonzero_exit`, `execute_returns_error_on_malformed_json`.
    pub async fn execute(&self, params: Value) -> ToolResult {
        match self.execute_inner(params).await {
            Ok(result) => result,
            Err(e) => ToolResult::err(format!("python plugin '{}' failed: {}", self.name, e)),
        }
    }

    async fn execute_inner(&self, params: Value) -> anyhow::Result<ToolResult> {
        let id = Uuid::new_v4().to_string();
        let call = json!({
            "type": "tool_call",
            "id": id,
            "params": params,
        });
        let line = format!("{}\n", serde_json::to_string(&call)?);

        // Allow tests / advanced users to override the interpreter (e.g. set
        // `OPEN_MPM_PYTHON=python` on systems where only `python` is on PATH).
        let python = std::env::var("OPEN_MPM_PYTHON").unwrap_or_else(|_| "python3".to_string());

        // For the special "-c" + inline-script test pattern we accept a script
        // path whose first component is literally `-c:` followed by the inline
        // body. This keeps the test surface honest (no temp files) without
        // introducing a separate "inline" config field for production use.
        let script_str = self.script.to_string_lossy().to_string();
        let mut cmd = Command::new(&python);
        if let Some(inline) = script_str.strip_prefix("-c:") {
            cmd.arg("-c").arg(inline);
        } else {
            cmd.arg(&self.script);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        debug!(plugin = %self.name, script = %script_str, "spawning python plugin");
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn `{} {}`: {}", python, script_str, e))?;

        // Write the single tool_call line, then close stdin so the child sees EOF.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
            drop(stdin);
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
        let mut reader = BufReader::new(stdout).lines();

        // Read one line + wait for exit, all under a single timeout.
        let work = async {
            let line_opt = reader.next_line().await?;
            let status = child.wait().await?;
            Ok::<_, anyhow::Error>((line_opt, status))
        };

        let (line_opt, status) = match timeout(self.timeout, work).await {
            Ok(res) => res?,
            Err(_) => {
                // Best-effort kill; reader / child drop will reap.
                warn!(plugin = %self.name, "python plugin timed out after {:?}", self.timeout);
                return Ok(ToolResult::err(format!(
                    "python plugin '{}' timed out after {}s",
                    self.name,
                    self.timeout.as_secs()
                )));
            }
        };

        if !status.success() {
            return Ok(ToolResult::err(format!(
                "python plugin '{}' exited with status {:?}",
                self.name,
                status.code()
            )));
        }

        let Some(line) = line_opt else {
            return Ok(ToolResult::err(format!(
                "python plugin '{}' produced no stdout",
                self.name
            )));
        };

        // Parse the NDJSON result envelope.
        let parsed: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::err(format!(
                    "python plugin '{}' returned malformed JSON: {} (line: {})",
                    self.name,
                    e,
                    line.trim()
                )));
            }
        };

        let status_field = parsed
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("success");
        let content = parsed
            .get("content")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();

        if status_field == "error" {
            let err_msg = parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or(&content);
            return Ok(ToolResult::err(format!(
                "python plugin '{}' reported error: {}",
                self.name, err_msg
            )));
        }

        Ok(ToolResult::ok(content))
    }
}

#[async_trait]
impl ToolExecutor for PythonToolPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> Value {
        self.to_tool_spec()
    }

    async fn execute(&self, args: Value) -> ToolResult {
        PythonToolPlugin::execute(self, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a plugin that runs an inline `python3 -c "<body>"` invocation.
    ///
    /// Why: Tests need a self-contained Python script with no external file
    /// dependencies. Using the `-c:` script-path prefix routes execution to
    /// `python3 -c <body>` while reusing all the spawn / IPC plumbing.
    /// What: Construct a `PythonToolPlugin` with the given inline body and a
    /// 5-second timeout (override the timeout for the timeout-path test).
    fn inline_plugin(name: &str, body: &str, timeout_secs: u64) -> PythonToolPlugin {
        PythonToolPlugin {
            name: name.to_string(),
            description: "test plugin".to_string(),
            script: PathBuf::from(format!("-c:{body}")),
            schema: json!({"type": "object", "properties": {}}),
            timeout: Duration::from_secs(timeout_secs),
            restricted_tiers: Vec::new(),
        }
    }

    /// Check whether `python3` is on PATH; if not, integration-style tests are
    /// skipped (CI may not have python installed for a Rust crate).
    fn python_available() -> bool {
        let python = std::env::var("OPEN_MPM_PYTHON").unwrap_or_else(|_| "python3".to_string());
        std::process::Command::new(&python)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Why: TOML round-trip for the `[[plugins.python]]` form with inline schema.
    /// What: Parse a TOML doc containing one plugin entry; assert all fields
    /// land in `PythonPluginConfig`.
    #[test]
    fn python_plugin_config_parses_inline_schema() {
        #[derive(Deserialize)]
        struct Wrapper {
            plugins: Plugins,
        }
        #[derive(Deserialize)]
        struct Plugins {
            python: Vec<PythonPluginConfig>,
        }

        let toml_src = r#"
[[plugins.python]]
name = "echo"
description = "Echo the query back"
script = "scripts/echo.py"
timeout_secs = 5
restricted_tiers = ["analytics"]

[plugins.python.schema]
type = "object"

[plugins.python.schema.properties]
query = { type = "string" }
"#;
        let parsed: Wrapper = toml::from_str(toml_src).expect("toml parses");
        assert_eq!(parsed.plugins.python.len(), 1);
        let p = &parsed.plugins.python[0];
        assert_eq!(p.name, "echo");
        assert_eq!(p.description, "Echo the query back");
        assert_eq!(p.script, PathBuf::from("scripts/echo.py"));
        assert_eq!(p.timeout_secs, Some(5));
        assert_eq!(p.restricted_tiers, vec!["analytics".to_string()]);
        assert!(p.schema.is_some());
        assert!(p.schema_file.is_none());
    }

    /// Why: TOML round-trip for the `schema_file` form (preferred when a
    /// schema lives next to the script and is shared with other tooling).
    /// What: Assert `schema_file` parses and `schema` is None.
    #[test]
    fn python_plugin_config_parses_schema_file() {
        #[derive(Deserialize)]
        struct Wrapper {
            plugins: Plugins,
        }
        #[derive(Deserialize)]
        struct Plugins {
            python: Vec<PythonPluginConfig>,
        }

        let toml_src = r#"
[[plugins.python]]
name = "gfa_report"
description = "GFA report"
script = "../cto/app/gfa.py"
schema_file = "../cto/app/gfa_schema.json"
"#;
        let parsed: Wrapper = toml::from_str(toml_src).expect("toml parses");
        let p = &parsed.plugins.python[0];
        assert_eq!(
            p.schema_file,
            Some(PathBuf::from("../cto/app/gfa_schema.json"))
        );
        assert!(p.schema.is_none());
        // Defaults
        assert!(p.timeout_secs.is_none());
        assert!(p.restricted_tiers.is_empty());
    }

    /// Why: Relative `script` paths in TOML must be resolved against the agent
    /// TOML directory, not the process CWD (which is unpredictable).
    /// What: Build a config with a relative path, hand it a base_dir, and
    /// confirm the runtime `script` is `<base_dir>/<rel>`.
    #[test]
    fn python_tool_plugin_from_config_resolves_relative_paths() {
        let cfg = PythonPluginConfig {
            name: "x".into(),
            description: "y".into(),
            script: PathBuf::from("tools/x.py"),
            schema: Some(json!({"type": "object"})),
            schema_file: None,
            timeout_secs: None,
            restricted_tiers: vec![],
        };
        let base = Path::new("/agents/foo");
        let plugin = PythonToolPlugin::from_config(cfg, base).expect("from_config");
        assert_eq!(plugin.script, PathBuf::from("/agents/foo/tools/x.py"));
        assert_eq!(plugin.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    /// Why: `schema_file` must be read from disk at load time, parsed as JSON,
    /// and stored as the runtime `schema`.
    /// What: Write a temp JSON file, build a config pointing at it, assert the
    /// runtime schema matches.
    #[test]
    fn python_tool_plugin_from_config_loads_schema_file() {
        let tmp = std::env::temp_dir().join(format!("om-plugin-schema-{}.json", Uuid::new_v4()));
        std::fs::write(
            &tmp,
            r#"{"type":"object","properties":{"q":{"type":"string"}}}"#,
        )
        .unwrap();

        let cfg = PythonPluginConfig {
            name: "x".into(),
            description: "y".into(),
            script: PathBuf::from("s.py"),
            schema: None,
            schema_file: Some(tmp.clone()),
            timeout_secs: None,
            restricted_tiers: vec![],
        };
        let plugin = PythonToolPlugin::from_config(cfg, Path::new("/anywhere")).unwrap();
        assert_eq!(plugin.schema["type"], "object");
        assert_eq!(plugin.schema["properties"]["q"]["type"], "string");

        let _ = std::fs::remove_file(&tmp);
    }

    /// Why: `to_tool_spec` is the wire shape the LLM sees; pin the envelope so
    /// a structural change in `schema()` doesn't silently break tool calling.
    /// What: Assert the standard OpenAI envelope keys are present.
    #[test]
    fn python_tool_plugin_to_tool_spec_shape() {
        let plugin = inline_plugin("echo", "pass", 30);
        let spec = plugin.to_tool_spec();
        assert_eq!(spec["type"], "function");
        assert_eq!(spec["function"]["name"], "echo");
        assert_eq!(spec["function"]["description"], "test plugin");
        assert_eq!(spec["function"]["parameters"]["type"], "object");
    }

    /// Why: RBAC guard returns true only when caller tier appears in the
    /// plugin's `restricted_tiers` list.
    /// What: Construct a plugin with `restricted_tiers = ["analytics"]`, assert
    /// guard fires for "analytics" but not for "all".
    #[test]
    fn python_tool_plugin_rbac_guard() {
        let mut plugin = inline_plugin("p", "pass", 30);
        plugin.restricted_tiers = vec!["analytics".into(), "read_only".into()];
        assert!(plugin.is_restricted_for_tier("analytics"));
        assert!(plugin.is_restricted_for_tier("read_only"));
        assert!(!plugin.is_restricted_for_tier("all"));
        assert!(!plugin.is_restricted_for_tier(""));
    }

    /// Why: Happy-path execution — spawn an inline python script that reads
    /// the tool_call line and echoes a tool_result. Proves the full IPC loop.
    /// What: Inline python reads one line, parses it, emits a tool_result with
    /// content "hello".
    #[tokio::test]
    async fn execute_happy_path_inline_python() {
        if !python_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let body = r#"
import sys, json
line = sys.stdin.readline()
req = json.loads(line)
out = {"type": "tool_result", "id": req["id"], "content": "hello", "status": "success"}
print(json.dumps(out), flush=True)
"#;
        let plugin = inline_plugin("echo", body, 10);
        let result = plugin.execute(json!({"q": "ignored"})).await;
        assert!(
            !result.is_error(),
            "expected success, got: {}",
            result.content()
        );
        assert_eq!(result.content(), "hello");
    }

    /// Why: A misbehaving plugin that sleeps past its timeout must be killed
    /// and produce a structured timeout error rather than hanging the agent.
    /// What: Inline python sleeps 10s; plugin timeout is 1s; expect error.
    #[tokio::test]
    async fn execute_returns_timeout_error() {
        if !python_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let body = r#"
import time, sys
time.sleep(10)
sys.stdout.write('never\n')
"#;
        let plugin = inline_plugin("slow", body, 1);
        let result = plugin.execute(json!({})).await;
        assert!(result.is_error(), "expected timeout error");
        assert!(
            result.content().contains("timed out"),
            "expected timeout message, got: {}",
            result.content()
        );
    }

    /// Why: A plugin that crashes (non-zero exit) without emitting a result
    /// must not be reported as success. The error message should carry the
    /// exit status for debugging.
    /// What: Inline python calls `sys.exit(2)` without writing to stdout.
    #[tokio::test]
    async fn execute_returns_error_on_nonzero_exit() {
        if !python_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let body = r#"
import sys
sys.exit(2)
"#;
        let plugin = inline_plugin("crash", body, 5);
        let result = plugin.execute(json!({})).await;
        assert!(result.is_error(), "expected error on non-zero exit");
        // Either "exited with status" or "produced no stdout" is acceptable
        // (the order of stdout-close vs. exit observation is timing-dependent),
        // but neither is a success.
        let msg = result.content();
        assert!(
            msg.contains("exited with status") || msg.contains("produced no stdout"),
            "unexpected error message: {}",
            msg
        );
    }

    /// Why: A plugin that prints garbage on stdout must be surfaced as a parse
    /// error, not silently swallowed (which would look like an empty success).
    /// What: Inline python prints `not json` on stdout.
    #[tokio::test]
    async fn execute_returns_error_on_malformed_json() {
        if !python_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let body = r#"
import sys
sys.stdout.write('not json at all\n')
sys.stdout.flush()
"#;
        let plugin = inline_plugin("garbage", body, 5);
        let result = plugin.execute(json!({})).await;
        assert!(result.is_error(), "expected malformed-json error");
        assert!(
            result.content().contains("malformed JSON"),
            "unexpected error message: {}",
            result.content()
        );
    }

    /// Why: When the plugin reports `status = "error"` in its NDJSON result,
    /// the harness must surface that as a `ToolResult::Error` rather than
    /// treating it as success.
    /// What: Inline python emits `{"type":"tool_result","status":"error",...}`.
    #[tokio::test]
    async fn execute_propagates_plugin_reported_error() {
        if !python_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let body = r#"
import sys, json
req = json.loads(sys.stdin.readline())
out = {"type": "tool_result", "id": req["id"], "status": "error", "error": "boom"}
print(json.dumps(out), flush=True)
"#;
        let plugin = inline_plugin("err", body, 5);
        let result = plugin.execute(json!({})).await;
        assert!(result.is_error());
        assert!(
            result.content().contains("boom"),
            "expected reported error to bubble up, got: {}",
            result.content()
        );
    }
}
