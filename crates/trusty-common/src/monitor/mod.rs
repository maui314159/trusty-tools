//! Unified monitor TUI for the trusty-search and trusty-memory daemons.
//!
//! Why: operators run both the trusty-search and trusty-memory daemons and
//! want one terminal surface that shows the health of both at a glance,
//! without juggling two `curl /health` tabs. Living in `trusty-common` behind
//! the `monitor-tui` feature flag keeps the event loop, rendering, and HTTP
//! transport in separate, independently testable modules without shipping a
//! separate published crate (issue #31 companion).
//! What: a ratatui app that polls both daemons on a 2-second timer (input
//! polled every 50ms so keys feel instant), renders the [`dashboard`] panels,
//! and offers a `[r]` reindex action against the focused search index. Offline
//! daemons are retried every 5 seconds while the other panel keeps refreshing.
//! Test: `cargo test -p trusty-common --features monitor-tui` covers the pure
//! rendering, layout, and client pieces; `trusty-search monitor tui` and
//! `trusty-memory monitor tui` launch the live dashboard.

pub mod dashboard;
pub mod memory_client;
pub mod search_client;

use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use dashboard::{DashboardState, PanelStatus};
use memory_client::MemoryClient;
use search_client::SearchClient;

/// Data-refresh interval: how often both daemons are polled.
const REFRESH_INTERVAL: Duration = Duration::from_millis(2000);

/// Input-poll interval: how often the keyboard is checked.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Retry interval for a panel that is currently offline.
const OFFLINE_RETRY: Duration = Duration::from_millis(5000);

/// Run the unified monitor dashboard.
///
/// Why: the single entry point the `monitor tui` subcommand of
/// `trusty-search` and `trusty-memory` calls.
/// What: resolves both daemon URLs from the service lock files, sets up the
/// alternate screen / raw mode, runs [`run_loop`], and always restores the
/// terminal afterward even on error.
/// Test: the pure pieces (rendering, layout, clients) are unit-tested; this
/// thin terminal glue is exercised by launching the dashboard.
pub async fn run() -> anyhow::Result<()> {
    let search_url = search_client::resolve_search_url();
    let memory_url = memory_client::resolve_memory_url();
    run_with_urls(search_url, memory_url).await
}

/// Run the dashboard against explicit daemon URLs.
///
/// Why: separated from [`run`] so a caller (or a future CLI flag) can override
/// the resolved addresses, and so the terminal setup/teardown is in one place.
/// What: builds the clients and state, enters raw mode + the alternate screen,
/// runs [`run_loop`], and unconditionally restores the terminal.
/// Test: terminal glue is exercised by launching the dashboard.
pub async fn run_with_urls(search_url: String, memory_url: String) -> anyhow::Result<()> {
    let mut search = SearchClient::new(search_url.clone());
    let mut memory = MemoryClient::new(memory_url.clone());
    let mut state = DashboardState::new(search_url, memory_url);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut state, &mut search, &mut memory).await;

    // Always restore the terminal, even on error.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Poll the trusty-search daemon and fold the result into the panel.
///
/// Why: keeps the per-daemon poll logic out of the event loop so the loop can
/// re-poll on demand (after a reindex) as well as on its timer.
/// What: calls `fetch_all`; on success the panel goes `Online`, on error it
/// goes `Offline` carrying the error string. Re-resolves the daemon URL from
/// the lock file before an offline poll so the dashboard follows a restarted
/// daemon onto a fresh port.
/// Test: the pure pieces are unit-tested; this is thin I/O glue.
async fn poll_search(state: &mut DashboardState, client: &mut SearchClient) {
    // A daemon that was offline may have restarted onto a new port — follow it.
    if !state.search.status.is_online() {
        let resolved = search_client::resolve_search_url();
        if resolved != client.base_url() {
            client.set_base_url(resolved.clone());
            state.search.base_url = resolved;
        }
    }
    match client.fetch_all().await {
        Ok(data) => state.search.status = PanelStatus::Online(data),
        Err(e) => {
            state.search.status = PanelStatus::Offline {
                last_error: e.to_string(),
            }
        }
    }
}

/// Poll the trusty-memory daemon and fold the result into the panel.
///
/// Why: mirrors [`poll_search`] for the memory daemon.
/// What: re-resolves the URL when offline, then calls `fetch_all`; success
/// yields `Online`, failure yields `Offline` with the error.
/// Test: the pure pieces are unit-tested; this is thin I/O glue.
async fn poll_memory(state: &mut DashboardState, client: &mut MemoryClient) {
    if !state.memory.status.is_online() {
        let resolved = memory_client::resolve_memory_url();
        if resolved != client.base_url() {
            client.set_base_url(resolved.clone());
            state.memory.base_url = resolved;
        }
    }
    match client.fetch_all().await {
        Ok(data) => state.memory.status = PanelStatus::Online(data),
        Err(e) => {
            state.memory.status = PanelStatus::Offline {
                last_error: e.to_string(),
            }
        }
    }
}

/// Whether a panel is due for a poll given the elapsed time since its last one.
///
/// Why: an online panel refreshes on the normal 2s cadence, but an offline
/// panel should retry on the slower 5s cadence so a downed daemon does not
/// spam connection attempts; this decides which applies.
/// What: returns `true` when `elapsed` has reached the cadence implied by
/// `online` (2s online, 5s offline).
/// Test: `panel_due_uses_offline_cadence`.
fn panel_due(online: bool, elapsed: Duration) -> bool {
    let cadence = if online {
        REFRESH_INTERVAL
    } else {
        OFFLINE_RETRY
    };
    elapsed >= cadence
}

