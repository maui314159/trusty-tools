//! Service-specific terminal UI for the trusty-search daemon.
//!
//! Why: operators of the trusty-search daemon want a focused, live terminal
//! surface — an index list, a streaming activity log of reindex/search events,
//! and a query bar — rather than the generic two-daemon dashboard. Living in
//! `trusty-common` behind the `monitor-tui` feature keeps the pure state /
//! rendering testable without a separate published crate (issue #34).
//! What: a ratatui app with a 3-zone layout (title bar, INDEXES + right-hand
//! split, SEARCH input bar). The INDEXES list always leads with an "All
//! indexes" entry that fans queries out across every index; the right side is
//! split vertically into an ACTIVITY feed (top) and a STATISTICS panel
//! (bottom), both scoped to the selected index — or aggregated when "All" is
//! selected. It polls the daemon every 2 seconds, streams reindex progress
//! over SSE on `[r]`, and runs hybrid searches from the input bar on `[Enter]`.
//! Input is polled every 50 ms so keys feel instant.
//! Test: `cargo test -p trusty-common --features monitor-tui` covers the pure
//! state, log capacity, selection clamp, the "All" selector, and the
//! activity / statistics line builders; `trusty-search monitor tui` launches
//! the live UI.

use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use tokio::sync::mpsc;

use crate::monitor::dashboard::{IndexRow, format_count};
use crate::monitor::search_client::{ReindexEvent, SearchClient, resolve_search_url};
use crate::monitor::utils::{ActivityLog, DaemonStatus, fmt_uptime};

/// Data-refresh interval: how often the daemon is polled.
const REFRESH_INTERVAL: Duration = Duration::from_millis(2000);

/// Input-poll interval: how often the keyboard is checked.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Number of results requested per search query.
const SEARCH_TOP_K: usize = 5;

/// Maximum width (in columns) of the left INDEXES panel.
const LEFT_PANEL_MAX: u16 = 28;

/// Crate version, surfaced in the title bar.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line key hint shown along the bottom of the UI.
pub const KEY_HINT: &str =
    "[Tab] focus  [r] reindex  [↑↓] select  [Enter] search  [q] quit  [?] help";

/// Label for the synthetic "All indexes" entry at the top of the list.
///
/// Why: selecting it fans queries / stats out across every index; a single
/// constant keeps the label consistent between the list and the panel titles.
/// What: the display text of the index list's first row.
/// Test: `test_index_lines` asserts this is the first row.
pub const ALL_LABEL: &str = "All indexes";

/// Which zone of the search UI currently holds keyboard focus.
///
/// Why: `[Tab]` cycles focus; the index list and the query bar consume keys
/// differently (navigation vs. text entry).
/// What: `List` (the default — the INDEXES panel) or `Input` (the SEARCH bar).
/// Test: `test_toggle_focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchFocus {
    /// The INDEXES list panel has focus; arrows move the selection.
    #[default]
    List,
    /// The SEARCH input bar has focus; typed characters edit the query.
    Input,
}

/// All mutable state the search UI renders and mutates.
///
/// Why: the event loop polls the daemon, streams reindex events, and handles
/// input — keeping every piece of state in one struct keeps the loop terse and
/// the rendering a pure function of this snapshot.
/// What: the daemon URL and status, the index list and selection cursor, the
/// bounded activity log, the query buffer, the focused zone, and the help flag.
/// The selection cursor addresses a list whose first row is the synthetic
/// "All indexes" entry, so cursor `0` means "All" and cursor `n` (n ≥ 1) means
/// `indexes[n - 1]`.
/// Test: `test_selected_clamp`, `test_toggle_focus`, `test_log_append`,
/// `test_all_selector`.
#[derive(Debug, Clone)]
pub struct SearchTuiState {
    /// The trusty-search daemon base URL being monitored.
    pub base_url: String,
    /// The daemon's current liveness state.
    pub daemon_status: DaemonStatus,
    /// One row per registered index.
    pub indexes: Vec<IndexRow>,
    /// Cursor into the index list, where row `0` is the "All indexes" entry
    /// and row `n` (n ≥ 1) selects `indexes[n - 1]`.
    pub selected: usize,
    /// Bounded, timestamped log of reindex / search activity.
    pub log: ActivityLog,
    /// The in-progress search query buffer.
    pub input: String,
    /// Which zone currently holds keyboard focus.
    pub focus: SearchFocus,
    /// Whether the help overlay is visible (toggled with `?`).
    pub show_help: bool,
}

