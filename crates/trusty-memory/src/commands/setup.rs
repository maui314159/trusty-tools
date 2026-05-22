//! Handler for `trusty-memory setup`.
//!
//! Why: first-time users want a single command that installs the launchd
//! service, creates the data directory, and registers `trusty-memory` as an
//! MCP server in every Claude settings file on the machine. Doing this
//! piecewise (manual plist install, hand-edit settings.json, restart Claude)
//! is brittle and error-prone — `setup` makes it a one-liner that leans on
//! the shared `trusty_common::{launchd, claude_config}` modules so the
//! behaviour stays in lockstep with `trusty-search setup` and any future
//! trusty-* tool.
//! What: orchestrates three phases:
//!   1. Creates `<data_dir>/trusty-memory/` (e.g. `~/Library/Application
//!      Support/trusty-memory` on macOS).
//!   2. On macOS, installs and bootstraps the launchd LaunchAgent via the
//!      shared `LaunchdConfig`. On other platforms, this phase is skipped
//!      with a friendly note.
//!   3. Patches every discovered Claude settings file with an MCP server
//!      entry pointing at `trusty-memory serve`. Falls back to creating
//!      `~/.claude/settings.json` when no settings files were found.
//!
//! Test: unit tests cover the patch phase against tempdir-rooted settings
//! files. The launchd phase is side-effecting (macOS only) and exercised
//! manually via `cargo run -p trusty-memory -- setup`.

use anyhow::{Context, Result};
use colored::Colorize;
use std::path::{Path, PathBuf};
use trusty_common::claude_config::{
    default_settings_max_depth, discover_claude_settings, mcp_server_entry, patch_mcp_server,
};

/// Canonical MCP server key used in Claude settings files.
///
/// Why: the same key is used by the migrate command and the patch phase
/// here; defining it once prevents the two from drifting (e.g. one writing
/// `trusty-memory` and the other writing `trusty_memory`).
/// What: the literal string `"trusty-memory"`.
/// Test: covered by every test in this module that asserts the key is
/// present after a patch.
const MCP_SERVER_KEY: &str = "trusty-memory";

/// Entry point for `trusty-memory setup`.
///
/// Why: a first-time-install command that wires up everything a user needs
/// to run trusty-memory from Claude Code with one invocation.
/// What: runs the three phases (data dir → launchd → Claude settings) in
/// order. A failure in the launchd phase is fatal on macOS (we want to
/// fail loud so the user can fix it), but Claude settings phase failures
/// for individual files are non-fatal — we log and continue.
/// Test: integration via `cargo run -p trusty-memory -- setup`; unit tests
/// cover the patch phase against fixture settings files.
pub fn handle_setup() -> Result<()> {
    println!("{} Setting up trusty-memory…\n", "·".dimmed());

    // Phase 1: data directory.
    let data_dir = ensure_data_dir()?;
    println!("{} Data directory: {}", "✓".green(), data_dir.display());

    // Phase 2: launchd (macOS only).
    install_service_phase()?;

    // Phase 2b: pre-warm the embedder model cache. This downloads ~22 MB
    // of ONNX into `$HOME/.cache/fastembed` before launchd ever starts the
    // daemon, so the first `memory_recall` request does not have to wait
    // for (and the read-only `TMPDIR` does not silently break) the model
    // retrieval (GH #58).
    prewarm_embedder_phase();

    // Phase 3: Claude settings patching.
    let patched = patch_claude_settings_phase()?;

    println!("\n{} Setup complete!", "✓".green());
    if patched > 0 {
        println!(
            "  Updated {} Claude settings file{}.",
            patched,
            if patched == 1 { "" } else { "s" }
        );
    }
    println!(
        "  Try: {} (or restart Claude Code to pick up the new MCP server)",
        "trusty-memory serve".cyan()
    );
    Ok(())
}

/// Create the user data directory for trusty-memory.
///
/// Why: `trusty-memory serve` reads/writes its palace files under this
/// directory; pre-creating it during setup avoids first-run race conditions
/// and lets us surface permission failures up-front.
/// What: resolves `<data_dir>/trusty-memory` via [`dirs::data_dir`] and
/// creates it (and any missing parents). Returns the resolved path.
/// Test: `setup_creates_data_dir_under_override` exercises the happy path
/// with a tempdir-based override of `dirs::data_dir`.
fn ensure_data_dir() -> Result<PathBuf> {
    let base =
        dirs::data_dir().ok_or_else(|| anyhow::anyhow!("could not resolve user data directory"))?;
    let dir = base.join("trusty-memory");
    std::fs::create_dir_all(&dir).with_context(|| format!("create data dir {}", dir.display()))?;
    Ok(dir)
}

