//! Auto-start the daemon when a CLI command needs it.
//!
//! Why: most CLI subcommands (query, index, status, etc.) silently fail or
//! emit a confusing connection error when the daemon isn't running. This
//! guard probes `/health`; if the daemon is down, it spawns
//! `trusty-search start` in the background and polls `/health` until the
//! daemon is ready (or a 60s budget is exhausted). Users get a single
//! informational line ("Starting trusty-search daemon…") and the command
//! they typed Just Works.
//!
//! What: `ensure_daemon_running(base)` returns `Ok(())` once the daemon is
//! responding to `/health`. Returns `Err(...)` when the spawn fails or the
//! daemon doesn't become ready within the budget.
//!
//! Test: with no daemon running, `cargo run -- list` prints the "Starting…"
//! line, the daemon boots, and the registered indexes are listed. With the
//! daemon already running, no informational line is printed and behaviour is
//! unchanged.
//!
//! Note: only call this from commands that *require* the daemon. Commands
//! like `start`, `stop`, `serve`, `service`, `init`, and `completions`
//! deliberately do not call this guard.

use anyhow::{anyhow, Result};
use colored::Colorize;
use std::io::Write;
use std::time::{Duration, Instant};

/// Total wall-clock budget for the daemon to become ready after we spawn it.
///
/// Why 60s: with the v0.3.12 deferred-embedder-init fix the HTTP port binds
/// in ~1s, so the readiness probe normally returns near-instantly. However if
/// an older daemon binary is running (or any other slow boot path is hit) we
/// want headroom so the user sees a friendlier outcome than a hard timeout.
/// ONNX/CoreML model loading on first run can take 15–30s, and we'd rather
/// wait than fail spuriously.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Polling interval between `/health` probes while we wait. 500ms keeps the
/// spinner feeling responsive without hammering the daemon.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Per-probe HTTP timeout. Short so a hung daemon doesn't blow our budget.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Spinner frames cycled while waiting for `/health` to return 2xx.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Probe `GET {base}/health`. Returns `true` on any 2xx response.
async fn probe_health(base: &str) -> bool {
    // Build a lightweight client per probe so we don't share connection pool
    // state between an unhealthy run and the next probe. Probes are infrequent
    // (every 500ms) so the cost is negligible.
    let client = match reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .connect_timeout(PROBE_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(format!("{}/health", base)).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Spawn `trusty-search start --foreground` as a detached background process.
///
/// Why: we want the daemon to outlive this CLI invocation. We use the
/// currently-running executable so a `cargo run` debugging session boots its
/// own debug daemon and a production install boots the production binary.
///
/// We pass `--foreground` to the spawned child so it runs `run_daemon` inline
/// rather than recursively self-spawning again (which would fork-bomb on every
/// `trusty-search start` invocation). The child process is fully detached from
/// the parent terminal via `Stdio::null()` for all three fds — this is what
/// prevents SIGHUP from a closing tmux pane / terminal from killing the
/// daemon.
//
// Retained as the device-agnostic convenience wrapper over
// `spawn_daemon_with_device`; all current call sites pass an explicit device
// (issue #24) so this is presently unused, but it is the stable entry point
// for callers that have no device preference.
#[allow(dead_code)]
pub(crate) fn spawn_daemon() -> Result<u32> {
    spawn_daemon_with_device(None)
}

/// Spawn `trusty-search start --foreground` as a detached background process,
/// optionally forcing a specific execution-provider device.
///
/// Why (issue #24): on Apple Silicon, CoreML EP session-init alone allocates
/// from the unified memory pool and inflates virtual RSS to ~72 GB before any
/// inference runs. macOS jetsam SIGKILLs the daemon within ~14s during the
/// `index --force` flow, before any files are processed. Auto-spawning the
/// daemon with `--device cpu` (or `TRUSTY_INDEX_DEVICE=cpu` honoured by the
/// caller) sidesteps CoreML init entirely for the indexing path. Operators who
/// want CoreML for query-time embeddings can still run
/// `trusty-search start --device auto` (or `gpu`) manually.
/// What: invokes `<exe> start --foreground` and, when `device` is `Some`,
/// appends `--device <device>` so the child's `handle_start` translates it to
/// `TRUSTY_DEVICE=<device>` for `trusty-embedder`. Stdio is fully detached.
/// Test: `cargo check -p trusty-search` plus
/// `trusty-search index --force` no longer SIGKILLs on M-series.
pub(crate) fn spawn_daemon_with_device(device: Option<&str>) -> Result<u32> {
    let exe = std::env::current_exe().map_err(|e| anyhow!("could not resolve current_exe: {e}"))?;
    // Detach stdio — we don't want the daemon's logs streaming into the
    // user's terminal session while they're waiting on a `query` result,
    // and we need the daemon to survive the parent shell closing.
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("start").arg("--foreground");
    if let Some(dev) = device {
        cmd.arg("--device").arg(dev);
    }
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            anyhow!(
                "could not spawn `{} start --foreground`: {e}",
                exe.display()
            )
        })?;
    Ok(child.id())
}