impl SearchTuiState {
    /// Build a fresh search UI state targeting `base_url`.
    ///
    /// Why: the event loop seeds the state at startup before the first poll.
    /// What: stores the URL, sets the daemon `Connecting`, and starts with an
    /// empty index list, empty log, empty query, and list focus.
    /// Test: `test_new_state_defaults`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            daemon_status: DaemonStatus::Connecting,
            indexes: Vec::new(),
            selected: 0,
            log: ActivityLog::new(),
            input: String::new(),
            focus: SearchFocus::List,
            show_help: false,
        }
    }

    /// Cycle keyboard focus between the index list and the query bar (`[Tab]`).
    ///
    /// Why: `[Tab]` decides whether arrows navigate the list or whether typed
    /// characters edit the search query.
    /// What: flips [`Self::focus`] between [`SearchFocus::List`] and
    /// [`SearchFocus::Input`].
    /// Test: `test_toggle_focus`.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            SearchFocus::List => SearchFocus::Input,
            SearchFocus::Input => SearchFocus::List,
        };
    }

    /// Move the index selection up one row, saturating at the top.
    ///
    /// Why: `↑` navigates the INDEXES list when it has focus.
    /// What: decrements [`Self::selected`], never below zero.
    /// Test: `test_selected_clamp`.
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the index selection down one row, clamped to the last index.
    ///
    /// Why: `↓` navigates the INDEXES list when it has focus.
    /// What: increments [`Self::selected`] but never past the last row. The
    /// list has `indexes.len() + 1` rows (row 0 is "All indexes").
    /// Test: `test_selected_clamp`.
    pub fn select_down(&mut self) {
        if self.selected < self.last_row() {
            self.selected += 1;
        }
    }

    /// The index of the last selectable row.
    ///
    /// Why: the list always carries the synthetic "All" row, so the last valid
    /// cursor is `indexes.len()` (not `indexes.len() - 1`).
    /// What: returns `indexes.len()` — row 0 is "All", rows `1..=len` are the
    /// individual indexes.
    /// Test: `test_selected_clamp`.
    fn last_row(&self) -> usize {
        self.indexes.len()
    }

    /// Clamp the selection cursor to the current index count.
    ///
    /// Why: a poll can shrink the index list (an index was deleted) leaving the
    /// cursor past the end; this keeps it valid before rendering.
    /// What: caps [`Self::selected`] at `indexes.len()` (the "All" row plus one
    /// row per index).
    /// Test: `test_selected_clamp`.
    pub fn clamp_selection(&mut self) {
        if self.selected > self.last_row() {
            self.selected = self.last_row();
        }
    }

    /// Whether the "All indexes" entry is currently selected.
    ///
    /// Why: when "All" is selected the UI fans queries out across every index
    /// and aggregates the activity feed and statistics.
    /// What: returns `true` exactly when the cursor is on row 0.
    /// Test: `test_all_selector`.
    pub fn is_all_selected(&self) -> bool {
        self.selected == 0
    }

    /// The id of the currently selected single index, if any.
    ///
    /// Why: `[r]` reindexes and `[Enter]` searches a single index; both need
    /// its id, and neither applies when "All" is selected.
    /// What: returns `Some(id)` for the index at cursor row `n ≥ 1`, or `None`
    /// when "All" is selected or the index list is empty.
    /// Test: `test_selected_id`.
    pub fn selected_id(&self) -> Option<&str> {
        if self.selected == 0 {
            return None;
        }
        self.indexes.get(self.selected - 1).map(|i| i.id.as_str())
    }

    /// The scope filter for the activity feed and statistics panels.
    ///
    /// Why: the right-hand panels render the selected index's events / stats,
    /// or every index's when "All" is selected; this folds the cursor into the
    /// `Option<&str>` filter [`ActivityLog::tail_scoped`] expects.
    /// What: returns `None` when "All" is selected (un-filtered) or `Some(id)`
    /// for the selected single index.
    /// Test: `test_all_selector`.
    pub fn scope_filter(&self) -> Option<&str> {
        self.selected_id()
    }
}

/// Run the trusty-search monitor TUI.
///
/// Why: the single entry point the `monitor tui` subcommand of `trusty-search`
/// calls.
/// What: resolves the daemon URL from the service lock file and delegates to
/// [`run_with_url`].
/// Test: the pure pieces are unit-tested; this thin glue is exercised by
/// launching the UI.
pub async fn run() -> anyhow::Result<()> {
    run_with_url(resolve_search_url()).await
}

/// Run the search TUI against an explicit daemon URL.
///
/// Why: separated from [`run`] so a future CLI flag can override the resolved
/// address, and so terminal setup/teardown lives in one place.
/// What: builds the client and state, enters raw mode + the alternate screen,
/// runs [`run_loop`], and unconditionally restores the terminal even on error.
/// Test: terminal glue is exercised by launching the UI.
pub async fn run_with_url(base_url: String) -> anyhow::Result<()> {
    let mut client = SearchClient::new(base_url.clone());
    let mut state = SearchTuiState::new(base_url);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut state, &mut client).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Poll the trusty-search daemon and fold the result into `state`.
///
/// Why: keeps the per-poll I/O out of the event loop so the loop can re-poll
/// on demand as well as on its timer.
/// What: re-resolves the URL when the daemon is offline (it may have rebound a
/// fresh port), calls `fetch_all`, and updates the status, index list, and
/// selection clamp.
/// Test: thin I/O glue; the pure clamp is unit-tested.
async fn poll_daemon(state: &mut SearchTuiState, client: &mut SearchClient) {
    if !state.daemon_status.is_online() {
        let resolved = resolve_search_url();
        if resolved != client.base_url() {
            client.set_base_url(resolved.clone());
            state.base_url = resolved;
        }
    }
    match client.fetch_all().await {
        Ok(data) => {
            state.daemon_status = DaemonStatus::Online {
                version: data.version,
                uptime_secs: data.uptime_secs,
            };
            state.indexes = data.indexes;
            state.clamp_selection();
        }
        Err(e) => {
            state.daemon_status = DaemonStatus::Offline {
                last_error: e.to_string(),
            };
        }
    }
}

