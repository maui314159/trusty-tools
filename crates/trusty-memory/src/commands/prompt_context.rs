//! Handler for `trusty-memory prompt-context`.
//!
//! Why: Claude Code's `UserPromptSubmit` hooks inject any stdout produced by
//! the hook command as additional context for the model. The trusty-memory
//! setup command installs a hook that runs `trusty-memory prompt-context`
//! before every prompt, so the model gets a freshly-rendered list of the
//! palace's hot-predicate facts without paying the per-message MCP tool-call
//! tax. This handler is the actual command the hook invokes.
//!
//! What: a side-effect-only command that talks to the running trusty-memory
//! HTTP daemon and prints the formatted prompt-context block to stdout. It is
//! designed to fail soft on every error path — missing daemon address file,
//! unreachable daemon, slow response — because failing the hook would block
//! every Claude Code prompt the user types.
//!
//! Note on MPM sub-agents: unlike `trusty-mpm hook`, this command is
//! **intentionally NOT** gated on the `CLAUDE_MPM_SUB_AGENT` environment
//! variable. Sub-agents benefit from the parent palace's prompt-fact block
//! just as much as the PM does — withholding it would force every nested
//! agent to rediscover project conventions from scratch. The token cost is
//! a single rendered fact list and the signal payoff (consistent style,
//! vocabulary, and architectural facts across the agent tree) is high. The
//! suppression of nested hook traffic happens at the `trusty-mpm hook`
//! layer instead, where doubled audit events are the real failure mode.
//!
//! Test: daemon-touching paths are exercised manually via `trusty-memory
//! prompt-context` after `trusty-memory start`. The soft-fail-on-missing-
//! daemon branch is covered by `prompt_context_returns_ok_without_daemon`.

use anyhow::Result;
use std::time::Duration;

/// HTTP path served by the trusty-memory daemon that returns the formatted
/// prompt-context block as `text/plain`.
const PROMPT_CONTEXT_PATH: &str = "/api/v1/kg/prompt-context";

/// Connect + total request timeout. Kept short so a slow/dead daemon can
/// never block a Claude Code prompt for more than a couple seconds.
const HTTP_TIMEOUT: Duration = Duration::from_millis(2500);

/// Entry point for `trusty-memory prompt-context`.
///
/// Why: every error path in this handler must result in a clean exit 0 — the
/// `UserPromptSubmit` hook is wired into every Claude Code prompt the user
/// types, so any non-zero exit (or panic) would either block the prompt or
/// inject a confusing error into the model's context. Logging to stderr is
/// fine because Claude Code only ingests stdout from hook commands.
/// What:
///   1. Look up the daemon's bound address via the shared
///      `trusty_common::read_daemon_addr` helper; return `Ok(())` when the
///      file is missing (daemon not running).
///   2. `GET <addr>/api/v1/kg/prompt-context` with a tight timeout; print the
///      body verbatim to stdout on a 2xx, otherwise exit 0.
///
/// Sub-agent behaviour: deliberately unguarded. MPM-spawned sub-agents inject
/// the same prompt-context block as the PM because the marginal token cost
/// is small and the convention/style signal is high — see the module-level
/// note for the full rationale.
/// Test: `prompt_context_returns_ok_without_daemon` covers the no-daemon
/// branch; live-daemon paths are exercised manually after
/// `trusty-memory start`.
pub async fn handle_prompt_context() -> Result<()> {
    // 1. Discover the running daemon. Missing file → daemon not running →
    //    silently exit. A real I/O error here (rare) also degrades to silence.
    let addr = match trusty_common::read_daemon_addr("trusty-memory") {
        Ok(Some(addr)) => addr,
        Ok(None) => return Ok(()),
        Err(_) => return Ok(()),
    };

    // The shared helper persists the bare `host:port`. The web daemon binds
    // HTTP, so prepend the scheme when callers haven't already.
    let base = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr
    } else {
        format!("http://{addr}")
    };
    let url = format!("{base}{PROMPT_CONTEXT_PATH}");

    // 2. Tightly-bounded HTTP call. Any failure → exit 0 silently so the
    //    Claude Code prompt is never blocked by a degraded daemon.
    let client = match reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(HTTP_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    if !resp.status().is_success() {
        return Ok(());
    }
    let body = match resp.text().await {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };
    // Print bare body — Claude Code injects stdout verbatim. No trailing
    // newline manipulation; the daemon's response already carries one.
    print!("{body}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Why: the hook is wired into every Claude Code prompt the user types;
    /// failing it would block the prompt. The contract is that a missing
    /// daemon-address lockfile (the canonical "daemon not running" signal)
    /// must produce `Ok(())` with no stdout, not an error.
    /// What: redirects `trusty_common::resolve_data_dir` at a fresh tempdir
    /// via `TRUSTY_DATA_DIR_OVERRIDE` so `read_daemon_addr("trusty-memory")`
    /// observes a missing lockfile, then runs the handler and asserts it
    /// returns `Ok(())`.
    #[tokio::test]
    async fn prompt_context_returns_ok_without_daemon() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests serialise on `TRUSTY_DATA_DIR_OVERRIDE` by convention
        // across the trusty-* workspace (see trusty-common's lib.rs notes).
        // This test only mutates the env var inside its own scope.
        unsafe {
            std::env::set_var(trusty_common::DATA_DIR_OVERRIDE_ENV, tmp.path());
        }
        let res = handle_prompt_context().await;
        unsafe {
            std::env::remove_var(trusty_common::DATA_DIR_OVERRIDE_ENV);
        }
        assert!(
            res.is_ok(),
            "missing daemon lockfile must degrade to Ok(()), got {res:?}"
        );
    }
}
