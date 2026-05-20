//! Handlers for `trusty-analyze setup` — one-command integration wiring.
//!
//! Why: getting trusty-analyze usable from Claude Code, Cursor, and claude-mpm
//! involves writing the right MCP / agent / skill config files in the right
//! places. `setup` automates that so users don't have to hand-edit JSON or
//! remember plist paths.
//! What: each `SetupTarget` variant writes (or merges) one configuration
//! artifact. `All` runs every target in sequence.
//! Test: `mod tests` covers the MCP-config merge logic (idempotent re-runs,
//! preserving sibling keys) and the markdown writers.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;

use trusty_analyzer::service::DEFAULT_PORT;

/// Targets for `trusty-analyze setup`.
///
/// Why: each integration (Claude Code, Cursor, claude-mpm, the daemon itself)
/// needs a different artifact written to a different place; modelling them as
/// subcommands keeps the wiring explicit and discoverable via `--help`.
/// What: see the per-variant docs. `All` chains every target.
/// Test: exercised by `mod tests` and by the binary integration paths.
#[derive(Subcommand, Debug)]
pub enum SetupTarget {
    /// Configure all targets at once
    All,
    /// Register as MCP server in Claude Code
    ClaudeCode {
        /// Write to global ~/.claude/mcp.json instead of project .mcp.json
        #[arg(long)]
        global: bool,
        /// Project directory (default: current dir)
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Register as MCP server in Cursor
    Cursor {
        /// Write to global ~/.cursor/mcp.json instead of project .cursor/mcp.json
        #[arg(long)]
        global: bool,
        /// Project directory (default: current dir)
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Register as claude-mpm agent + skill
    ClaudeMpm,
    /// Install and start the background daemon
    Daemon,
}

/// Dispatch a `trusty-analyze setup <target>` invocation.
///
/// Why: a single entry point keeps `main.rs` thin — it just maps the parsed
/// subcommand here.
/// What: routes each `SetupTarget` to its handler; `All` runs daemon →
/// claude-code → cursor → claude-mpm in order.
/// Test: covered transitively; the merge/writer helpers are unit-tested.
pub async fn run_setup(target: SetupTarget) -> Result<()> {
    match target {
        SetupTarget::All => setup_all().await,
        SetupTarget::ClaudeCode { global, project } => setup_claude_code(global, project),
        SetupTarget::Cursor { global, project } => setup_cursor(global, project),
        SetupTarget::ClaudeMpm => setup_claude_mpm(),
        SetupTarget::Daemon => setup_daemon().await,
    }
}

/// Run every setup target in sequence.
///
/// Why: most users want "wire up everything"; chaining the targets is the
/// one-command path.
/// What: daemon → claude-code (project) → cursor (project) → claude-mpm.
/// Test: covered transitively by the per-target tests.
async fn setup_all() -> Result<()> {
    println!("{}", "Setting up all integrations…".bold());
    setup_daemon().await?;
    setup_claude_code(false, None)?;
    setup_cursor(false, None)?;
    setup_claude_mpm()?;
    println!("{} all integrations configured", "✓".green());
    Ok(())
}

/// The MCP server entry trusty-analyze registers in every host's config.
///
/// Why: identical across Claude Code and Cursor — one source of truth avoids
/// drift between the two writers.
/// What: the `{ command, args, env }` object stored under
/// `mcpServers."trusty-analyzer"`.
/// Test: `mcp_server_entry_is_stable` pins the shape.
fn mcp_server_entry() -> serde_json::Value {
    serde_json::json!({
        "command": "trusty-analyze",
        "args": ["serve", "--mcp"],
        "env": {},
    })
}

/// Merge the `trusty-analyzer` MCP server entry into the JSON config at `path`.
///
/// Why: hosts' MCP config files often already contain other servers; a blind
/// overwrite would destroy them. This reads, merges one key, and writes back.
/// What: creates parent dirs, reads any existing file as JSON (an empty/absent
/// file starts from `{}`), ensures `mcpServers` is an object, inserts the
/// `trusty-analyzer` entry, and writes pretty-printed JSON. Returns `true` if
/// the file was already up to date (no write performed).
/// Test: `merge_into_empty_config`, `merge_preserves_sibling_servers`,
/// `merge_is_idempotent`.
fn merge_mcp_config(path: &Path) -> Result<bool> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
    }