/// Run a search and append the hits to the activity log.
///
/// Why: pressing `[Enter]` in the query bar runs a hybrid search; the operator
/// sees the results inline in the ACTIVITY panel. When "All indexes" is
/// selected the search fans out across every registered index.
/// What: with a single index selected, calls `client.search` for it and logs a
/// `search "<q>" → N results` summary plus one indented continuation line per
/// hit, all tagged with that index's id. With "All" selected it iterates every
/// index, logging each index's per-index summary tagged to that index and a
/// final daemon-wide total line. An empty query is a no-op; transport errors
/// are logged.
/// Test: thin I/O glue; result projection is tested in `search_client`.
async fn run_search(state: &mut SearchTuiState, client: &SearchClient) {
    let query = state.input.trim().to_string();
    if query.is_empty() {
        return;
    }

    if state.is_all_selected() {
        run_search_all(state, client, &query).await;
    } else if let Some(id) = state.selected_id().map(str::to_string) {
        run_search_one(state, client, &id, &query).await;
    } else {
        state.log.push("search: no index selected");
    }
    state.input.clear();
}

/// Run a search against one index and append the hits to the log.
///
/// Why: the single-index path of [`run_search`]; isolating it keeps the
/// fan-out loop in [`run_search_all`] terse.
/// What: calls `client.search(id, …)`, appends an `id`-scoped summary line and
/// one indented `path:line  snippet` continuation per hit. A transport error
/// is logged as an `id`-scoped failure line.
/// Test: thin I/O glue; result projection is tested in `search_client`.
async fn run_search_one(state: &mut SearchTuiState, client: &SearchClient, id: &str, query: &str) {
    match client.search(id, query, SEARCH_TOP_K).await {
        Ok(hits) => {
            state
                .log
                .push_scoped(id, format!("search \"{query}\" → {} results", hits.len()));
            for hit in &hits {
                state
                    .log
                    .push_raw_scoped(id, format!("  {}:{}  {}", hit.file, hit.line, hit.snippet));
            }
        }
        Err(e) => state
            .log
            .push_scoped(id, format!("search \"{query}\" failed: {e}")),
    }
}

/// Fan a search out across every index and append a merged summary.
///
/// Why: the "All indexes" selector lets an operator run one query over the
/// whole machine's corpus; the activity feed then shows each index's hit count
/// (tagged so the per-index view still works) plus a daemon-wide total.
/// What: snapshots the index ids, then for each calls `client.search`,
/// appending an `id`-scoped `· <id>: N results` line. A daemon-wide
/// `search "<q>" (all) → T results across K indexes` total line closes the
/// burst. With no indexes registered it logs a single note.
/// Test: thin I/O glue; the single-index search is unit-tested in
/// `search_client`.
async fn run_search_all(state: &mut SearchTuiState, client: &SearchClient, query: &str) {
    let ids: Vec<String> = state.indexes.iter().map(|i| i.id.clone()).collect();
    if ids.is_empty() {
        state.log.push("search (all): no indexes registered");
        return;
    }
    state
        .log
        .push(format!("search \"{query}\" (all) → {} indexes", ids.len()));
    let mut total = 0usize;
    for id in &ids {
        match client.search(id, query, SEARCH_TOP_K).await {
            Ok(hits) => {
                total += hits.len();
                state
                    .log
                    .push_raw_scoped(id, format!("  · {id}: {} results", hits.len()));
            }
            Err(e) => state
                .log
                .push_raw_scoped(id, format!("  · {id}: failed: {e}")),
        }
    }
    state.log.push(format!(
        "search \"{query}\" (all) → {total} results across {} indexes",
        ids.len()
    ));
}

/// One reindex SSE event tagged with the index it concerns.
///
/// Why: a reindex is started per-index, but the streamed [`ReindexEvent`]s
/// carry no index id; pairing each with `index_id` lets the activity log scope
/// the line so the per-index feed (and the "All" merge) stay correct.
/// What: the index id the reindex targets and the streamed event.
/// Test: `test_apply_reindex_event` exercises the scoped logging.
#[derive(Debug, Clone)]
pub struct ScopedReindexEvent {
    /// The index this reindex event belongs to.
    pub index_id: String,
    /// The streamed reindex progress event.
    pub event: ReindexEvent,
}

/// Apply one streamed reindex event to the activity log, scoped to its index.
///
/// Why: the reindex SSE task forwards [`ScopedReindexEvent`]s through a
/// channel; the event loop drains them and this turns each into a
/// human-readable, index-scoped log line so the per-index activity feed shows
/// only its own reindex progress.
/// What: `Started` / `Progress` / `Complete` / `Failed` each map to a distinct
/// timestamped line tagged with `scoped.index_id`, with progress carrying a
/// percent-complete figure.
/// Test: `test_apply_reindex_event`.
pub fn apply_reindex_event(state: &mut SearchTuiState, scoped: ScopedReindexEvent) {
    let id = scoped.index_id;
    match scoped.event {
        ReindexEvent::Started { total_files } => {
            state
                .log
                .push_scoped(&id, format!("reindex started: {total_files} files"));
        }
        ReindexEvent::Progress {
            indexed,
            total_files,
        } => {
            let pct = if total_files > 0 {
                indexed.saturating_mul(100) / total_files
            } else {
                0
            };
            state
                .log
                .push_scoped(&id, format!("indexing: {indexed}/{total_files} ({pct}%)"));
        }
        ReindexEvent::Complete {
            total_chunks,
            status,
        } => {
            state.log.push_scoped(
                &id,
                format!("reindex {status}: {} chunks", format_count(total_chunks)),
            );
        }
        ReindexEvent::Failed(message) => {
            state
                .log
                .push_scoped(&id, format!("reindex error: {message}"));
        }
    }
}

