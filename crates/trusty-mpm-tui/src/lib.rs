//! trusty-mpm TUI coordinator dashboard library.
//!
//! Why: operators need one conversational surface — the *coordinator chat* —
//! that has visibility into every active Claude Code session, with a
//! dismissable session sidebar beside it. Exposing the dashboard as a library
//! lets the unified `trusty-mpm tui` subcommand reuse it without a separate
//! binary.
//! What: a ratatui app that polls the daemon's coordinator-context endpoint on
//! a timer, renders the [`dashboard`] panes, and POSTs typed messages to the
//! coordinator-chat endpoint. Rendering and HTTP are split into the
//! [`dashboard`] and [`client`] modules so the logic is unit-testable.
//! Test: `cargo test -p trusty-mpm-tui` covers chat/session formatting and the
//! client; `trusty-mpm tui` launches the live dashboard.

pub mod client;
pub mod dashboard;
pub mod iterm2;

use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use client::DaemonClient;
use dashboard::{ChatMessage, DashboardState, Focus};

/// Run the ratatui coordinator dashboard against `url`.
///
/// Why: shared entry point for both the `trusty-mpm tui` subcommand and the
/// backward-compatible `trusty-mpm-tui` shim binary.
/// What: sets up the alternate screen / raw mode, runs [`run_loop`], and always
/// restores the terminal afterward even on error.
/// Test: pure parts (rendering, client) are unit-tested; this is the thin glue
/// exercised by launching the dashboard.
pub async fn run(url: String, interval_ms: u64) -> anyhow::Result<()> {
    run_focused(url, interval_ms, None).await
}

/// Re-resolve the daemon URL from the lock file when the daemon is unreachable.
///
/// Why: `DaemonClient` is built once at startup; if the daemon later restarted
/// onto a fresh ephemeral port, the client would stay pinned to a stale address
/// forever. Re-resolving on every failed poll lets the TUI self-heal.
/// What: when `reachable` is `false`, calls [`trusty_mpm_core::resolve_daemon_url`]
/// and, if it yields a different URL, re-points the client and returns `true`.
/// Test: `rediscover_is_noop_when_daemon_reachable`.
fn rediscover_daemon(client: &mut DaemonClient, reachable: bool) -> bool {
    if reachable {
        return false;
    }
    let resolved = trusty_mpm_core::resolve_daemon_url(None);
    if resolved != client.base_url() {
        client.set_base_url(resolved);
        true
    } else {
        false
    }
}

/// Run the dashboard, optionally pre-focusing a session in the sidebar.
///
/// Why: `tm connect <target>` resolves a session id and wants the TUI to open
/// with that session highlighted in the sidebar.
/// What: same terminal setup/teardown as [`run`], threading `focus_id` into
/// [`run_loop`], which selects the matching session after the priming poll.
/// Test: terminal glue is exercised by launching the dashboard.
pub async fn run_focused(
    url: String,
    interval_ms: u64,
    focus_id: Option<String>,
) -> anyhow::Result<()> {
    let mut client = DaemonClient::new(url);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut client, interval_ms, focus_id).await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Refresh [`DashboardState`] from one daemon poll.
///
/// Why: keeps the poll logic out of the key-driven event loop so the loop can
/// re-poll on demand (after a send) as well as on its timer.
/// What: probes health, then pulls the coordinator context (the session list);
/// clears the sessions when the daemon is unreachable. When the daemon looks
/// unreachable it re-resolves the URL from the lock file via
/// [`rediscover_daemon`] and retries one health probe.
/// Test: the pure pieces (rendering, client, rediscovery) are unit-tested.
async fn poll_daemon(state: &mut DashboardState, client: &mut DaemonClient) {
    state.daemon_reachable = client.is_healthy().await;
    if rediscover_daemon(client, state.daemon_reachable) {
        state.daemon_reachable = client.is_healthy().await;
    }
    if state.daemon_reachable {
        match client.coordinator_context().await {
            Ok(context) => {
                state.sessions = context
                    .sessions
                    .into_iter()
                    .map(coordinator_session_to_row)
                    .collect();
            }
            Err(_) => state.daemon_reachable = false,
        }
    } else {
        state.sessions.clear();
    }
    state.clamp_selection();
}