/// The dashboard event loop: poll the daemons, render, handle input.
///
/// Why: kept separate from [`run_with_urls`] so terminal setup/teardown wraps
/// it cleanly.
/// What: polls both daemons immediately, then renders every frame while
/// polling the keyboard every 50ms; each panel re-polls on its own cadence
/// ([`panel_due`]). `Tab` cycles focus, `r` reindexes the focused search
/// index, `?` toggles help, `q`/`Esc` (and `Ctrl-C` via crossterm) quit.
/// Test: the pure pieces (rendering, layout, clients) are unit-tested.
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut DashboardState,
    search: &mut SearchClient,
    memory: &mut MemoryClient,
) -> anyhow::Result<()> {
    poll_search(state, search).await;
    poll_memory(state, memory).await;
    let mut last_search_poll = Instant::now();
    let mut last_memory_poll = Instant::now();

    loop {
        terminal.draw(|f| dashboard::render(f, state))?;

        // Drain a pending keypress, if any. `event::poll` is non-blocking
        // beyond the short `INPUT_POLL` window; a non-`Key` event (resize,
        // mouse) leaves `key` `None` and the input handling is skipped.
        let key = if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) => Some(key),
                _ => None,
            }
        } else {
            None
        };
        if let Some(key) = key {
            use crossterm::event::{KeyEventKind, KeyModifiers};
            // crossterm reports both press and release on some platforms;
            // act only on the press so a key never fires twice.
            if key.kind != KeyEventKind::Release {
                // Ctrl-C always quits, regardless of the help overlay.
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(());
                }
                if state.show_help {
                    if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                        state.show_help = false;
                    } else if key.code == KeyCode::Char('q') {
                        return Ok(());
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('?') => state.show_help = true,
                        KeyCode::Tab => state.toggle_focus(),
                        KeyCode::Char('r') => {
                            trigger_reindex(state, search).await;
                            // Reflect the new chunk counts immediately.
                            poll_search(state, search).await;
                            last_search_poll = Instant::now();
                        }
                        _ => {}
                    }
                }
            }
        }

        if panel_due(state.search.status.is_online(), last_search_poll.elapsed()) {
            poll_search(state, search).await;
            last_search_poll = Instant::now();
        }
        if panel_due(state.memory.status.is_online(), last_memory_poll.elapsed()) {
            poll_memory(state, memory).await;
            last_memory_poll = Instant::now();
        }
    }
}

/// Trigger a reindex of the focused search index and record the outcome.
///
/// Why: the `r` key reindexes a search index; the result must be surfaced in
/// the header so the operator sees whether it was accepted.
/// What: resolves the reindex target via [`DashboardState::reindex_target`]
/// (a no-op when the search panel is not focused or has no index), POSTs the
/// reindex, and writes a human-readable note into `last_action`.
/// Test: the target resolution is unit-tested via `test_reindex_target`.
async fn trigger_reindex(state: &mut DashboardState, client: &SearchClient) {
    let Some(id) = state.reindex_target() else {
        state.last_action = Some("reindex: focus the SEARCH panel first".to_string());
        return;
    };
    match client.reindex(&id).await {
        Ok(()) => state.last_action = Some(format!("reindex queued for '{id}'")),
        Err(e) => {
            tracing::warn!("reindex of {id} failed: {e}");
            state.last_action = Some(format!("reindex of '{id}' failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_due_uses_online_cadence() {
        // An online panel polls on the 2s cadence.
        assert!(!panel_due(true, Duration::from_millis(1999)));
        assert!(panel_due(true, Duration::from_millis(2000)));
    }

    #[test]
    fn panel_due_uses_offline_cadence() {
        // An offline panel retries on the slower 5s cadence: a 2s gap is not
        // yet due, a 5s gap is.
        assert!(!panel_due(false, Duration::from_millis(2000)));
        assert!(panel_due(false, Duration::from_millis(5000)));
    }

    #[tokio::test]
    async fn trigger_reindex_without_focus_records_note() {
        // With the SEARCH panel un-focused there is no reindex target, so the
        // action must report that rather than attempting an HTTP call.
        let mut state = DashboardState::new("http://127.0.0.1:7878", "http://127.0.0.1:7070");
        state.toggle_focus(); // focus -> Memory
        let client = SearchClient::new("http://127.0.0.1:7878");
        trigger_reindex(&mut state, &client).await;
        assert!(
            state
                .last_action
                .as_deref()
                .unwrap_or_default()
                .contains("focus the SEARCH panel"),
            "expected a focus hint, got {:?}",
            state.last_action
        );
    }

    #[tokio::test]
    async fn poll_memory_resolves_to_a_terminal_status() {
        // Polling must always leave the panel in a renderable terminal state —
        // never `Connecting` — whether or not a real daemon answers. We do not
        // assert Offline specifically because the poller re-resolves the URL
        // from the lock file, so a live local daemon would legitimately make
        // the panel Online; both outcomes are correct.
        let mut state = DashboardState::new("http://127.0.0.1:1", "http://127.0.0.1:2");
        let mut client = MemoryClient::new("http://127.0.0.1:2");
        poll_memory(&mut state, &mut client).await;
        match &state.memory.status {
            PanelStatus::Offline { last_error } => assert!(!last_error.is_empty()),
            PanelStatus::Online(_) => {}
            PanelStatus::Connecting => panic!("poll must leave Connecting behind"),
        }
    }
}