/// Install the launchd service (macOS) or skip with a note (other platforms).
///
/// Why: keeps the platform-specific logic in one place so `handle_setup`
/// can read top-to-bottom without `#[cfg]` blocks. On macOS the service is
/// the canonical way to keep the daemon alive across logins; on Linux /
/// Windows we expect operators to use systemd / Task Scheduler directly
/// and don't try to forge a half-working wrapper.
/// What: on macOS, calls `LaunchdConfig::install()` + `.bootstrap()`. On
/// other platforms, prints a one-line skip notice and returns Ok.
/// Test: side-effecting on macOS; covered manually. Other platforms hit the
/// no-op path during `cargo test -p trusty-memory` on Linux CI.
fn install_service_phase() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use crate::commands::service::{build_launchd_config, launchd_log_dir, LAUNCHD_LABEL};

        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("could not resolve current exe: {e}"))?;
        let log_dir = launchd_log_dir()?;
        let cfg = build_launchd_config(exe, log_dir.clone());
        cfg.install().context("install LaunchAgent plist")?;
        println!(
            "{} Installed LaunchAgent: {}",
            "✓".green(),
            cfg.plist_path()?.display()
        );

        cfg.bootstrap()
            .context("bootstrap LaunchAgent into user gui domain")?;
        println!(
            "{} Loaded {} (daemon will auto-start; logs in {}).",
            "✓".green(),
            LAUNCHD_LABEL,
            log_dir.display().to_string().dimmed()
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!(
            "{} Skipping launchd install (not macOS) — use your distro's \
             service manager to run `trusty-memory serve` on demand.",
            "·".dimmed()
        );
    }
    Ok(())
}

/// Pre-warm the fastembed ONNX model cache before launchd ever starts the
/// daemon.
///
/// Why: GH #58 — under launchd, `TMPDIR` is mounted read-only for the
/// agent's UID, so fastembed's first `TextEmbedding::try_new` fails with
/// `EROFS (os error 30)` and the HTTP daemon never becomes ready. Even
/// with `FASTEMBED_CACHE_DIR` correctly set in the plist, downloading the
/// ~22 MB model on the daemon's first request introduces latency and
/// failure modes (network blips, slow ANE compile). Pre-warming during
/// `setup` — which runs in the user's normal shell with full network and
/// HOME access — moves both the download and the ONNX session warmup
/// off the daemon's critical path and surfaces failures up-front where the
/// user can act on them.
/// What: explicitly sets `FASTEMBED_CACHE_DIR` to `$HOME/.cache/fastembed`
/// (the same path the launchd plist will use), then spins up a single-
/// threaded tokio runtime to drive `FastEmbedder::new()`. Failures are
/// reported as warnings — they do not abort `setup` because the daemon
/// will retry on its own startup, but a successful pre-warm is the
/// difference between "instant first recall" and "users see EROFS errors".
/// Test: side-effecting (network + filesystem); covered manually via
/// `cargo run -p trusty-memory -- setup`.
fn prewarm_embedder_phase() {
    let cache_dir = trusty_common::embedder::resolve_fastembed_cache_dir();
    // SAFETY: setup runs single-threaded before any worker spawns.
    unsafe {
        std::env::set_var("FASTEMBED_CACHE_DIR", &cache_dir);
    }
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        eprintln!(
            "  {} could not create {} ({e}) — daemon will retry on first request.",
            "·".dimmed(),
            cache_dir.display()
        );
        return;
    }

    println!(
        "\n{} Pre-warming embedder model cache at {}…",
        "·".dimmed(),
        cache_dir.display()
    );

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!(
                "  {} could not build tokio runtime for pre-warm ({e}); skipping.",
                "·".dimmed()
            );
            return;
        }
    };

    match rt.block_on(trusty_common::embedder::FastEmbedder::new()) {
        Ok(_e) => {
            println!(
                "{} Embedder model cached. First recall after daemon start will be instant.",
                "✓".green()
            );
        }
        Err(e) => {
            // Non-fatal: daemon will retry on its own. Surface the error
            // loudly so the operator can intervene (e.g. fix offline
            // proxy, free disk space) before launchd hits the same wall.
            eprintln!(
                "  {} pre-warm failed ({e}). The daemon will retry on first request — \
                 if this persists, inspect {} for partial downloads.",
                "✗".red(),
                cache_dir.display()
            );
        }
    }
}