/// Convert a coordinator-context session into a dashboard `SessionRow`.
///
/// Why: the dashboard sidebar renders `SessionRow`s; the coordinator endpoint
/// returns a richer `CoordinatorSession`, so a projection is needed.
/// What: maps the id (parsed from its UUID string), tmux name, workdir,
/// delegation count, and a status word back into a `SessionStatus`.
/// Test: covered indirectly by `poll_daemon`; the status mapping is pure.
fn coordinator_session_to_row(s: trusty_mpm_client::CoordinatorSession) -> client::SessionRow {
    use trusty_mpm_core::session::{SessionId, SessionStatus};
    let id = uuid::Uuid::parse_str(&s.id)
        .map(SessionId)
        .unwrap_or_else(|_| SessionId(uuid::Uuid::nil()));
    let status = match s.status.as_str() {
        "Starting" => SessionStatus::Starting,
        "Active" => SessionStatus::Active,
        "AwaitingApproval" => SessionStatus::AwaitingApproval,
        "Detached" => SessionStatus::Detached,
        "Paused" => SessionStatus::Paused,
        _ => SessionStatus::Stopped,
    };
    client::SessionRow {
        id,
        workdir: s.workdir,
        status,
        active_delegations: s.active_delegations,
        tmux_name: s.name,
        last_seen: Default::default(),
    }
}

/// Send the typed message to the coordinator and fold the reply into the chat.
///
/// Why: pressing Enter is the single action of the coordinator dashboard —
/// every message goes to `POST /api/v1/coordinator/chat`, which either routes a
/// `@prefix:` command at a session or answers via the LLM.
/// What: appends the user message to the transcript, calls the daemon, then
/// appends the coordinator reply (or routed-command output). A `None` response
/// (LLM not configured) or a transport error becomes a coordinator-authored
/// note so a failure is always renderable, never a panic.
/// Test: `coordinator_send_without_daemon_reports_error`.
async fn coordinator_send(state: &mut DashboardState, client: &DaemonClient, message: &str) {
    state.push_chat(ChatMessage::user(message));
    match client.coordinator_chat(message, &state.coord_history).await {
        Ok(Some(outcome)) => {
            let reply = match outcome.command_output {
                Some(output) => format!("{}\n{output}", outcome.reply),
                None => outcome.reply.clone(),
            };
            state.push_chat(ChatMessage::coordinator(reply));
            // A routed command resets the LLM window — it was not a chat turn.
            if outcome.routed_to_session.is_some() {
                state.coord_history.clear();
            } else {
                state
                    .coord_history
                    .push(trusty_mpm_client::ChatMessage::user(message.to_string()));
                state
                    .coord_history
                    .push(trusty_mpm_client::ChatMessage::assistant(outcome.reply));
            }
            state.last_action = Some("message sent".to_string());
        }
        Ok(None) => {
            state.push_chat(ChatMessage::coordinator(
                "coordinator chat is not configured — set OPENROUTER_API_KEY and enable the overseer",
            ));
            state.last_action = Some("LLM not configured".to_string());
        }
        Err(e) => {
            state.push_chat(ChatMessage::coordinator(format!("daemon error: {e}")));
            state.last_action = Some("daemon error".to_string());
        }
    }
}