/// Ensure the daemon at `base` is running and ready. Spawns `trusty-search
/// start` (only when no daemon process is already running) and polls
/// `/health` for up to `READY_TIMEOUT` if not.
///
/// On success (already-running case), prints nothing and returns `Ok(())`
/// quickly. On the auto-start path, prints a single line + spinner to stderr
/// so stdout stays clean for tools that pipe JSON.
///
/// Why (v0.3.12): previously this unconditionally spawned a daemon when the
/// initial `/health` probe returned false. If a daemon process existed but
/// hadn't bound its HTTP port yet (e.g. mid-boot while loading the embedder),
/// the spawn would print "already running (pid …)" to stderr and exit, then
/// the poll would still wait up to the full timeout. Now we check the PID
/// lockfile first: if a daemon is already running, skip the spawn entirely
/// and go straight to polling — we just need to wait for `/health` to flip.
pub async fn ensure_daemon_running(base: &str) -> Result<()> {
    ensure_daemon_running_with_device(base, None).await
}

/// Like `ensure_daemon_running` but passes `--device <device>` to the spawned
/// daemon when an auto-spawn is performed.
///
/// Why (issue #24): the `index --force` flow on Apple Silicon was killed by
/// macOS jetsam before any indexing happened, because CoreML EP init alone
/// inflated virtual RSS to ~72 GB. Forcing CPU for the spawned daemon avoids
/// the spike entirely. An already-running daemon's device is left untouched —
/// the caller can decide what to do (we currently just attach to it).
/// What: when the daemon is auto-spawned, propagates `device` to
/// `spawn_daemon_with_device`. `device=None` preserves the legacy auto-detect
/// behaviour (so unrelated commands like `query` and `status` still get GPU
/// acceleration on M-series).
/// Test: covered indirectly by `cargo check -p trusty-search` and by the
/// `index --force` no-OOM behavioural test on M-series.
pub async fn ensure_daemon_running_with_device(base: &str, device: Option<&str>) -> Result<()> {
    // Fast path: already up.
    if probe_health(base).await {
        return Ok(());
    }

    // Detect existing daemon process before spawning a duplicate. If a
    // daemon is already running but `/health` hasn't responded yet (e.g.
    // because the embedder is still loading and the HTTP listener hasn't
    // bound — only relevant for pre-v0.3.12 binaries, but the check is
    // cheap regardless), skip the spawn and just wait for it to come up.
    let already_running = crate::service::running_daemon_pid().is_some();

    if already_running {
        eprintln!(
            "{} trusty-search daemon already running, waiting for it to become ready…",
            "◉".cyan()
        );
    } else {
        match device {
            Some(dev) => eprintln!(
                "{} Starting trusty-search daemon (--device {dev})…",
                "◉".cyan()
            ),
            None => eprintln!("{} Starting trusty-search daemon…", "◉".cyan()),
        }
        spawn_daemon_with_device(device)?;
    }

    let deadline = Instant::now() + READY_TIMEOUT;
    let start = Instant::now();
    let mut frame = 0usize;
    loop {
        // Render a spinner so the user knows we're still waiting (ONNX/CoreML
        // model load can take 15-30s on first run; without feedback users
        // assume the daemon hung).
        let elapsed = start.elapsed().as_secs();
        let glyph = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
        eprint!(
            "\r{} Waiting for daemon to become ready… ({}s) ",
            glyph.cyan(),
            elapsed
        );
        let _ = std::io::stderr().flush();
        frame = frame.wrapping_add(1);

        tokio::time::sleep(POLL_INTERVAL).await;
        if probe_health(base).await {
            // Erase the spinner line so subsequent output starts fresh.
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
            eprintln!(
                "{} Daemon ready ({}s)",
                "✓".green(),
                start.elapsed().as_secs()
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Erase the spinner line before the error message.
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
            return Err(anyhow!(
                "daemon did not become ready within {}s at {} — \
                 try `trusty-search start` manually to see the error",
                READY_TIMEOUT.as_secs(),
                base
            ));
        }
    }
}

/// Convenience wrapper used by command handlers: returns a contextualized
/// error on failure. Returns `Ok(())` on success.
///
/// Why: every caller of `ensure_daemon_running` would otherwise duplicate the
/// error-handling boilerplate. Returns `Result` (not `process::exit`) so command
/// handlers stay testable — the central `main()` dispatcher prints the friendly
/// error and chooses the exit code.
pub async fn ensure_daemon_running_or_exit(base: &str) -> Result<()> {
    ensure_daemon_running(base).await
}

/// Variant of `ensure_daemon_running_or_exit` that prefers CPU EP for an
/// auto-spawned daemon during the indexing flow.
///
/// Why (issue #24): the indexing path is the load-bearing OOM site on Apple
/// Silicon — CoreML EP init alone allocates ~72 GB of virtual RSS during
/// `FastEmbedder::new()`, and macOS jetsam SIGKILLs the daemon ~14s in,
/// before any chunks are processed. Forcing `--device cpu` on auto-spawn from
/// the `index`/`add` commands eliminates the CoreML init spike entirely.
/// Operators who need CoreML for query-time embeddings can start the daemon
/// manually with `trusty-search start --device auto` first; this helper only
/// affects the auto-spawn case.
/// What: resolves the desired device from `TRUSTY_INDEX_DEVICE` (override) or
/// defaults to `"cpu"`. Already-running daemons are left untouched. Passing
/// the explicit value `auto` (case-insensitive) restores legacy behaviour.
/// Test: `cargo test -p trusty-search` (compile) + manual `trusty-search index
/// --force` on M-series no longer SIGKILL.
pub async fn ensure_daemon_running_for_indexing(base: &str) -> Result<()> {
    let device = resolve_indexing_device();
    let device_opt = if device.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(device.as_str())
    };
    ensure_daemon_running_with_device(base, device_opt).await
}

