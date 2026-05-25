//! Handler for `trusty-memory start` — boots the HTTP daemon in the background.
//!
//! Why: the three trusty-* daemons (search, memory, mpm) historically had
//! diverging `start` / `serve` / `stop` semantics. As of this change,
//! `trusty-memory start` mirrors `trusty-search start`: it self-spawns a
//! detached copy of the binary with `serve --foreground` so the parent shell
//! gets its prompt back immediately. A second `start` invocation while the
//! daemon is already healthy is a no-op (prints the live address and exits 0)
//! rather than racing a second instance against the dynamic port walker.
//! What: a thin async handler that probes `read_daemon_addr("trusty-memory")`
//! plus an HTTP `/health` check, and on cold start spawns `<this exe> serve
//! --foreground` with stdio piped to `/dev/null`.
//! Test: `cargo run -p trusty-memory -- start` returns immediately; running
//! it twice in a row reports "already running" on the second invocation.

use anyhow::Result;
use colored::Colorize;
use std::time::Duration;

/// Probe whether a trusty-memory daemon is already bound and answering
/// `/health` on the address recorded in `~/.trusty-memory/http_addr`.
///
/// Why: a second `start` invocation must NOT spawn a second daemon — the port
/// walker in `bind_dynamic_port` would happily pick the next free slot,
/// leaving the operator with two competing instances. The address file is the
/// canonical "where am I?" record; a successful `/health` probe against it
/// confirms the recorded address still points at a live process.
/// What: reads the address file via `trusty_common::read_daemon_addr` (which
/// returns `Ok(None)` when no file exists), then issues a 1-second-timeout
/// `GET /health` against the recorded `host:port`. Returns `Some(url)` only
/// when both reads succeed.
/// Test: covered indirectly by the `start` integration path — the second
/// invocation prints "already running" rather than spawning a duplicate.
async fn probe_running_daemon() -> Option<String> {
    let addr = trusty_common::read_daemon_addr("trusty-memory")
        .ok()
        .flatten()?;
    let url = format!("http://{addr}/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .ok()?;
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => Some(format!("http://{addr}")),
        _ => None,
    }
}

/// Boot the trusty-memory HTTP daemon in the background.
///
/// Why: a CLI invocation must return control to the shell as soon as the
/// daemon is healthy. Blocking on `run_http_dynamic` (as `serve` historically
/// did) tied the daemon's lifetime to the controlling terminal, which broke
/// shell profiles, tmux panes, and any `make`-driven dev loop. Detaching via
/// self-spawn fixes the lifetime AND keeps the surface identical to
/// `trusty-search start`.
/// What: if `probe_running_daemon` finds a healthy daemon, prints its URL and
/// returns. Otherwise spawns `<this exe> serve --foreground` with stdio
/// redirected to `/dev/null` so the child inherits no terminal, and prints
/// the spawned PID. Does NOT wait for the child to finish binding — callers
/// that need health confirmation should run `trusty-memory doctor` next.
/// Test: `cargo run -p trusty-memory -- start` returns within a second.
pub async fn handle_start() -> Result<()> {
    if let Some(url) = probe_running_daemon().await {
        eprintln!(
            "{} trusty-memory daemon already running at {url}",
            "◉".green()
        );
        return Ok(());
    }

    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not resolve current_exe: {e}"))?;
    let child = std::process::Command::new(&exe)
        .arg("serve")
        .arg("--foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn detached daemon: {e}"))?;
    let pid = child.id();
    eprintln!(
        "{} Daemon starting in background (pid {pid}). Run `trusty-memory doctor` to verify readiness.",
        "✓".green()
    );
    Ok(())
}
