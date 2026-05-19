//! /tm REPL slash-command dispatcher.
//!
//! Why: Issue #316 — give users a single `/tm <subcommand>` interface from
//! the REPL to inspect, create, and control tmux-managed sessions without
//! leaving the chat. Centralizing the dispatcher here keeps `repl/mod.rs`
//! focused on UI plumbing and lets the same code be reused by tests / other
//! frontends.
//! What: `handle_tm_command` parses the first whitespace-delimited token as a
//! subcommand name and routes to the corresponding `cmd_*` helper, each of
//! which writes user-facing output into a shared `String` buffer. All
//! subcommands return `Result<()>`; helper formatting errors are bubbled up
//! via `?`.
//! Test: `tests` module below covers help + unknown-command rendering and
//! the parse paths that don't require a live tmux server. Live-tmux flows
//! are exercised by the integration tests.

use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::tm::manager::TmManager;
use crate::tm::project::{AdapterType, SessionStatus, TmSession};

/// Dispatch a `/tm <subcommand> [args]` invocation.
///
/// Why: Single entry point so the REPL only has to forward the raw arg
/// string; this function owns the parse of the first token + delegation.
/// What: Splits `args` on the first whitespace, matches the subcommand name,
/// and calls the matching helper. Unknown subcommands produce a friendly
/// hint pointing at `/tm help`.
/// Test: `tests::dispatch_help` and `tests::dispatch_unknown` below.
pub async fn handle_tm_command(
    manager: &Arc<Mutex<TmManager>>,
    args: &str,
    out: &mut String,
) -> Result<()> {
    let args = args.trim();
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();

    match sub {
        "list" | "ls" => cmd_list(manager, out).await,
        "new" => cmd_new(manager, rest, out).await,
        "attach" | "a" => cmd_attach(manager, rest, out).await,
        "pause" => cmd_pause(manager, rest, out).await,
        "resume" => cmd_resume(manager, rest, out).await,
        "kill" => cmd_kill(manager, rest, out).await,
        "send" => cmd_send(manager, rest, out).await,
        "capture" | "cap" => cmd_capture(manager, rest, out).await,
        "detect" => cmd_detect(manager, rest, out).await,
        "reconcile" | "sync" => cmd_reconcile(manager, out).await,
        "status" | "st" => cmd_status(manager, rest, out).await,
        "projects" | "proj" => cmd_projects(manager, out).await,
        "help" | "" => {
            write_tm_help(out);
            Ok(())
        }
        _ => {
            writeln!(out, "Unknown /tm subcommand: '{sub}'. Try /tm help")?;
            Ok(())
        }
    }
}

// ==================== /tm help ====================

/// Write the static help text into `out`.
///
/// Why: Centralizing the help text avoids drift between `/help` summary and
/// `/tm help`'s detail page.
/// What: One usage line per subcommand.
/// Test: `tests::dispatch_help` asserts the buffer is non-empty and lists
/// all the verbs.
pub fn write_tm_help(out: &mut String) {
    let _ = writeln!(
        out,
        "/tm commands:
  /tm list                                 List all TM sessions
  /tm new [name] [-p path] [-a adapter]    Create new session
  /tm attach <name>                        Show attach command
  /tm pause <name>                         Pause session (adapter-dependent)
  /tm resume <name>                        Resume session (adapter-dependent)
  /tm kill <name>                          Kill tmux session
  /tm send <name> <message>                Send message to session
  /tm capture <name> [lines]               Show pane output (default 50)
  /tm detect <name>                        Auto-detect adapter type
  /tm reconcile                            Sync registry with live tmux
  /tm status [name]                        Session detail or summary
  /tm projects                             List projects with frameworks
  /tm help                                 Show this help"
    );
}

// ==================== /tm list ====================

async fn cmd_list(manager: &Arc<Mutex<TmManager>>, out: &mut String) -> Result<()> {
    let mgr = manager.lock().await;
    let sessions = mgr.list_sessions().await?;
    let projects = mgr.registry.list_projects()?;
    drop(mgr);

    render_session_list(&sessions, &projects, out)
}