/// Resolve the auto-spawn device for the indexing flow.
///
/// Why: keep the env-var contract in one place so tests and docs match
/// behaviour. Reads `TRUSTY_INDEX_DEVICE` (`cpu` | `gpu` | `auto`); defaults
/// to `auto` as of trusty-search 0.3.55. The original blocking OOM (issue
/// #24) — CoreML EP allocating ~72 GB virtual RSS from the unified-memory
/// GPU pool — was rooted in `MLComputeUnits=ALL`. The shared
/// `trusty-embedder` now defaults to `MLComputeUnits=CPUAndNeuralEngine`,
/// which uses the Neural Engine's dedicated memory rather than the GPU
/// unified-memory pool. That keeps virtual RSS bounded while restoring the
/// ~10× throughput advantage over CPU-only indexing.
///
/// Operators who hit edge cases (or want to A/B benchmark) can still force
/// CPU with `TRUSTY_INDEX_DEVICE=cpu`. Setting `TRUSTY_COREML_COMPUTE_UNITS=all`
/// re-enables the old CPU+GPU+ANE pipeline at the cost of the original
/// memory behaviour.
/// What: returns a lowercased owned `String` so the caller can match on it
/// or pass it directly into `--device`.
/// Test: `resolve_indexing_device_defaults_to_auto` and
/// `resolve_indexing_device_honours_env_override`.
fn resolve_indexing_device() -> String {
    match std::env::var("TRUSTY_INDEX_DEVICE") {
        Ok(v) if !v.is_empty() => v.to_ascii_lowercase(),
        _ => "auto".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `probe_health` against an unbound localhost port returns `false`
    /// (connect refused) within a reasonable deadline.
    #[tokio::test]
    async fn probe_health_returns_false_on_connection_refused() {
        // Port 1 is reserved and never bound by any normal process — pick a
        // high-numbered port we believe is free. Using 65535 minimises the
        // chance of collision with a real service.
        let base = "http://127.0.0.1:65535";
        let started = Instant::now();
        let ok = probe_health(base).await;
        assert!(!ok, "probe should fail against an unbound port");
        // The probe must return false within a generous bound. macOS may delay
        // the RST on filtered ports so we allow up to 6 s (PROBE_TIMEOUT ×
        // reqwest internal retry overhead) rather than asserting near-instant
        // behaviour that is OS / firewall dependent.
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "probe took too long: {:?}",
            started.elapsed()
        );
    }

    /// `probe_health` against a malformed URL returns `false` cleanly (no panic).
    #[tokio::test]
    async fn probe_health_returns_false_on_bad_url() {
        let ok = probe_health("not-a-valid-url").await;
        assert!(!ok);
    }

    /// `probe_health` returns false for a locally unreachable port within a
    /// generous wall-clock bound.
    #[tokio::test]
    async fn probe_health_respects_short_timeout() {
        // Use a host:port that's locally unreachable.
        let started = Instant::now();
        let _ = probe_health("http://127.0.0.1:1").await;
        // macOS may buffer the TCP RST rather than immediately refusing — allow
        // up to 6 s so the test isn't flaky on filtered-port kernel paths while
        // still catching genuine hangs (e.g. PROBE_TIMEOUT being ignored).
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "probe took too long: {:?}",
            started.elapsed()
        );
    }

    /// Why: as of trusty-search 0.3.55 the indexing flow defaults to `auto`
    /// because the embedder now registers CoreML with
    /// `MLComputeUnits=CPUAndNeuralEngine`, which eliminates the GPU
    /// unified-memory allocation that caused the original 72 GB virtual-RSS
    /// spike (issue #24). The Neural Engine uses dedicated memory, so the
    /// jetsam SIGKILL no longer fires. Operators who want to A/B benchmark
    /// can still force CPU with `TRUSTY_INDEX_DEVICE=cpu`.
    /// What: clears `TRUSTY_INDEX_DEVICE`, calls `resolve_indexing_device`,
    /// asserts it returns `"auto"`.
    /// Test: this test.
    // Shared env lock for all TRUSTY_INDEX_DEVICE tests in this module.
    // Why: separate per-test statics let parallel tests race on the same env
    // var, producing flaky "cpu vs gpu" assertion failures.
    use std::sync::Mutex;
    static INDEX_DEVICE_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_indexing_device_defaults_to_auto() {
        let _guard = INDEX_DEVICE_ENV_LOCK.lock().unwrap();
        let prev = std::env::var("TRUSTY_INDEX_DEVICE").ok();
        // SAFETY: single-threaded under ENV_LOCK.
        unsafe { std::env::remove_var("TRUSTY_INDEX_DEVICE") };

        assert_eq!(resolve_indexing_device(), "auto");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("TRUSTY_INDEX_DEVICE", v),
                None => std::env::remove_var("TRUSTY_INDEX_DEVICE"),
            }
        }
    }

    /// Why: operators with enough headroom (or on non-Apple platforms) may
    /// want GPU during indexing for throughput. `TRUSTY_INDEX_DEVICE` is the
    /// documented escape hatch — if it stops being honoured, the documented
    /// contract breaks silently.
    /// What: sets `TRUSTY_INDEX_DEVICE=gpu` (then `auto`) and asserts
    /// `resolve_indexing_device` echoes the lowercased value.
    /// Test: this test.
    #[test]
    fn resolve_indexing_device_honours_env_override() {
        let _guard = INDEX_DEVICE_ENV_LOCK.lock().unwrap();
        let prev = std::env::var("TRUSTY_INDEX_DEVICE").ok();

        // SAFETY: single-threaded under ENV_LOCK.
        unsafe { std::env::set_var("TRUSTY_INDEX_DEVICE", "GPU") };
        assert_eq!(resolve_indexing_device(), "gpu");

        unsafe { std::env::set_var("TRUSTY_INDEX_DEVICE", "auto") };
        assert_eq!(resolve_indexing_device(), "auto");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("TRUSTY_INDEX_DEVICE", v),
                None => std::env::remove_var("TRUSTY_INDEX_DEVICE"),
            }
        }
    }
}