/// The search TUI event loop: poll, render, handle input, drain reindex events.
///
/// Why: kept separate from [`run_with_url`] so terminal setup/teardown wraps it
/// cleanly.
/// What: polls the daemon immediately, then renders every frame while polling
/// the keyboard every 50 ms; re-polls on the 2 s timer. `[r]` spawns an SSE
/// reindex task whose events are drained via `try_recv`; `[Enter]` runs a
/// search; `Tab`, arrows, `?`, `q`/`Esc`, and `Ctrl-C` behave per [`KEY_HINT`].
/// Test: the pure pieces (state, log, rendering helpers) are unit-tested.
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut SearchTuiState,
    client: &mut SearchClient,
) -> anyhow::Result<()> {
    poll_daemon(state, client).await;
    let mut last_poll = Instant::now();

    // Channel for reindex SSE events forwarded by a background task. Each
    // event is tagged with its index id so the activity feed can scope it.
    let (reindex_tx, mut reindex_rx) = mpsc::channel::<ScopedReindexEvent>(64);

    loop {
        terminal.draw(|f| render(f, state))?;

        // Drain any reindex events the SSE task has produced since last frame.
        while let Ok(event) = reindex_rx.try_recv() {
            apply_reindex_event(state, event);
        }

        let key = if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) => Some(key),
                _ => None,
            }
        } else {
            None
        };
        if let Some(key) = key
            && key.kind != KeyEventKind::Release
        {
            // Ctrl-C always quits, regardless of focus or the help overlay.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                return Ok(());
            }
            if state.show_help {
                if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                    state.show_help = false;
                } else if key.code == KeyCode::Char('q') {
                    return Ok(());
                }
                continue;
            }
            match (state.focus, key.code) {
                (_, KeyCode::Char('?')) => state.show_help = true,
                (_, KeyCode::Tab) => state.toggle_focus(),
                (_, KeyCode::Esc) => return Ok(()),
                // List-focus bindings.
                (SearchFocus::List, KeyCode::Char('q')) => return Ok(()),
                (SearchFocus::List, KeyCode::Up) => state.select_up(),
                (SearchFocus::List, KeyCode::Down) => state.select_down(),
                (SearchFocus::List, KeyCode::Char('r')) => {
                    let targets: Vec<String> = if state.is_all_selected() {
                        state.indexes.iter().map(|i| i.id.clone()).collect()
                    } else {
                        state
                            .selected_id()
                            .map(str::to_string)
                            .into_iter()
                            .collect()
                    };
                    if targets.is_empty() {
                        if state.is_all_selected() {
                            state.log.push("reindex (all): no indexes registered");
                        } else {
                            state.log.push("reindex: no index selected");
                        }
                    } else {
                        if state.is_all_selected() {
                            state
                                .log
                                .push(format!("reindex triggered: all {} indexes", targets.len()));
                        }
                        for id in targets {
                            state
                                .log
                                .push_scoped(&id, format!("reindex triggered: {id}"));
                            spawn_reindex(client.clone(), reindex_tx.clone(), id);
                        }
                    }
                }
                // Input-focus bindings.
                (SearchFocus::Input, KeyCode::Enter) => {
                    run_search(state, client).await;
                    poll_daemon(state, client).await;
                    last_poll = Instant::now();
                }
                (SearchFocus::Input, KeyCode::Backspace) => {
                    state.input.pop();
                }
                (SearchFocus::Input, KeyCode::Char(c)) => state.input.push(c),
                _ => {}
            }
        }

        if last_poll.elapsed() >= REFRESH_INTERVAL {
            poll_daemon(state, client).await;
            last_poll = Instant::now();
        }
    }
}

/// Spawn a background task streaming one index's reindex into `out`.
///
/// Why: `SearchClient::reindex_stream` emits bare [`ReindexEvent`]s, but the
/// event loop needs each tagged with its index id; this bridges a per-index
/// inner channel onto the loop's [`ScopedReindexEvent`] channel so several
/// indexes can reindex concurrently (the "All" fan-out) without losing track
/// of which event belongs to which index.
/// What: spawns the SSE streaming task plus a forwarding task that wraps every
/// [`ReindexEvent`] in a [`ScopedReindexEvent`] carrying `index_id`.
/// Test: side-effect-only (spawns tasks); the scoped event handling is
/// unit-tested via `test_apply_reindex_event`.
fn spawn_reindex(client: SearchClient, out: mpsc::Sender<ScopedReindexEvent>, index_id: String) {
    let (inner_tx, mut inner_rx) = mpsc::channel::<ReindexEvent>(64);
    let stream_id = index_id.clone();
    tokio::spawn(async move {
        client.reindex_stream(&stream_id, inner_tx).await;
    });
    tokio::spawn(async move {
        while let Some(event) = inner_rx.recv().await {
            let scoped = ScopedReindexEvent {
                index_id: index_id.clone(),
                event,
            };
            if out.send(scoped).await.is_err() {
                break; // event loop gone — stop forwarding.
            }
        }
    });
}