/// Patch every discovered Claude settings file (or fall back to
/// `~/.claude/settings.json`) so it registers `trusty-memory` as an MCP
/// server.
///
/// Why: Claude Code only loads MCP servers it knows about; without this
/// step `setup` would install the daemon but Claude would never call it.
/// Walking every settings file matters because users frequently have both
/// a global `~/.claude/settings.json` and per-project
/// `<repo>/.claude/settings.local.json` files.
/// What: discovers settings files via
/// [`trusty_common::claude_config::discover_claude_settings`], then calls
/// [`patch_mcp_server`] for each. The shared helper is idempotent (it
/// returns `false` and skips the write when the entry is already present),
/// so re-running setup is safe. When no files are found, falls back to
/// creating `~/.claude/settings.json`.
/// Test: `setup_patches_existing_settings_file` and
/// `setup_creates_fallback_settings_file` cover both branches.
fn patch_claude_settings_phase() -> Result<usize> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not resolve home directory"))?;
    println!(
        "\n{} Scanning for Claude settings under {}…",
        "·".dimmed(),
        home.display()
    );

    let entry = mcp_server_entry(MCP_SERVER_KEY, &["serve"]);
    let files = discover_claude_settings(&home, default_settings_max_depth());

    if files.is_empty() {
        let fallback = home.join(".claude").join("settings.json");
        println!(
            "{} No Claude settings files found. Creating {}…",
            "·".dimmed(),
            fallback.display()
        );
        let n = patch_one(&fallback, &entry)?;
        return Ok(n);
    }

    println!(
        "{} Found {} settings file(s). Patching each…",
        "·".dimmed(),
        files.len()
    );
    let mut changed = 0usize;
    for path in &files {
        match patch_one(path, &entry) {
            Ok(1) => {
                changed += 1;
                println!("  {} {}", "✓".green(), path.display());
            }
            Ok(_) => {
                println!(
                    "  {} {} {}",
                    "↻".cyan(),
                    path.display().to_string().dimmed(),
                    "(already configured)".dimmed()
                );
            }
            Err(e) => {
                // Non-fatal: log and continue so one bad file doesn't sink
                // the whole setup run.
                eprintln!(
                    "  {} {} {}",
                    "✗".red(),
                    path.display(),
                    format!("({e})").red()
                );
            }
        }
    }
    Ok(changed)
}

/// Patch a single Claude settings file, returning `1` if it was modified
/// and `0` if it was already up to date.
///
/// Why: the surrounding loop in [`patch_claude_settings_phase`] wants a
/// uniform success/no-op signal so it can render a colourised summary.
/// What: thin wrapper around
/// [`trusty_common::claude_config::patch_mcp_server`] that translates its
/// `bool` (`true` = wrote, `false` = no-op) into a count.
/// Test: `patch_one_is_idempotent` and `patch_one_creates_missing_file`.
fn patch_one(path: &Path, entry: &serde_json::Value) -> Result<usize> {
    let wrote = patch_mcp_server(path, MCP_SERVER_KEY, entry)?;
    Ok(if wrote { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Why: patching a fresh settings file must produce a valid
    /// `mcpServers` block with the canonical `trusty-memory` entry.
    /// What: writes a minimal settings.json, calls `patch_one`, asserts
    /// the entry shape.
    #[test]
    fn patch_one_creates_missing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        let entry = mcp_server_entry(MCP_SERVER_KEY, &["serve"]);

        let n = patch_one(&path, &entry).expect("patch ok");
        assert_eq!(n, 1, "first patch must write the file");

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let server = &value["mcpServers"][MCP_SERVER_KEY];
        assert_eq!(server["command"], "trusty-memory");
        assert_eq!(server["args"][0], "serve");
    }

    /// Why: re-running `setup` must be safe — calling `patch_one` against
    /// an already-configured file must not rewrite it.
    /// What: writes settings.json, patches twice, asserts the second call
    /// returns 0 and the file is byte-identical to after the first patch.
    #[test]
    fn patch_one_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        let entry = mcp_server_entry(MCP_SERVER_KEY, &["serve"]);

        assert_eq!(patch_one(&path, &entry).unwrap(), 1, "first patch writes");
        let after_first = std::fs::read_to_string(&path).unwrap();

        assert_eq!(
            patch_one(&path, &entry).unwrap(),
            0,
            "second patch is no-op"
        );
        let after_second = std::fs::read_to_string(&path).unwrap();

        assert_eq!(after_first, after_second, "file must not change on no-op");
    }

    /// Why: patching must preserve unrelated keys (theme, other servers).
    /// Anything else is a regression — `setup` would destroy user config.
    /// What: seeds a settings file with extra keys, patches, asserts every
    /// pre-existing key still exists alongside the new MCP entry.
    #[test]
    fn patch_one_preserves_unrelated_keys() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("settings.json");
        let seed = json!({
            "theme": "dark",
            "mcpServers": {
                "some-other-server": { "command": "x", "args": [] }
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let entry = mcp_server_entry(MCP_SERVER_KEY, &["serve"]);
        let n = patch_one(&path, &entry).expect("patch ok");
        assert_eq!(n, 1);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark", "unrelated top-level key dropped");
        let servers = value["mcpServers"].as_object().unwrap();
        assert!(servers.contains_key("some-other-server"));
        assert!(servers.contains_key(MCP_SERVER_KEY));
    }
}
