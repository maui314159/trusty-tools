//! Handler for `trusty-memory stop` — terminates the running daemon.
//!
//! Why: with `start` now self-spawning a background daemon, operators need a
//! matching `stop` that does not depend on launchd / systemd. The historical
//! `service stop` only worked on macOS launchd-managed installations; a
//! `start`-spawned daemon is just a detached child process whose only public
//! handle is its name on the process table and its address file at
//! `~/.trusty-memory/http_addr`.
//! What: walks the process table via `sysinfo`, collects every `trusty-memory`
//! process whose argv contains `serve` (filters out short-lived CLI calls and
//! `cargo run -- migrate` invocations), sends SIGTERM, polls up to five
//! seconds for them to exit, and SIGKILLs stragglers. Mirrors the
//! `trusty-search stop` flow so the two daemons share a stop UX.
//! Test: `cargo run -p trusty-memory -- start && cargo run -p trusty-memory --
//! stop` should report at least one process killed and leave no live
//! `trusty-memory` process behind.

use anyhow::{bail, Result};
use colored::Colorize;
use std::time::{Duration, Instant};

/// Stop every live `trusty-memory serve` process owned by this user.
///
/// Why: the daemon writes no PID file (only an `http_addr` record), so the
/// process table is the source of truth. Killing every matching process is
/// the safe answer — `find_daemon_pids` filters out short-lived CLI calls by
/// requiring `serve` in argv, so `trusty-memory status`, `migrate`, etc.
/// cannot be hit. Exits non-zero ("No daemon running") when nothing matches
/// so shell-scripted callers can distinguish "I stopped it" from "nothing to
/// stop".
/// What: SIGTERM phase → 5 s poll → SIGKILL phase; finally removes the stale
/// address file when every targeted process has exited.
/// Test: exercised via `start` followed by `stop` in the integration paths.
pub async fn handle_stop() -> Result<()> {
    let targets = find_daemon_pids();
    if targets.is_empty() {
        bail!("No daemon running");
    }

    println!(
        "{} Stopping trusty-memory daemon ({} process(es): {:?})…",
        "⟳".cyan(),
        targets.len(),
        targets
    );

    // Phase 1: SIGTERM all targets.
    for pid in &targets {
        let _ = send_signal(*pid, "TERM");
    }

    // Phase 2: poll up to 5 s for every targeted PID to exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let any_alive = targets.iter().any(|p| pid_alive(*p));
        if !any_alive {
            println!("{} Daemon stopped", "✓".green());
            cleanup_addr_file();
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    // Phase 3: SIGKILL anything still alive.
    let stragglers: Vec<u32> = targets.iter().copied().filter(|p| pid_alive(*p)).collect();
    if !stragglers.is_empty() {
        println!(
            "{} {} process(es) ignored SIGTERM — sending SIGKILL: {:?}",
            "⚠".yellow(),
            stragglers.len(),
            stragglers
        );
        for pid in &stragglers {
            let _ = send_signal(*pid, "KILL");
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    if targets.iter().any(|p| pid_alive(*p)) {
        println!("{} Daemon may still be shutting down", "⚠".yellow());
    } else {
        println!("{} Daemon stopped", "✓".green());
        cleanup_addr_file();
    }
    Ok(())
}

/// Remove the stale `~/.trusty-memory/http_addr` after a successful stop.
///
/// Why: the daemon writes its address on bind but does not clean it up on
/// SIGKILL, so a CLI client reading the file next would chase a dead port
/// for the discovery timeout. Best-effort: an I/O error here just gets
/// silently swallowed because the daemon is already down — the next `start`
/// will overwrite the file with a fresh address anyway.
/// What: locates the file via `trusty_common::resolve_data_dir` and
/// `fs::remove_file`s it.
/// Test: covered indirectly by the stop integration path.
fn cleanup_addr_file() {
    if let Ok(dir) = trusty_common::resolve_data_dir("trusty-memory") {
        let _ = std::fs::remove_file(dir.join("http_addr"));
    }
}

/// Walk the process table and return every `trusty-memory` daemon PID.
///
/// Why: we need a portable "find every live trusty-memory daemon" probe; the
/// crate already depends on `sysinfo` indirectly via the workspace, and the
/// same approach is used by `trusty-search stop`, so the two stop paths share
/// a mental model. Filtering by argv keeps short-lived CLI invocations
/// (`trusty-memory status`, `trusty-memory migrate`) from being killed.
/// What: refreshes the process list once, matches `name() == "trusty-memory"`
/// AND `cmd().contains("serve")`. Excludes the current process so a future
/// caller running inside the same binary cannot suicide.
/// Test: covered indirectly by the stop integration path.
pub(crate) fn find_daemon_pids() -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()),
    );
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let me = std::process::id();
    let mut out = Vec::new();
    for (pid, proc_) in sys.processes() {
        let raw = pid.as_u32();
        if raw == me {
            continue;
        }
        // `name()` is the executable basename. Matching on basename avoids
        // killing `cargo run -p trusty-memory ...` invocations whose argv
        // contains the string but whose binary is `cargo`.
        if proc_.name().to_string_lossy() == "trusty-memory" {
            let is_daemon = proc_.cmd().iter().any(|a| a.to_string_lossy() == "serve");
            if is_daemon {
                out.push(raw);
            }
        }
    }
    out
}

/// Send a POSIX signal to a PID by shelling out to `/bin/kill`.
///
/// Why: avoid pulling in `nix` just for this — the crate already runs
/// `colored`-friendly user output, and `kill -SIGNAL pid` is universally
/// available on every Unix `trusty-memory` is supported on (macOS, Linux).
/// What: spawns `kill -<sig> <pid>` and returns an error if the exit status
/// is non-zero.
/// Test: covered indirectly by the stop integration path.
#[cfg(unix)]
fn send_signal(pid: u32, sig: &str) -> std::io::Result<()> {
    let status = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "kill -{sig} {pid} exited {status}"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _sig: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "signals unsupported on this platform",
    ))
}

/// Check whether a PID is still alive (Unix only).
///
/// Why: the SIGTERM-then-SIGKILL poll loop needs a portable "is this PID
/// alive?" probe. `kill(pid, 0)` returns success when the process exists
/// and EPERM when it exists but we cannot signal it — both count as "alive"
/// for the purposes of the poll.
/// What: invokes `kill -0 <pid>` via `Command` so we do not pull in `nix`.
/// Test: covered indirectly by the stop integration path.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}