/// The body text for the help overlay, one binding per line.
///
/// Why: kept separate so a test can assert every binding is documented.
/// What: returns the multi-line help string.
/// Test: `test_help_text_lists_bindings`.
pub fn help_text() -> String {
    [
        "  Tab     switch focus between the index list and the search bar",
        "  ↑ / ↓   move the index selection (when the list has focus)",
        "  All     the top list row fans queries / stats across every index",
        "  r       reindex the selected index — or all, when 'All' is selected",
        "  Enter   run a search against the selected index — or all of them",
        "  ?       toggle this help overlay",
        "  q / Esc quit",
    ]
    .join("\n")
}

/// Compute the width of the left INDEXES panel for a given terminal width.
///
/// Why: the layout caps the index panel so the activity log gets the bulk of
/// the width on wide terminals, but a narrow terminal must still leave room.
/// What: returns `min(LEFT_PANEL_MAX, width / 3)`.
/// Test: `test_left_panel_width`.
pub fn left_panel_width(width: u16) -> u16 {
    LEFT_PANEL_MAX.min(width / 3)
}

/// One rendered row of the INDEXES panel.
///
/// Why: the renderer styles three row kinds differently — the "All" row is
/// bold, the selected row is highlighted, ordinary rows are plain — so the line
/// builder must surface which kind each row is rather than just a bool.
/// What: the row `text`, whether it is `selected`, and whether it is the
/// synthetic `is_all` ("All indexes") row.
/// Test: `test_index_lines`, `test_all_selector`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexListRow {
    /// The fully-formatted row text.
    pub text: String,
    /// Whether this row is the current selection.
    pub selected: bool,
    /// Whether this row is the synthetic "All indexes" entry.
    pub is_all: bool,
}

/// Build the rows for the INDEXES panel body.
///
/// Why: separating row construction from the ratatui widgets lets a test
/// assert the rendered content without a terminal backend.
/// What: returns the synthetic "All indexes" row first (carrying the summed
/// chunk count across every index), then one row per index — id, chunk count,
/// and a `✓` marker. With no indexes registered the "All" row is still shown
/// followed by a placeholder line.
/// Test: `test_index_lines`, `test_all_selector`.
pub fn index_lines(state: &SearchTuiState) -> Vec<IndexListRow> {
    let mut rows: Vec<IndexListRow> = Vec::with_capacity(state.indexes.len() + 1);

    // The synthetic "All indexes" row always leads the list.
    let total_chunks: u64 = state.indexes.iter().map(|i| i.chunk_count).sum();
    let all_selected = state.selected == 0;
    let all_marker = if all_selected { ">" } else { " " };
    rows.push(IndexListRow {
        text: format!(
            "{all_marker} {:<12} {:>8} ∗",
            truncate(ALL_LABEL, 12),
            format_count(total_chunks),
        ),
        selected: all_selected,
        is_all: true,
    });

    if state.indexes.is_empty() {
        rows.push(IndexListRow {
            text: "  (no indexes registered)".to_string(),
            selected: false,
            is_all: false,
        });
        return rows;
    }

    for (i, idx) in state.indexes.iter().enumerate() {
        // Row 0 is "All", so index `i` lives at cursor row `i + 1`.
        let row = i + 1;
        let selected = row == state.selected;
        let marker = if selected { ">" } else { " " };
        rows.push(IndexListRow {
            text: format!(
                "{marker} {:<12} {:>8} ✓",
                truncate(&idx.id, 12),
                format_count(idx.chunk_count),
            ),
            selected,
            is_all: false,
        });
    }
    rows
}

/// Build the STATISTICS panel lines for the current selection.
///
/// Why: the bottom-right panel shows counts and sizes for whichever index is
/// selected, or aggregate totals plus a per-index breakdown when "All" is
/// selected; isolating the builder makes the content testable without a
/// terminal.
/// What: for a single index, returns its id, chunk count, and indexed root
/// path. For the "All" selection, returns the index count, the summed chunk
/// count, and one `· <id>: <chunks>` breakdown line per index.
/// Test: `test_stats_lines`.
pub fn stats_lines(state: &SearchTuiState) -> Vec<String> {
    if state.is_all_selected() {
        let total: u64 = state.indexes.iter().map(|i| i.chunk_count).sum();
        let mut lines = vec![
            format!("Scope:        {ALL_LABEL}"),
            format!("Indexes:      {}", state.indexes.len()),
            format!("Total chunks: {}", format_count(total)),
        ];
        if state.indexes.is_empty() {
            lines.push("(no indexes registered)".to_string());
        } else {
            lines.push(String::new());
            for idx in &state.indexes {
                lines.push(format!(
                    "  · {:<14} {:>8}",
                    truncate(&idx.id, 14),
                    format_count(idx.chunk_count),
                ));
            }
        }
        return lines;
    }

    match state.indexes.get(state.selected.saturating_sub(1)) {
        Some(idx) => vec![
            format!("Index:        {}", idx.id),
            format!("Chunks:       {}", format_count(idx.chunk_count)),
            format!(
                "Root path:    {}",
                if idx.root_path.is_empty() {
                    "(unknown)"
                } else {
                    idx.root_path.as_str()
                }
            ),
        ],
        None => vec!["(no index selected)".to_string()],
    }
}