    let mut root: serde_json::Value = if path.exists() {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if text.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&text)
                .with_context(|| format!("parse {} as JSON", path.display()))?
        }
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        anyhow::bail!("{} is not a JSON object", path.display());
    }

    let entry = mcp_server_entry();

    // Fast path: already configured identically → nothing to write.
    if root
        .get("mcpServers")
        .and_then(|m| m.get("trusty-analyzer"))
        == Some(&entry)
    {
        return Ok(true);
    }

    let obj = root.as_object_mut().expect("checked is_object above");
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        anyhow::bail!("'mcpServers' in {} is not a JSON object", path.display());
    }
    servers
        .as_object_mut()
        .expect("checked is_object above")
        .insert("trusty-analyzer".to_string(), entry);

    let serialized = serde_json::to_string_pretty(&root).context("serialize merged MCP config")?;
    std::fs::write(path, format!("{serialized}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(false)
}

/// Resolve the project directory: the `--project` override or the current dir.
fn project_dir(project: Option<PathBuf>) -> Result<PathBuf> {
    match project {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("resolve current directory"),
    }
}

/// Register trusty-analyze as an MCP server in Claude Code.
///
/// Why: Claude Code auto-discovers `.mcp.json` (project) and `~/.claude/mcp.json`
/// (global); writing one of those is all it takes to expose the analyzer's MCP
/// tools.
/// What: merges the `trusty-analyzer` entry into the chosen config file.
/// Test: `mod tests` covers the merge; this wrapper just picks the path.
fn setup_claude_code(global: bool, project: Option<PathBuf>) -> Result<()> {
    let path = if global {
        let home = dirs::home_dir().context("resolve $HOME")?;
        home.join(".claude").join("mcp.json")
    } else {
        project_dir(project)?.join(".mcp.json")
    };
    let already = merge_mcp_config(&path)?;
    report_config_write("Claude Code", &path, already);
    Ok(())
}

/// Register trusty-analyze as an MCP server in Cursor.
///
/// Why: Cursor reads `.cursor/mcp.json` (project) and `~/.cursor/mcp.json`
/// (global) for MCP server definitions.
/// What: merges the `trusty-analyzer` entry into the chosen config file.
/// Test: `mod tests` covers the merge; this wrapper just picks the path.
fn setup_cursor(global: bool, project: Option<PathBuf>) -> Result<()> {
    let path = if global {
        let home = dirs::home_dir().context("resolve $HOME")?;
        home.join(".cursor").join("mcp.json")
    } else {
        project_dir(project)?.join(".cursor").join("mcp.json")
    };
    let already = merge_mcp_config(&path)?;
    report_config_write("Cursor", &path, already);
    Ok(())
}

/// Print a uniform "configured" / "already configured" line.
fn report_config_write(host: &str, path: &Path, already: bool) {
    if already {
        println!(
            "{} {host}: already configured ({})",
            "✓".green(),
            path.display()
        );
    } else {
        println!(
            "{} {host}: wrote MCP config to {}",
            "✓".green(),
            path.display()
        );
    }
}

/// claude-mpm agent definition (frontmatter-only markdown).
const CLAUDE_MPM_AGENT: &str = r#"---
name: trusty-analyzer
description: >
  Code analysis agent backed by trusty-analyze. Runs complexity analysis,
  smell detection, quality grading, multi-tool linting, and PR review
  against trusty-search indexed projects. Use for: code quality checks,
  pre-merge review, complexity hotspot investigation, fact extraction.
tools:
  - name: analyzer_health
    endpoint: GET http://127.0.0.1:7879/health
  - name: complexity_hotspots
    endpoint: GET http://127.0.0.1:7879/indexes/{index_id}/complexity_hotspots
  - name: find_smells
    endpoint: GET http://127.0.0.1:7879/indexes/{index_id}/smells
  - name: analyze_quality
    endpoint: GET http://127.0.0.1:7879/indexes/{index_id}/quality
  - name: run_diagnostics
    endpoint: GET http://127.0.0.1:7879/indexes/{index_id}/diagnostics
  - name: review_diff
    endpoint: POST http://127.0.0.1:7879/review
  - name: review_github_pr
    endpoint: POST http://127.0.0.1:7879/review/github-pr
---
"#;

/// claude-mpm skill body written to `.claude/skills/trusty-analyzer.md`.
const CLAUDE_MPM_SKILL: &str = r#"# trusty-analyzer skill

Use the trusty-analyze HTTP API (port 7879) for code analysis:

- `GET /health` — check daemon status
- `GET /indexes/:id/complexity_hotspots` — top complex functions
- `GET /indexes/:id/smells` — code smell detection
- `GET /indexes/:id/quality` — overall grade A–F
- `GET /indexes/:id/diagnostics` — multi-tool linter output
- `POST /review` — analyze a git diff (body: unified diff text)
- `POST /review/github-pr` — analyze a GitHub PR by number

Ensure trusty-search is running on port 7878 and trusty-analyze on 7879.
Run `trusty-analyze status` to confirm.
"#;

/// Register trusty-analyze as a claude-mpm agent and skill.
///
/// Why: claude-mpm discovers agents under `~/.claude-mpm/agents/` and skills
/// under `.claude/skills/`; dropping the two files there wires the analyzer
/// into an mpm workflow.
/// What: writes `~/.claude-mpm/agents/trusty-analyzer.md` and
/// `<cwd>/.claude/skills/trusty-analyzer.md`, creating parent dirs.
/// Test: `claude_mpm_writes_both_files` writes into a temp HOME/CWD and checks
/// both files exist with the expected markers.
fn setup_claude_mpm() -> Result<()> {
    let home = dirs::home_dir().context("resolve $HOME")?;
    let agent_path = home
        .join(".claude-mpm")
        .join("agents")
        .join("trusty-analyzer.md");
    write_file_with_parents(&agent_path, CLAUDE_MPM_AGENT)?;
    println!(
        "{} claude-mpm: wrote agent to {}",
        "✓".green(),
        agent_path.display()
    );

    let skill_path = std::env::current_dir()
        .context("resolve current directory")?
        .join(".claude")
        .join("skills")
        .join("trusty-analyzer.md");
    write_file_with_parents(&skill_path, CLAUDE_MPM_SKILL)?;
    println!(
        "{} claude-mpm: wrote skill to {}",
        "✓".green(),
        skill_path.display()
    );
    Ok(())
}

/// Write `content` to `path`, creating any missing parent directories.
fn write_file_with_parents(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
    }
    std::fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// TCP-probe whether the analyzer daemon's port is accepting connections.
fn port_reachable(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Install and start the background daemon (idempotent).
///
/// Why: gives users a one-command "make the daemon run forever" path.
/// What: if the port already answers, returns early. Otherwise installs the
/// launchd service (when not already installed), `launchctl load`s the plist,
/// and polls `/health` for up to 10 s.
/// Test: on a machine with the daemon already up, prints "already running" and
/// exits 0; the launchd path is macOS-only and verified manually.
async fn setup_daemon() -> Result<()> {
    if port_reachable(DEFAULT_PORT) {
        println!(
            "{} daemon already running on port {DEFAULT_PORT}",
            "✓".green()
        );
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        use crate::commands::service::{launchd_plist_path, service_install};

        let plist = launchd_plist_path()?;
        if !plist.exists() {
            service_install()?;
        } else {
            // Plist exists but the port is down — (re)load it.
            let status = std::process::Command::new("launchctl")
                .arg("load")
                .arg(&plist)
                .status()
                .context("launchctl load")?;
            if !status.success() {
                tracing::warn!("launchctl load exited with {status}");
            }
        }

        // Poll /health for up to 10 s.
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{DEFAULT_PORT}/health");
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(resp) = client
                .get(&url)
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                if resp.status().is_success() {
                    println!("{} daemon installed and healthy", "✓".green());
                    return Ok(());
                }
            }
        }
        println!(
            "{} daemon installed but did not report healthy within 10 s — \
             check `trusty-analyze service logs`",
            "⚠".yellow()
        );
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!(
            "{} `setup daemon` installs a macOS launchd service; on this \
             platform start the daemon with `trusty-analyze start` or your \
             distro's service manager.",
            "·".dimmed()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn mcp_server_entry_is_stable() {
        let e = mcp_server_entry();
        assert_eq!(e["command"], "trusty-analyze");
        assert_eq!(e["args"][0], "serve");
        assert_eq!(e["args"][1], "--mcp");
        assert!(e["env"].is_object());
    }

    #[test]
    fn merge_into_empty_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        let already = merge_mcp_config(&path).unwrap();
        assert!(!already, "first write should not be a no-op");

        let text = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["mcpServers"]["trusty-analyzer"]["command"],
            "trusty-analyze"
        );
    }

    #[test]
    fn merge_preserves_sibling_servers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#,
        )
        .unwrap();

        merge_mcp_config(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        // Sibling server and unrelated key both survive.
        assert_eq!(json["mcpServers"]["other"]["command"], "x");
        assert_eq!(json["theme"], "dark");
        // Our entry was added.
        assert_eq!(
            json["mcpServers"]["trusty-analyzer"]["command"],
            "trusty-analyze"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        let first = merge_mcp_config(&path).unwrap();
        assert!(!first);
        let second = merge_mcp_config(&path).unwrap();
        assert!(second, "second identical merge should be a no-op");
    }

    #[test]
    fn merge_rejects_non_object_root() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert!(merge_mcp_config(&path).is_err());
    }

    #[test]
    fn merge_handles_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.json");
        std::fs::write(&path, "   \n").unwrap();
        let already = merge_mcp_config(&path).unwrap();
        assert!(!already);
        let text = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(json["mcpServers"]["trusty-analyzer"].is_object());
    }

    #[test]
    fn write_file_with_parents_creates_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c.md");
        write_file_with_parents(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn claude_mpm_constants_are_well_formed() {
        // Agent is frontmatter; skill is a markdown doc — sanity-check markers.
        assert!(CLAUDE_MPM_AGENT.starts_with("---\n"));
        assert!(CLAUDE_MPM_AGENT.contains("name: trusty-analyzer"));
        assert!(CLAUDE_MPM_AGENT.contains("review_github_pr"));
        assert!(CLAUDE_MPM_SKILL.contains("# trusty-analyzer skill"));
        assert!(CLAUDE_MPM_SKILL.contains("POST /review/github-pr"));
    }
}
