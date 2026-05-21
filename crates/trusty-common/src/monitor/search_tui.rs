//! Service-specific terminal UI for the trusty-search daemon.
//!
//! Why: operators of the trusty-search daemon want a focused, live terminal
//! surface — an index list, a streaming activity log of reindex/search events,
//! and a query bar — rather than the generic two-daemon dashboard. Living in
//! `trusty-common` behind the `monitor-tui` feature keeps the pure state /
//! rendering testable without a separate published crate (issue #34).
//! What: a ratatui app with a 3-zone layout (title bar, INDEXES + ACTIVITY
//! split, SEARCH input bar). It polls the daemon every 2 seconds, streams
//! reindex progress over SSE on `[r]`, and runs hybrid searches from the input
//! bar on `[Enter]`. Input is polled every 50 ms so keys feel instant.
//! Test: `cargo test -p trusty-common --features monitor-tui` covers the pure
//! state, log capacity, and selection clamp; `trusty-search monitor tui`
//! launches the live UI.

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
/// Test: `test_selected_clamp`, `test_toggle_focus`, `test_log_append`.
#[derive(Debug, Clone)]
pub struct SearchTuiState {
    /// The trusty-search daemon base URL being monitored.
    pub base_url: String,
    /// The daemon's current liveness state.
    pub daemon_status: DaemonStatus,
    /// One row per registered index.
    pub indexes: Vec<IndexRow>,
    /// Cursor into [`Self::indexes`] for the selected row.
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
    /// What: increments [`Self::selected`] but never past the last row.
    /// Test: `test_selected_clamp`.
    pub fn select_down(&mut self) {
        let last = self.indexes.len().saturating_sub(1);
        if self.selected < last {
            self.selected += 1;
        }
    }

    /// Clamp the selection cursor to the current index count.
    ///
    /// Why: a poll can shrink the index list (an index was deleted) leaving the
    /// cursor past the end; this keeps it valid before rendering.
    /// What: caps [`Self::selected`] at `indexes.len().saturating_sub(1)`; an
    /// empty list resets the cursor to zero.
    /// Test: `test_selected_clamp`.
    pub fn clamp_selection(&mut self) {
        let last = self.indexes.len().saturating_sub(1);
        if self.selected > last {
            self.selected = last;
        }
    }

