//! Test-only transport shims.
//!
//! Why: `trusty-rpc` is a bin crate (no library surface), so integration tests
//! can't import its `Transport` impls. We mirror the minimum behaviour here
//! to exercise the same wire format against real subprocesses and TCP servers.
//! What: tiny `StdioT` / `HttpT` types with a single `send` method each.
//! Test: consumed by `tests/integration.rs`.

use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

pub struct StdioT {
    _child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
}

impl StdioT {
    pub async fn send(&self, request: Value) -> anyhow::Result<Value> {
        let notif = request.get("id").is_none();
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        {
            let mut s = self.stdin.lock().await;
            s.write_all(line.as_bytes()).await?;
            s.flush().await?;
        }
        if notif {
            return Ok(Value::Null);
        }
        let mut out = self.stdout.lock().await;
        loop {
            let mut buf = String::new();
            let n = out.read_line(&mut buf).await?;
            if n == 0 {
                anyhow::bail!("subprocess closed stdout");
            }
            let t = buf.trim();
            if t.is_empty() || !t.starts_with('{') {
                continue;
            }
            return Ok(serde_json::from_str(t)?);
        }
    }
}

pub async fn spawn_stdio(program: &str, args: &[&str]) -> Arc<StdioT> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn test subprocess");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    Arc::new(StdioT {
        _child: Mutex::new(child),
        stdin: Mutex::new(stdin),
        stdout: Mutex::new(BufReader::new(stdout)),
    })
}

pub struct HttpT {
    client: reqwest::Client,
    url: String,
}

impl HttpT {
    pub async fn send(&self, request: Value) -> anyhow::Result<Value> {
        let resp = self.client.post(&self.url).json(&request).send().await?;
        let body = resp.text().await?;
        if body.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&body)?)
    }
}

pub fn http(url: &str) -> Arc<HttpT> {
    Arc::new(HttpT {
        client: reqwest::Client::new(),
        url: url.to_string(),
    })
}
