//! Stdio subprocess transport.
//!
//! Why: MCP servers (and many JSON-RPC tools) communicate over stdin/stdout
//! with newline-delimited JSON. `trpc` needs to spawn one and exchange messages.
//! What: Spawns the configured command via `tokio::process::Command`, writes
//! one JSON line to its stdin per request, and reads one JSON line from its
//! stdout per non-notification request. Tolerates non-JSON log lines on stdout
//! by skipping anything that doesn't start with `{`.
//! Test: integration coverage via `tests/integration.rs` and manual smoke
//! tests against MCP servers.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{Transport, is_notification};

/// Spawns a subprocess and exchanges newline-delimited JSON with it.
///
/// Why: encapsulates the child-process lifecycle and concurrent stdin/stdout
/// access (each guarded by its own `Mutex`) so callers can send from anywhere.
/// What: keeps the `Child` alive for the duration of the transport; drops it
/// on `Drop` which terminates the subprocess.
/// Test: see `tests/integration.rs::stdio_transport_echo`.
pub struct StdioTransport {
    // Held to keep the child alive; `_child` because we don't read from it
    // directly after spawn (stdin/stdout were taken).
    _child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
}

impl StdioTransport {
    /// Spawn the given command string (split on whitespace).
    ///
    /// Why: convenience for the `--cmd "binary --flag arg"` CLI form.
    /// What: tokenises naively on whitespace; for arguments containing spaces
    /// callers must pre-tokenise via `spawn_argv`.
    /// Test: `tests/integration.rs::stdio_transport_echo` spawns a real cmd.
    pub async fn new(cmd: &str) -> Result<Self> {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        if parts.is_empty() {
            anyhow::bail!("--cmd is empty");
        }
        Self::spawn_argv(parts[0], &parts[1..]).await
    }

    /// Spawn an explicit program + args vector.
    pub async fn spawn_argv(program: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn subprocess: {program}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture subprocess stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture subprocess stdout"))?;

        Ok(Self {
            _child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
        })
    }
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn send(&self, request: Value) -> Result<Value> {
        let notif = is_notification(&request);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');

        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .context("writing request to subprocess stdin")?;
            stdin.flush().await.context("flushing subprocess stdin")?;
        }

        if notif {
            // No response expected for notifications.
            return Ok(Value::Null);
        }

        // Read lines, skipping blanks and obvious non-JSON log noise.
        let mut stdout = self.stdout.lock().await;
        loop {
            let mut buf = String::new();
            let n = stdout
                .read_line(&mut buf)
                .await
                .context("reading subprocess stdout")?;
            if n == 0 {
                anyhow::bail!("subprocess closed stdout before responding");
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !trimmed.starts_with('{') {
                // Some servers emit log lines to stdout despite spec. Skip.
                tracing::debug!(line = %trimmed, "skipping non-JSON line from subprocess");
                continue;
            }
            let val: Value = serde_json::from_str(trimmed)
                .with_context(|| format!("parsing JSON response: {trimmed}"))?;
            return Ok(val);
        }
    }
}