/// The dashboard event loop: poll the daemon, render, handle input.
///
/// Why: kept separate from [`run`] so terminal setup/teardown wraps it cleanly.
/// What: refreshes [`DashboardState`] from the daemon on an `interval_ms` timer
/// but polls the keyboard every 50ms so input feels instantaneous; Enter sends
/// the typed message to the coordinator and triggers an immediate re-poll;
/// `s` toggles the sidebar, `Tab` switches focus, arrows scroll/select, `q`
/// quits.
/// Test: the pure pieces (rendering, client) are unit-tested.
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: &mut DaemonClient,
    interval_ms: u64,
    focus_id: Option<String>,
) -> anyhow::Result<()> {
    // The sidebar starts visible only when there is at least one session to
    // show; otherwise the coordinator chat gets the full width immediately.
    let mut state = DashboardState::default();

    poll_daemon(&mut state, client).await;
    state.sidebar_visible = !state.sessions.is_empty();
    // Apply a `tm connect` focus once the priming poll has filled the list.
    if let Some(id) = focus_id.as_deref()
        && let Some(idx) = state.sessions.iter().position(|s| s.id.0.to_string() == id)
    {
        state.selected_session = idx;
        state.last_action = Some(format!("Connected to {id}"));
    }
    let mut last_poll = Instant::now();

    loop {
        terminal.draw(|f| dashboard::render(f, &state))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            // The help overlay swallows the next key (to close itself).
            if state.show_help {
                if matches!(
                    key.code,
                    KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
                ) {
                    if key.code == KeyCode::Char('q') {
                        return Ok(());
                    }
                    state.show_help = false;
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('?') => state.show_help = true,
                KeyCode::Char('s') => state.toggle_sidebar(),
                KeyCode::Tab => state.toggle_focus(),
                KeyCode::Esc => state.command_bar.clear(),
                KeyCode::Up => match state.focus {
                    Focus::Sidebar => state.select_up(),
                    Focus::Input => {
                        // ↑ recalls input history when the buffer is empty,
                        // otherwise scrolls the chat transcript.
                        if state.command_bar.input.is_empty() {
                            state.scroll_up();
                        } else {
                            state.command_bar.history_prev();
                        }
                    }
                },
                KeyCode::Down => match state.focus {
                    Focus::Sidebar => state.select_down(),
                    Focus::Input => {
                        if state.command_bar.input.is_empty() {
                            state.scroll_down();
                        } else {
                            state.command_bar.history_next();
                        }
                    }
                },
                KeyCode::Enter => {
                    if state.focus == Focus::Sidebar {
                        // Enter on a sidebar row prefills the input with the
                        // session's `@prefix:` routing prefix and returns focus
                        // to the input bar so the operator can type a command.
                        if let Some(name) = state.selected_target() {
                            let prefix = dashboard::session_prefix(&name);
                            state.command_bar.input = format!("@{prefix}: ");
                            state.focus = Focus::Input;
                        }
                    } else {
                        let typed = state.command_bar.take_for_execution();
                        if !typed.is_empty() {
                            coordinator_send(&mut state, client, &typed).await;
                            poll_daemon(&mut state, client).await;
                            last_poll = Instant::now();
                        }
                    }
                }
                KeyCode::Backspace if state.focus == Focus::Input => {
                    state.command_bar.backspace();
                }
                KeyCode::Char(c) if state.focus == Focus::Input => {
                    state.command_bar.push(c);
                }
                _ => {}
            }
        }

        // Throttle the data refresh: only re-poll the daemon every interval_ms.
        if last_poll.elapsed() >= Duration::from_millis(interval_ms) {
            poll_daemon(&mut state, client).await;
            last_poll = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rediscover_is_noop_when_daemon_reachable() {
        // A reachable daemon must never trigger a URL re-resolution.
        let mut client = DaemonClient::new("http://127.0.0.1:7880");
        assert!(!rediscover_daemon(&mut client, true));
        assert_eq!(client.base_url(), "http://127.0.0.1:7880");
    }

    #[test]
    fn rediscover_is_noop_when_resolved_url_unchanged() {
        // When the daemon is unreachable but the lock file resolves to the same
        // URL, re-pointing is pointless and the function reports "no change".
        let mut client = DaemonClient::new(trusty_mpm_core::DEFAULT_DAEMON_URL);
        let changed = rediscover_daemon(&mut client, false);
        if !changed {
            assert_eq!(client.base_url(), trusty_mpm_core::DEFAULT_DAEMON_URL);
        }
    }

    #[test]
    fn coordinator_session_maps_status() {
        // The status word from the coordinator endpoint maps back to the enum.
        let session = trusty_mpm_client::CoordinatorSession {
            id: "00000000-0000-0000-0000-000000000000".into(),
            name: "tmpm-foo".into(),
            prefix: "foo".into(),
            workdir: "/tmp/p".into(),
            status: "Paused".into(),
            active_delegations: 2,
            recent_output: Vec::new(),
        };
        let row = coordinator_session_to_row(session);
        assert_eq!(row.tmux_name, "tmpm-foo");
        assert_eq!(row.active_delegations, 2);
        assert_eq!(row.status, trusty_mpm_core::session::SessionStatus::Paused);
    }

    #[tokio::test]
    async fn coordinator_send_without_daemon_reports_error() {
        // A send against an unreachable daemon appends the user message and a
        // coordinator-authored error note rather than panicking.
        let client = DaemonClient::new("http://127.0.0.1:0");
        let mut state = DashboardState::default();
        coordinator_send(&mut state, &client, "what is happening?").await;
        assert_eq!(state.chat_history.len(), 2);
        assert_eq!(state.chat_history[0].role, dashboard::ChatRole::User);
        assert!(
            state.chat_history[1].content.contains("daemon error"),
            "expected a daemon error, got {:?}",
            state.chat_history[1].content
        );
    }
}