    /// The id of the currently selected index, if any.
    ///
    /// Why: `[r]` reindexes and `[Enter]` searches the selected index; both
    /// need its id.
    /// What: returns `Some(id)` for the row at [`Self::selected`], or `None`
    /// when the index list is empty.
    /// Test: `test_selected_id`.
    pub fn selected_id(&self) -> Option<&str> {
        self.indexes.get(self.selected).map(|i| i.id.as_str())
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

/// Run a search against the selected index and append the hits to the log.
///
/// Why: pressing `[Enter]` in the query bar runs a hybrid search; the operator
/// sees the results inline in the ACTIVITY panel.
/// What: calls `client.search`, appends a `search "<q>" → N results` summary
/// plus one indented `path:line  snippet` continuation line per hit. An empty
/// query or absent index is a no-op note; a transport error is logged.
/// Test: thin I/O glue; result projection is tested in `search_client`.
async fn run_search(state: &mut SearchTuiState, client: &SearchClient) {
    let query = state.input.trim().to_string();
    if query.is_empty() {
        return;
    }
    let Some(id) = state.selected_id().map(str::to_string) else {
        state.log.push("search: no index selected");
        state.input.clear();
        return;
    };
    match client.search(&id, &query, SEARCH_TOP_K).await {
        Ok(hits) => {
            state
                .log
                .push(format!("search \"{query}\" → {} results", hits.len()));
            for hit in &hits {
                state
                    .log
                    .push_raw(format!("  {}:{}  {}", hit.file, hit.line, hit.snippet));
            }
        }
        Err(e) => state.log.push(format!("search \"{query}\" failed: {e}")),
    }
    state.input.clear();
}

/// Apply one streamed reindex event to the activity log.
///
/// Why: the reindex SSE task forwards [`ReindexEvent`]s through a channel; the
/// event loop drains them and this turns each into a human-readable log line.
/// What: `Started` / `Progress` / `Complete` / `Failed` each map to a distinct
/// timestamped line, with progress carrying a percent-complete figure.
/// Test: `test_apply_reindex_event`.
pub fn apply_reindex_event(state: &mut SearchTuiState, event: ReindexEvent) {
    match event {
        ReindexEvent::Started { total_files } => {
            state
                .log
                .push(format!("reindex started: {total_files} files"));
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
                .push(format!("indexing: {indexed}/{total_files} ({pct}%)"));
        }
        ReindexEvent::Complete {
            total_chunks,
            status,
        } => {
            state.log.push(format!(
                "reindex {status}: {} chunks",
                format_count(total_chunks)
            ));
        }
        ReindexEvent::Failed(message) => {
            state.log.push(format!("reindex error: {message}"));
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

    // Channel for reindex SSE events forwarded by a background task.
    let (reindex_tx, mut reindex_rx) = mpsc::channel::<ReindexEvent>(64);

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
                    if let Some(id) = state.selected_id().map(str::to_string) {
                        state.log.push(format!("reindex triggered: {id}"));
                        let stream_client = client.clone();
                        let tx = reindex_tx.clone();
                        tokio::spawn(async move {
                            stream_client.reindex_stream(&id, tx).await;
                        });
                    } else {
                        state.log.push("reindex: no index selected");
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

/// The body text for the help overlay, one binding per line.
///
/// Why: kept separate so a test can assert every binding is documented.
/// What: returns the multi-line help string.
/// Test: `test_help_text_lists_bindings`.
pub fn help_text() -> String {
    [
        "  Tab     switch focus between the index list and the search bar",
        "  ↑ / ↓   move the index selection (when the list has focus)",
        "  r       reindex the selected index (streams live progress)",
        "  Enter   run a search against the selected index (search bar)",
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

/// Build the lines for the INDEXES panel body.
///
/// Why: separating line construction from the ratatui widgets lets a test
/// assert the rendered content without a terminal backend.
/// What: returns one `(text, selected)` pair per index — id, chunk count, and
/// a `✓` marker — or a single placeholder line when there are no indexes.
/// Test: `test_index_lines`.
pub fn index_lines(state: &SearchTuiState) -> Vec<(String, bool)> {
    if state.indexes.is_empty() {
        return vec![("(no indexes registered)".to_string(), false)];
    }
    state
        .indexes
        .iter()
        .enumerate()
        .map(|(i, idx)| {
            let marker = if i == state.selected { ">" } else { " " };
            let text = format!(
                "{marker} {:<12} {:>8} ✓",
                truncate(&idx.id, 12),
                format_count(idx.chunk_count),
            );
            (text, i == state.selected)
        })
        .collect()
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

/// Draw the search TUI frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a 4-row vertical layout — title bar, the INDEXES/ACTIVITY split, the
/// SEARCH input bar, and the key-hint footer. A centred help overlay floats on
/// top when `show_help` is set.
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

    // INDEXES + ACTIVITY split.
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
        .map(|(text, selected)| {
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();
    frame.render_widget(
        List::new(index_items).block(panel_block("INDEXES", list_focused)),
        split[0],
    );

    // ACTIVITY panel — show the tail that fits the panel height.
    let activity_height = split[1].height.saturating_sub(2) as usize;
    let activity_items: Vec<ListItem> = if state.log.is_empty() {
        vec![ListItem::new("(no activity yet)")]
    } else {
        state
            .log
            .tail(activity_height.max(1))
            .map(|line| ListItem::new(line.as_str()))
            .collect()
    };
    frame.render_widget(
        List::new(activity_items).block(panel_block("ACTIVITY", false)),
        split[1],
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
        // select_down stops at the last index, never past it.
        for _ in 0..10 {
            state.select_down();
        }
        assert_eq!(state.selected, 2, "clamped to indexes.len() - 1");
        // select_up saturates at zero.
        for _ in 0..10 {
            state.select_up();
        }
        assert_eq!(state.selected, 0);
        // A shrunk index list re-clamps the cursor.
        state.selected = 2;
        state.indexes.truncate(1);
        state.clamp_selection();
        assert_eq!(state.selected, 0);
        // An empty list resets the cursor to zero.
        state.indexes.clear();
        state.selected = 5;
        state.clamp_selection();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_selected_id() {
        let mut state = sample_state();
        assert_eq!(state.selected_id(), Some("cto"));
        state.select_down();
        assert_eq!(state.selected_id(), Some("trusty"));
        state.indexes.clear();
        state.clamp_selection();
        assert_eq!(state.selected_id(), None);
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

    #[test]
    fn test_apply_reindex_event() {
        let mut state = SearchTuiState::new("http://x");
        apply_reindex_event(&mut state, ReindexEvent::Started { total_files: 1200 });
        apply_reindex_event(
            &mut state,
            ReindexEvent::Progress {
                indexed: 600,
                total_files: 1200,
            },
        );
        apply_reindex_event(
            &mut state,
            ReindexEvent::Complete {
                total_chunks: 19_012,
                status: "complete".into(),
            },
        );
        let lines: Vec<&String> = state.log.iter().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("reindex started: 1200 files"));
        assert!(lines[1].contains("600/1200 (50%)"));
        assert!(lines[2].contains("reindex complete: 19.0k chunks"));

        // A failed event records an error line.
        apply_reindex_event(&mut state, ReindexEvent::Failed("disk full".into()));
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
        let lines = index_lines(&state);
        assert_eq!(lines.len(), 3);
        // The selected (first) row is marked.
        assert!(lines[0].0.contains("cto") && lines[0].1);
        assert!(lines[0].0.starts_with('>'));
        assert!(lines[1].0.contains("trusty") && !lines[1].1);
        assert!(lines[1].0.contains("19.0k"));

        // An empty index list shows a placeholder.
        let empty = SearchTuiState::new("http://x");
        let lines = index_lines(&empty);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].0.contains("no indexes"));
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
        // A full render in several states must not panic.
        let mut state = sample_state();
        state.log.push("reindex started: 1200 files");
        state.log.push("search \"fn embed\" → 5 results");
        state.input = "fn authenticate".into();
        state.focus = SearchFocus::Input;
        for (w, h) in [(120u16, 30u16), (80, 24)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| render(f, &state))
                .expect("render must not panic");
        }
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