fn render_session_list(
    sessions: &[TmSession],
    projects: &[crate::tm::project::TmProject],
    out: &mut String,
) -> Result<()> {
    // Filter out Orphaned sessions for the default view; they are pruned on
    // reconcile but may still appear in the list between reconcile passes.
    let visible: Vec<&TmSession> = sessions
        .iter()
        .filter(|s| s.status != SessionStatus::Orphaned)
        .collect();
    let hidden_count = sessions.len() - visible.len();

    writeln!(out, "TM Sessions")?;
    writeln!(
        out,
        "──────────────────────────────────────────────────────────"
    )?;
    if visible.is_empty() {
        writeln!(out, "  (no sessions)")?;
        writeln!(
            out,
            "──────────────────────────────────────────────────────────"
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "  {:<16} {:<14} {:<13} {:<10} LAST ACTIVE",
        "PROJECT", "NAME", "ADAPTER", "STATUS"
    )?;

    let mut running = 0usize;
    let mut paused = 0usize;
    let mut idle = 0usize;
    for s in &visible {
        let project_name = projects
            .iter()
            .find(|p| p.id == s.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("(unregistered)");
        match s.status {
            SessionStatus::Running => running += 1,
            SessionStatus::Paused => paused += 1,
            SessionStatus::Idle => idle += 1,
            _ => {}
        }
        writeln!(
            out,
            "  {:<16} {:<14} {:<13} {:<10} {}",
            truncate(project_name, 16),
            truncate(&s.name, 14),
            truncate(s.adapter_type.as_str(), 13),
            s.status.to_string(),
            s.last_active_ago()
        )?;
    }
    writeln!(
        out,
        "──────────────────────────────────────────────────────────"
    )?;
    let unique_projects: std::collections::HashSet<&String> =
        visible.iter().map(|s| &s.project_id).collect();
    writeln!(
        out,
        "{} session{} across {} project{}  ({} running, {} paused, {} idle)",
        visible.len(),
        if visible.len() == 1 { "" } else { "s" },
        unique_projects.len(),
        if unique_projects.len() == 1 { "" } else { "s" },
        running,
        paused,
        idle
    )?;
    if hidden_count > 0 {
        writeln!(
            out,
            "({hidden_count} orphaned session{} hidden; run /tm reconcile to clean up)",
            if hidden_count == 1 { "" } else { "s" }
        )?;
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ==================== /tm new ====================

/// Parse `/tm new [name] [-p <path>] [-a <adapter>]`.
///
/// Why: A small, predictable arg parser lets users mix positional name with
/// optional flags without pulling in clap for a single command.
/// What: Walks the tokens once, treating the first non-flag token as `name`;
/// subsequent `-p`/`-a` flags consume the next token as their value.
fn parse_new_args(rest: &str) -> Result<(Option<String>, Option<PathBuf>, Option<AdapterType>)> {
    let mut name: Option<String> = None;
    let mut path: Option<PathBuf> = None;
    let mut adapter: Option<AdapterType> = None;

    let mut iter = rest.split_whitespace();
    while let Some(tok) = iter.next() {
        match tok {
            "-p" | "--path" => {
                let v = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("-p needs a value"))?;
                path = Some(PathBuf::from(v));
            }
            "-a" | "--adapter" => {
                let v = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("-a needs a value"))?;
                adapter = Some(AdapterType::from_id(v));
            }
            other if !other.starts_with('-') && name.is_none() => {
                name = Some(other.to_string());
            }
            other => anyhow::bail!("unexpected token '{}'", other),
        }
    }
    Ok((name, path, adapter))
}

async fn cmd_new(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let (name_opt, path_opt, adapter_opt) = match parse_new_args(rest) {
        Ok(v) => v,
        Err(e) => {
            writeln!(out, "/tm new: {e}")?;
            return Ok(());
        }
    };

    let name = name_opt.unwrap_or_else(|| format!("tm-{}", chrono::Utc::now().timestamp()));
    let path = match path_opt {
        Some(p) => p,
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };

    let mgr = manager.lock().await;
    match mgr.new_session(&name, &path, adapter_opt).await {
        Ok(session) => {
            writeln!(
                out,
                "Created session '{}' ({}) in {}",
                session.name,
                session.adapter_type,
                session.project_path.display()
            )?;
        }
        Err(e) => {
            writeln!(out, "/tm new: failed: {e:#}")?;
        }
    }
    Ok(())
}

// ==================== /tm attach ====================

async fn cmd_attach(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let name = match require_name(rest, "attach", out)? {
        Some(n) => n,
        None => return Ok(()),
    };
    let mgr = manager.lock().await;
    match mgr.attach_instructions(&name) {
        Ok(cmd) => {
            writeln!(out, "To attach, run in a new terminal:")?;
            writeln!(out, "  {cmd}")?;
        }
        Err(e) => {
            writeln!(out, "/tm attach: {e:#}")?;
        }
    }
    Ok(())
}

// ==================== /tm pause / resume / kill ====================

async fn cmd_pause(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let name = match require_name(rest, "pause", out)? {
        Some(n) => n,
        None => return Ok(()),
    };
    let mgr = manager.lock().await;
    match mgr.pause_session(&name).await {
        Ok(()) => writeln!(out, "Paused session '{name}'")?,
        Err(e) => writeln!(out, "Cannot pause '{name}': {e:#}")?,
    }
    Ok(())
}

async fn cmd_resume(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let name = match require_name(rest, "resume", out)? {
        Some(n) => n,
        None => return Ok(()),
    };
    let mgr = manager.lock().await;
    match mgr.resume_session(&name).await {
        Ok(()) => writeln!(out, "Resumed session '{name}'")?,
        Err(e) => writeln!(out, "Cannot resume '{name}': {e:#}")?,
    }
    Ok(())
}

async fn cmd_kill(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let name = match require_name(rest, "kill", out)? {
        Some(n) => n,
        None => return Ok(()),
    };
    let mgr = manager.lock().await;
    match mgr.kill_session(&name).await {
        Ok(()) => writeln!(out, "Killed session '{name}'")?,
        Err(e) => writeln!(out, "/tm kill: {e:#}")?,
    }
    Ok(())
}

// ==================== /tm send ====================

async fn cmd_send(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let (name, message) = match rest.split_once(char::is_whitespace) {
        Some((n, m)) if !m.trim().is_empty() => (n.trim().to_string(), m.trim().to_string()),
        _ => {
            writeln!(out, "usage: /tm send <name> <message>")?;
            return Ok(());
        }
    };
    let mgr = manager.lock().await;
    match mgr.send_message(&name, &message).await {
        Ok(()) => writeln!(out, "Sent message to '{name}'")?,
        Err(e) => writeln!(out, "/tm send: {e:#}")?,
    }
    Ok(())
}

// ==================== /tm capture ====================

async fn cmd_capture(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let mut iter = rest.split_whitespace();
    let name = match iter.next() {
        Some(n) => n.to_string(),
        None => {
            writeln!(out, "usage: /tm capture <name> [lines]")?;
            return Ok(());
        }
    };
    let lines: u32 = iter.next().and_then(|s| s.parse().ok()).unwrap_or(50);

    let mgr = manager.lock().await;
    match mgr.capture_pane(&name, lines).await {
        Ok(content) => {
            writeln!(out, "─── {name} (last {lines} lines) ──────────────")?;
            // Trim trailing newlines so the closing rule sits flush.
            let body = content.trim_end_matches('\n');
            writeln!(out, "{body}")?;
            writeln!(out, "────────────────────────────────────────────")?;
        }
        Err(e) => writeln!(out, "/tm capture: {e:#}")?,
    }
    Ok(())
}

// ==================== /tm detect ====================

async fn cmd_detect(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    let name = match require_name(rest, "detect", out)? {
        Some(n) => n,
        None => return Ok(()),
    };
    let mgr = manager.lock().await;
    match mgr.detect_adapter(&name).await {
        Ok((kind, conf)) => {
            writeln!(out, "Detected: {} (confidence: {:.2})", kind, conf)?;
        }
        Err(e) => writeln!(out, "/tm detect: {e:#}")?,
    }
    Ok(())
}

// ==================== /tm reconcile ====================

async fn cmd_reconcile(manager: &Arc<Mutex<TmManager>>, out: &mut String) -> Result<()> {
    let mgr = manager.lock().await;
    writeln!(out, "Reconciling with tmux...")?;
    match mgr.reconcile().await {
        Ok(report) => {
            for s in &report.added {
                writeln!(
                    out,
                    "  + discovered: {} ({}, {})",
                    s.name,
                    s.adapter_type,
                    s.project_path.display()
                )?;
            }
            for name in &report.orphaned {
                writeln!(out, "  ~ orphaned:   {}", name)?;
            }
            for s in &report.updated {
                writeln!(out, "  = updated:    {}", s.name)?;
            }
            if report.added.is_empty() && report.orphaned.is_empty() && report.updated.is_empty() {
                writeln!(out, "  (no changes)")?;
            }
            writeln!(
                out,
                "Done: {} added, {} orphaned, {} updated",
                report.added.len(),
                report.orphaned.len(),
                report.updated.len()
            )?;
        }
        Err(e) => {
            writeln!(out, "Reconcile failed: {e:#}")?;
            writeln!(
                out,
                "Hint: ensure tmux is installed and on PATH (`which tmux`)."
            )?;
        }
    }
    Ok(())
}

// ==================== /tm status ====================

async fn cmd_status(manager: &Arc<Mutex<TmManager>>, rest: &str, out: &mut String) -> Result<()> {
    if rest.is_empty() {
        return cmd_list(manager, out).await;
    }
    let name = rest.split_whitespace().next().unwrap_or("").to_string();
    let mgr = manager.lock().await;
    let session = match mgr.registry.get_session_by_name(&name)? {
        Some(s) => s,
        None => {
            writeln!(out, "session '{name}' not found")?;
            return Ok(());
        }
    };
    let project = mgr.registry.get_project(&session.project_id)?;
    let adapter = mgr.adapters.get(session.adapter_type.as_str());
    let can_pause = adapter.as_ref().map(|a| a.can_pause()).unwrap_or(false);
    drop(mgr);

    writeln!(out, "Session: {}", session.name)?;
    if let Some(p) = project {
        writeln!(out, "  Project:  {} ({})", p.name, p.path.display())?;
    } else {
        writeln!(out, "  Project:  (unregistered)")?;
    }
    writeln!(out, "  Adapter:  {}", session.adapter_type)?;
    writeln!(out, "  Status:   {}", session.status)?;
    writeln!(
        out,
        "  Created:  {}",
        session.created_at.format("%Y-%m-%d %H:%M UTC")
    )?;
    writeln!(out, "  Active:   {}", session.last_active_ago())?;
    writeln!(out, "  Can pause: {}", if can_pause { "yes" } else { "no" })?;
    Ok(())
}

// ==================== /tm projects ====================

async fn cmd_projects(manager: &Arc<Mutex<TmManager>>, out: &mut String) -> Result<()> {
    let mgr = manager.lock().await;
    let projects = mgr.list_projects().await?;
    drop(mgr);

    writeln!(out, "TM Projects")?;
    writeln!(
        out,
        "──────────────────────────────────────────────────────"
    )?;
    if projects.is_empty() {
        writeln!(out, "  (no projects)")?;
        writeln!(
            out,
            "──────────────────────────────────────────────────────"
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "  {:<14} {:<28} {:<14} SESSIONS",
        "NAME", "PATH", "FRAMEWORK"
    )?;
    for p in &projects {
        let running = p
            .sessions
            .iter()
            .filter(|s| matches!(s.status, SessionStatus::Running))
            .count();
        let total = p.sessions.len();
        let summary = if total == 0 {
            "0".to_string()
        } else {
            format!("{} ({} running)", total, running)
        };
        writeln!(
            out,
            "  {:<14} {:<28} {:<14} {}",
            truncate(&p.name, 14),
            truncate(&p.path.display().to_string(), 28),
            truncate(&p.framework.display(), 14),
            summary
        )?;
    }
    writeln!(
        out,
        "──────────────────────────────────────────────────────"
    )?;
    writeln!(
        out,
        "{} project{}",
        projects.len(),
        if projects.len() == 1 { "" } else { "s" }
    )?;
    Ok(())
}

// ==================== Helpers ====================

/// Why: Most subcommands need a session name; centralize the "missing arg"
/// message so the wording stays consistent.
/// What: Returns `Ok(Some(name))` on success, writes a usage line and
/// returns `Ok(None)` when `rest` is empty.
fn require_name(rest: &str, sub: &str, out: &mut String) -> Result<Option<String>> {
    let name = rest.split_whitespace().next().unwrap_or("").to_string();
    if name.is_empty() {
        writeln!(out, "usage: /tm {sub} <name>")?;
        return Ok(None);
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_help_writes_subcommand_list() {
        let mut out = String::new();
        write_tm_help(&mut out);
        assert!(out.contains("/tm list"));
        assert!(out.contains("/tm new"));
        assert!(out.contains("/tm reconcile"));
    }

    #[test]
    fn parse_new_positional_only() {
        let (n, p, a) = parse_new_args("api-work").unwrap();
        assert_eq!(n.as_deref(), Some("api-work"));
        assert!(p.is_none());
        assert!(a.is_none());
    }

    #[test]
    fn parse_new_with_flags() {
        let (n, p, a) = parse_new_args("ui-dev -p /tmp/foo -a claude-code").unwrap();
        assert_eq!(n.as_deref(), Some("ui-dev"));
        assert_eq!(p, Some(PathBuf::from("/tmp/foo")));
        assert_eq!(a, Some(AdapterType::ClaudeCode));
    }

    #[test]
    fn parse_new_flag_without_value_errors() {
        assert!(parse_new_args("name -p").is_err());
        assert!(parse_new_args("name -a").is_err());
    }

    #[test]
    fn parse_new_unknown_flag_errors() {
        assert!(parse_new_args("name --bogus value").is_err());
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_long_appends_ellipsis() {
        let t = truncate("abcdefghij", 5);
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 5);
    }

    #[test]
    fn require_name_empty_writes_usage() {
        let mut out = String::new();
        let r = require_name("", "pause", &mut out).unwrap();
        assert!(r.is_none());
        assert!(out.contains("usage: /tm pause <name>"));
    }

    #[test]
    fn require_name_returns_first_word() {
        let mut out = String::new();
        let r = require_name("alpha extra", "kill", &mut out).unwrap();
        assert_eq!(r.as_deref(), Some("alpha"));
        assert!(out.is_empty());
    }
}