/// Truncate a string to `max` characters, appending an ellipsis when cut.
///
/// Why: index ids can be long; the fixed-width left panel needs bounded labels.
/// What: returns `s` unchanged when short enough, else its first `max - 1`
/// characters plus `…`.
/// Test: `test_truncate`.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Build the title-bar line for the search UI.
///
/// Why: the top row shows the daemon name, version, liveness badge, and uptime
/// at a glance; isolating it keeps `render` terse and the text testable.
/// What: returns `trusty-search vX  [●] <status>  uptime: Xh Ym` — uptime is
/// only shown when the daemon is online.
/// Test: `test_title_line`.
pub fn title_line(state: &SearchTuiState) -> String {
    let (glyph, label) = state.daemon_status.badge();
    match &state.daemon_status {
        DaemonStatus::Online { uptime_secs, .. } => format!(
            "trusty-search v{VERSION}  [{glyph}] {label}  uptime: {}",
            fmt_uptime(*uptime_secs)
        ),
        _ => format!(
            "trusty-search v{VERSION}  [{glyph}] {label}  {}",
            state.base_url
        ),
    }
}

/// Vertical split of the right-hand pane: ACTIVITY over STATISTICS.
///
/// Why: the right side shows two things — a live event feed and the selected
/// index's stats; isolating the split as a named constant keeps `render` terse
/// and documents the 60 / 40 ratio.
/// What: the ACTIVITY panel takes the top 60 %, STATISTICS the bottom 40 %.
const ACTIVITY_PERCENT: u16 = 60;

/// Draw the search TUI frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a 4-row vertical layout — title bar, the INDEXES / right-pane split,
/// the SEARCH input bar, and the key-hint footer. The right pane is itself
/// split vertically into an ACTIVITY feed (top 60 %) and a STATISTICS panel
/// (bottom 40 %), both scoped to the selected index — or aggregated when "All"
/// is selected. A centred help overlay floats on top when `show_help` is set.
/// Test: line content is unit-tested via the `*_lines` helpers; this glue is
/// exercised by `test_render_smoke`.
pub fn render(frame: &mut Frame, state: &SearchTuiState) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(4),    // panels
            Constraint::Length(3), // search input
            Constraint::Length(1), // key hint
        ])
        .split(area);

    // Title bar.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title_line(state),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );

    // INDEXES on the left, the ACTIVITY / STATISTICS stack on the right.
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_panel_width(area.width)),
            Constraint::Min(10),
        ])
        .split(rows[1]);

    let list_focused = state.focus == SearchFocus::List;
    let index_items: Vec<ListItem> = index_lines(state)
        .into_iter()
        .map(|row| {
            let style = if row.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.is_all {
                // The unselected "All" row stays distinct — bold yellow.
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(row.text, style)))
        })
        .collect();
    frame.render_widget(
        List::new(index_items).block(panel_block("INDEXES", list_focused)),
        split[0],
    );

    // Right pane: ACTIVITY (top) over STATISTICS (bottom).
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(ACTIVITY_PERCENT),
            Constraint::Percentage(100 - ACTIVITY_PERCENT),
        ])
        .split(split[1]);

    // ACTIVITY panel — the tail of the scoped feed that fits the panel height.
    let scope = state.scope_filter();
    let activity_title = match scope {
        Some(id) => format!("ACTIVITY — {id}"),
        None => format!("ACTIVITY — {ALL_LABEL}"),
    };
    let activity_height = right[0].height.saturating_sub(2) as usize;
    let activity_items: Vec<ListItem> = if state.log.has_scoped(scope) {
        state
            .log
            .tail_scoped(scope, activity_height.max(1))
            .map(|line| ListItem::new(line.as_str()))
            .collect()
    } else {
        vec![ListItem::new("(no activity yet)")]
    };
    frame.render_widget(
        List::new(activity_items).block(panel_block(&activity_title, false)),
        right[0],
    );

    // STATISTICS panel — counts and sizes for the selection.
    let stats_items: Vec<ListItem> = stats_lines(state).into_iter().map(ListItem::new).collect();
    frame.render_widget(
        List::new(stats_items).block(panel_block("STATISTICS", false)),
        right[1],
    );

    // SEARCH input bar.
    let input_focused = state.focus == SearchFocus::Input;
    let cursor = if input_focused { "_" } else { "" };
    let input_style = if input_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("SEARCH ▶ ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{}{cursor}", state.input), input_style),
        ]))
        .block(panel_block("SEARCH", input_focused)),
        rows[2],
    );

    // Key-hint footer.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            KEY_HINT,
            Style::default().fg(Color::DarkGray),
        ))),
        rows[3],
    );

    if state.show_help {
        render_help_overlay(frame);
    }
}

/// Build a bordered block for a UI panel, highlighting it when focused.
///
/// Why: the focused panel must be visually distinct; both panels share this.
/// What: returns a [`Block`] titled `name` with a thick cyan border when
/// `focused`, a dim gray border otherwise.
fn panel_block(name: &str, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            format!(" {name} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
}

/// Render the centred help overlay listing every key binding.
///
/// Why: the `?` key shows a floating reference of every binding.
/// What: clears a centred rectangle and draws [`help_text`] in a block.
fn render_help_overlay(frame: &mut Frame) {
    let area = frame.area();
    let w = 60.min(area.width);
    let h = 9.min(area.height);
    let rect = ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(help_text())
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Help — press ? or Esc to close "),
            ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::utils::timestamped;
    use ratatui::{Terminal, backend::TestBackend};

    /// A state with three indexes for selection / rendering tests.
    fn sample_state() -> SearchTuiState {
        let mut state = SearchTuiState::new("http://127.0.0.1:7878");
        state.daemon_status = DaemonStatus::Online {
            version: "0.3.65".into(),
            uptime_secs: 7440,
        };
        state.indexes = vec![
            IndexRow {
                id: "cto".into(),
                chunk_count: 1_200,
                root_path: "/tmp/cto".into(),
            },
            IndexRow {
                id: "trusty".into(),
                chunk_count: 18_994,
                root_path: "/tmp/trusty".into(),
            },
            IndexRow {
                id: "duetto".into(),
                chunk_count: 900,
                root_path: "/tmp/duetto".into(),
            },
        ];
        state
    }

    #[test]
    fn test_new_state_defaults() {
        let state = SearchTuiState::new("http://127.0.0.1:7878");
        assert_eq!(state.base_url, "http://127.0.0.1:7878");
        assert!(matches!(state.daemon_status, DaemonStatus::Connecting));
        assert!(state.indexes.is_empty());
        assert_eq!(state.selected, 0);
        assert!(state.log.is_empty());
        assert!(state.input.is_empty());
        assert_eq!(state.focus, SearchFocus::List);
        assert!(!state.show_help);
    }

    #[test]
    fn test_toggle_focus() {
        let mut state = SearchTuiState::new("http://x");
        assert_eq!(state.focus, SearchFocus::List);
        state.toggle_focus();
        assert_eq!(state.focus, SearchFocus::Input);
        state.toggle_focus();
        assert_eq!(state.focus, SearchFocus::List);
    }

    #[test]
    fn test_selected_clamp() {
        let mut state = sample_state();
        // The list has 1 ("All") + 3 indexes = 4 rows; the cursor stops at 3.
        for _ in 0..10 {
            state.select_down();
        }
        assert_eq!(state.selected, 3, "clamped to indexes.len()");
        // select_up saturates at zero (the "All" row).
        for _ in 0..10 {
            state.select_up();
        }
        assert_eq!(state.selected, 0);
        // A shrunk index list re-clamps the cursor (1 "All" + 1 index = row 1).
        state.selected = 3;
        state.indexes.truncate(1);
        state.clamp_selection();
        assert_eq!(state.selected, 1);
        // An empty list leaves only the "All" row at cursor 0.
        state.indexes.clear();
        state.selected = 5;
        state.clamp_selection();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_selected_id() {
        let mut state = sample_state();
        // Cursor 0 is "All" — no single index.
        assert!(state.is_all_selected());
        assert_eq!(state.selected_id(), None);
        // Cursor 1 is the first index.
        state.select_down();
        assert_eq!(state.selected_id(), Some("cto"));
        state.select_down();
        assert_eq!(state.selected_id(), Some("trusty"));
        state.indexes.clear();
        state.clamp_selection();
        assert_eq!(state.selected_id(), None);
    }

    #[test]
    fn test_all_selector() {
        let mut state = sample_state();
        // The default selection is the "All indexes" row.
        assert!(state.is_all_selected());
        assert_eq!(state.scope_filter(), None);
        // Moving down off row 0 picks a single index and a scoped filter.
        state.select_down();
        assert!(!state.is_all_selected());
        assert_eq!(state.scope_filter(), Some("cto"));
        // Moving back up returns to "All".
        state.select_up();
        assert!(state.is_all_selected());
        assert_eq!(state.scope_filter(), None);

        // The index list always leads with the "All" row.
        let rows = index_lines(&state);
        assert_eq!(rows.len(), 4, "1 'All' row + 3 indexes");
        assert!(rows[0].is_all);
        assert!(rows[0].text.contains(ALL_LABEL));
        assert!(rows[0].selected, "'All' is selected by default");
        assert!(!rows[1].is_all);
        assert!(rows[1].text.contains("cto"));
    }

    #[test]
    fn test_stats_lines() {
        let mut state = sample_state();
        // "All" selected → aggregate totals + a per-index breakdown.
        let all = stats_lines(&state);
        assert!(
            all.iter()
                .any(|l| l.contains("Indexes:") && l.contains('3'))
        );
        // 1,200 + 18,994 + 900 = 21,094 → abbreviated as 21.1k.
        assert!(all.iter().any(|l| l.contains("Total chunks:")));
        assert!(all.iter().any(|l| l.contains("cto")));
        assert!(all.iter().any(|l| l.contains("trusty")));

        // A single index selected → that index's detail.
        state.select_down(); // cursor 1 → cto
        let one = stats_lines(&state);
        assert!(
            one.iter()
                .any(|l| l.contains("Index:") && l.contains("cto"))
        );
        assert!(
            one.iter()
                .any(|l| l.contains("Chunks:") && l.contains("1,200"))
        );
        assert!(one.iter().any(|l| l.contains("/tmp/cto")));
    }

    #[test]
    fn test_log_append() {
        // The activity log is a bounded VecDeque capped at MAX_ENTRIES; the
        // search UI shares it via `state.log`.
        let mut state = SearchTuiState::new("http://x");
        for i in 0..(ActivityLog::MAX_ENTRIES + 50) {
            state.log.push(format!("event {i}"));
        }
        assert_eq!(state.log.len(), ActivityLog::MAX_ENTRIES);
    }

    #[test]
    fn test_timestamped_format() {
        // `timestamped` is the shared helper the activity log uses; assert the
        // `[HH:MM:SS] ...` shape it produces.
        let line = timestamped("reindex started");
        assert!(line.starts_with('['));
        assert!(line.ends_with(" reindex started"));
        assert_eq!(line.as_bytes()[9], b']');
    }

    /// Tag a [`ReindexEvent`] with a test index id.
    fn scoped(event: ReindexEvent) -> ScopedReindexEvent {
        ScopedReindexEvent {
            index_id: "cto".into(),
            event,
        }
    }

    #[test]
    fn test_apply_reindex_event() {
        let mut state = SearchTuiState::new("http://x");
        apply_reindex_event(
            &mut state,
            scoped(ReindexEvent::Started { total_files: 1200 }),
        );
        apply_reindex_event(
            &mut state,
            scoped(ReindexEvent::Progress {
                indexed: 600,
                total_files: 1200,
            }),
        );
        apply_reindex_event(
            &mut state,
            scoped(ReindexEvent::Complete {
                total_chunks: 19_012,
                status: "complete".into(),
            }),
        );
        let lines: Vec<&String> = state.log.iter().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("reindex started: 1200 files"));
        assert!(lines[1].contains("600/1200 (50%)"));
        assert!(lines[2].contains("reindex complete: 19.0k chunks"));

        // The events are scoped to the "cto" index, so the per-index activity
        // feed keeps them and a different index's feed does not.
        assert_eq!(state.log.tail_scoped(Some("cto"), 100).count(), 3);
        assert_eq!(state.log.tail_scoped(Some("trusty"), 100).count(), 0);

        // A failed event records an error line.
        apply_reindex_event(&mut state, scoped(ReindexEvent::Failed("disk full".into())));
        assert!(
            state
                .log
                .iter()
                .last()
                .expect("entry")
                .contains("reindex error: disk full")
        );
    }

    #[test]
    fn test_left_panel_width() {
        // Wide terminals cap the panel at LEFT_PANEL_MAX.
        assert_eq!(left_panel_width(200), LEFT_PANEL_MAX);
        // Narrow terminals get a third of the width.
        assert_eq!(left_panel_width(60), 20);
    }

    #[test]
    fn test_index_lines() {
        let state = sample_state();
        let rows = index_lines(&state);
        // 1 "All" row + 3 index rows.
        assert_eq!(rows.len(), 4);
        // Row 0 is "All", selected by default, and bold-marked with `>`.
        assert!(rows[0].is_all);
        assert!(rows[0].selected);
        assert!(rows[0].text.starts_with('>'));
        assert!(rows[0].text.contains(ALL_LABEL));
        // Row 1 is the first index, unselected.
        assert!(!rows[1].is_all && !rows[1].selected);
        assert!(rows[1].text.contains("cto"));
        // Row 2 carries its chunk count.
        assert!(rows[2].text.contains("trusty"));
        assert!(rows[2].text.contains("19.0k"));

        // An empty index list still shows the "All" row plus a placeholder.
        let empty = SearchTuiState::new("http://x");
        let rows = index_lines(&empty);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_all);
        assert!(rows[1].text.contains("no indexes"));
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 12), "short");
        assert_eq!(truncate("a-very-long-index-id", 8), "a-very-…");
    }

    #[test]
    fn test_title_line() {
        let state = sample_state();
        let title = title_line(&state);
        assert!(title.contains("trusty-search v"));
        assert!(title.contains("online"));
        assert!(title.contains("uptime: 2h 4m"));

        // An offline daemon shows its URL rather than uptime.
        let mut offline = SearchTuiState::new("http://127.0.0.1:7878");
        offline.daemon_status = DaemonStatus::Offline {
            last_error: "refused".into(),
        };
        let title = title_line(&offline);
        assert!(title.contains("offline"));
        assert!(title.contains("http://127.0.0.1:7878"));
    }

    #[test]
    fn test_help_text_lists_bindings() {
        let text = help_text();
        for token in ["Tab", "r ", "Enter", "?", "q "] {
            assert!(text.contains(token), "help text missing {token}");
        }
    }

    #[test]
    fn test_render_smoke() {
        // A full render in several states must not panic — exercise both the
        // "All" selection (aggregated panels) and a single-index selection.
        let mut state = sample_state();
        state.log.push("daemon started");
        state.log.push_scoped("cto", "reindex started: 1200 files");
        state
            .log
            .push_scoped("trusty", "search \"fn embed\" → 5 results");
        state.input = "fn authenticate".into();
        state.focus = SearchFocus::Input;
        for (w, h) in [(120u16, 30u16), (80, 24)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| render(f, &state))
                .expect("render (All) must not panic");
        }
        // Single-index selection — the right panels scope to that index.
        state.selected = 1;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &state))
            .expect("render (single index) must not panic");

        // Help overlay and offline daemon paths.
        state.show_help = true;
        state.daemon_status = DaemonStatus::Connecting;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &state))
            .expect("help render must not panic");
    }
}
