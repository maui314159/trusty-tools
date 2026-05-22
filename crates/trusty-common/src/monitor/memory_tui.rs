//! Service-specific terminal UI for the trusty-memory daemon.
//!
//! Why: operators of the trusty-memory daemon want a focused terminal surface
//! — a palace list, a streaming activity log of dream / drawer / recall
//! events, and a recall query bar — rather than the generic two-daemon
//! dashboard. Living in `trusty-common` behind the `monitor-tui` feature keeps
//! the pure state / rendering testable (issue #34).
//! What: a ratatui app with a 3-zone layout (title bar, PALACES + right-hand
//! split, RECALL input bar). The PALACES list always leads with an "All
//! palaces" entry that fans recalls out across every palace; the right side is
//! split vertically into an ACTIVITY feed (top) and a STATISTICS panel
//! (bottom), both scoped to the selected palace — or aggregated when "All" is
//! selected. It polls the daemon every 2 seconds, subscribes to the `/sse`
//! event stream on startup, runs cross-palace recalls from the input bar on
//! `[Enter]`, and triggers a dream cycle on `[d]`.
//! Test: `cargo test -p trusty-common --features monitor-tui` covers the pure
//! state, the "All" selector, palace row formatting, the activity / statistics
//! line builders, and dream-event logging; `trusty-memory monitor tui`
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
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use tokio::sync::mpsc;

use crate::monitor::dashboard::{MemoryData, PalaceRow, format_count};
use crate::monitor::memory_client::{MemoryClient, MemoryEvent, RecallHit, resolve_memory_url};
use crate::monitor::utils::{ActivityLog, DaemonStatus};

/// Data-refresh interval: how often the daemon is polled.
const REFRESH_INTERVAL: Duration = Duration::from_millis(2000);

/// Input-poll interval: how often the keyboard is checked.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Number of results requested per recall query.
const RECALL_TOP_K: usize = 5;

/// Maximum width (in columns) of the left PALACES panel.
const LEFT_PANEL_MAX: u16 = 28;

/// Crate version, surfaced in the title bar.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line key hint shown along the bottom of the UI.
pub const KEY_HINT: &str = "[Tab] focus  [d] dream  [↑↓] select  [Enter] recall  [/] filter  [s] sort  [g] group  [q] quit  [?] help";

/// Sort order applied to the palace list.
///
/// Why: operators want to flip between most-recently-active, alphabetical, and
/// vector-count-heavy views without leaving the TUI; a small enum keeps the
/// renderer and the key handler agreed on the available options.
/// What: `Activity` (default — last write desc, vectors as tiebreak), `Name`
/// (alphabetical asc), `Vectors` (vector_count desc).
/// Test: `test_palace_sort_key_cycle`, `test_apply_sort_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PalaceSortKey {
    /// Sort by last write timestamp desc; vector_count desc as tiebreak.
    #[default]
    Activity,
    /// Sort alphabetically by palace name (ascending).
    Name,
    /// Sort by vector_count desc.
    Vectors,
}

impl PalaceSortKey {
    /// Advance to the next sort key in the cycle.
    ///
    /// Why: `[s]` cycles through the three sort orders.
    /// What: `Activity → Name → Vectors → Activity`.
    /// Test: `test_palace_sort_key_cycle`.
    pub fn next(self) -> Self {
        match self {
            Self::Activity => Self::Name,
            Self::Name => Self::Vectors,
            Self::Vectors => Self::Activity,
        }
    }

    /// One-word label for the status bar / panel title.
    ///
    /// Why: the panel header shows the current sort key.
    /// What: returns `"Activity"`, `"Name"`, or `"Vectors"`.
    /// Test: `test_palace_sort_key_cycle`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Activity => "Activity",
            Self::Name => "Name",
            Self::Vectors => "Vectors",
        }
    }
}

/// Label for the synthetic "All palaces" entry at the top of the list.
///
/// Why: selecting it fans recalls / stats out across every palace; a single
/// constant keeps the label consistent between the list and the panel titles.
/// What: the display text of the palace list's first row.
/// Test: `test_palace_lines` asserts this is the first row.
pub const ALL_LABEL: &str = "All palaces";

/// Which zone of the memory UI currently holds keyboard focus.
///
/// Why: `[Tab]` cycles focus; the palace list and the recall bar consume keys
/// differently (navigation vs. text entry).
/// What: `List` (the default — the PALACES panel) or `Input` (the RECALL bar).
/// Test: `test_toggle_focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryFocus {
    /// The PALACES list panel has focus; arrows move the selection.
    #[default]
    List,
    /// The RECALL input bar has focus; typed characters edit the query.
    Input,
}

/// All mutable state the memory UI renders and mutates.
///
/// Why: the event loop polls the daemon, streams `/sse` events, and handles
/// input — keeping every piece of state in one struct keeps the loop terse and
/// the rendering a pure function of this snapshot.
/// What: the daemon URL and status, the aggregate stats, the palace list and
/// selection cursor, the scroll offset of the palace panel, the bounded
/// activity log, the query buffer, the focused zone, and the help flag. The
/// selection cursor addresses a list whose first row is the synthetic "All
/// palaces" entry, so cursor `0` means "All" and cursor `n` (n ≥ 1) means
/// `palaces[n - 1]`.
/// Test: `test_selected_clamp`, `test_toggle_focus`, `test_palace_row_display`,
/// `test_all_selector`, `test_scroll_offset`.
#[derive(Debug, Clone)]
pub struct MemoryTuiState {
    /// The trusty-memory daemon base URL being monitored.
    pub base_url: String,
    /// The daemon's current liveness state.
    pub daemon_status: DaemonStatus,
    /// The latest aggregate stats, or `None` before the first poll.
    pub status: Option<MemoryData>,
    /// One row per palace.
    pub palaces: Vec<PalaceRow>,
    /// Cursor into the palace list, where row `0` is the "All palaces" entry
    /// and row `n` (n ≥ 1) selects `palaces[n - 1]`.
    pub selected: usize,
    /// Index of the first row drawn in the PALACES panel — the scroll offset
    /// that keeps [`Self::selected`] on screen when the list overflows.
    pub scroll_offset: usize,
    /// Bounded, timestamped log of dream / drawer / recall activity.
    pub log: ActivityLog,
    /// The in-progress recall query buffer.
    pub input: String,
    /// Which zone currently holds keyboard focus.
    pub focus: MemoryFocus,
    /// Whether the help overlay is visible (toggled with `?`).
    pub show_help: bool,
    /// Case-insensitive filter applied to palace name / project; empty disables.
    pub filter: String,
    /// Whether the inline filter bar is focused (captures typed chars).
    pub filter_active: bool,
    /// Current palace-list sort order.
    pub sort_key: PalaceSortKey,
    /// Whether the palace list is grouped by inferred project.
    pub group_by_project: bool,
}

impl MemoryTuiState {
    /// Build a fresh memory UI state targeting `base_url`.
    ///
    /// Why: the event loop seeds the state at startup before the first poll.
    /// What: stores the URL, sets the daemon `Connecting`, and starts with no
    /// stats, an empty palace list, empty log, empty query, and list focus.
    /// Test: `test_new_state_defaults`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            daemon_status: DaemonStatus::Connecting,
            status: None,
            palaces: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            log: ActivityLog::new(),
            input: String::new(),
            focus: MemoryFocus::List,
            show_help: false,
            filter: String::new(),
            filter_active: false,
            sort_key: PalaceSortKey::default(),
            group_by_project: false,
        }
    }

    /// Cycle keyboard focus between the palace list and the recall bar.
    ///
    /// Why: `[Tab]` decides whether arrows navigate the list or whether typed
    /// characters edit the recall query.
    /// What: flips [`Self::focus`] between [`MemoryFocus::List`] and
    /// [`MemoryFocus::Input`].
    /// Test: `test_toggle_focus`.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            MemoryFocus::List => MemoryFocus::Input,
            MemoryFocus::Input => MemoryFocus::List,
        };
    }

    /// Move the palace selection up one row, saturating at the top.
    ///
    /// Why: `↑` navigates the PALACES list when it has focus.
    /// What: decrements [`Self::selected`], never below zero.
    /// Test: `test_selected_clamp`.
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the palace selection down one row, clamped to the last palace.
    ///
    /// Why: `↓` navigates the PALACES list when it has focus.
    /// What: increments [`Self::selected`] but never past the last row. The
    /// list has `palaces.len() + 1` rows (row 0 is "All palaces").
    /// Test: `test_selected_clamp`.
    pub fn select_down(&mut self) {
        if self.selected < self.last_row() {
            self.selected += 1;
        }
    }

    /// The index of the last selectable row.
    ///
    /// Why: the list always carries the synthetic "All" row, so the last valid
    /// cursor is `palaces.len()` (not `palaces.len() - 1`).
    /// What: returns `palaces.len()` — row 0 is "All", rows `1..=len` are the
    /// individual palaces.
    /// Test: `test_selected_clamp`.
    fn last_row(&self) -> usize {
        self.palaces.len()
    }

    /// Clamp the selection cursor to the current palace count.
    ///
    /// Why: a poll can shrink the palace list leaving the cursor past the end;
    /// this keeps it valid before rendering.
    /// What: caps [`Self::selected`] at `palaces.len()` (the "All" row plus one
    /// row per palace).
    /// Test: `test_selected_clamp`.
    pub fn clamp_selection(&mut self) {
        if self.selected > self.last_row() {
            self.selected = self.last_row();
        }
    }

    /// Recompute the scroll offset so the selected row fits a `visible` window.
    ///
    /// Why: the PALACES panel is a fixed-height viewport; when the list has
    /// more rows than fit, the panel must scroll so [`Self::selected`] is never
    /// drawn off-screen — otherwise `↑`/`↓` appear to do nothing past the edge.
    /// What: given the panel's visible row count, shifts [`Self::scroll_offset`]
    /// down when the cursor falls below the window and up when it rises above
    /// it, leaving it untouched while the cursor is already in view. A zero
    /// `visible` is treated as one row so the offset always tracks the cursor.
    /// Test: `test_scroll_offset`.
    pub fn sync_scroll(&mut self, visible: usize) {
        let window = visible.max(1);
        if self.selected >= self.scroll_offset + window {
            self.scroll_offset = self.selected + 1 - window;
        } else if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    /// Whether the "All palaces" entry is currently selected.
    ///
    /// Why: when "All" is selected the UI fans recalls out across every palace
    /// and aggregates the activity feed and statistics.
    /// What: returns `true` exactly when the cursor is on row 0.
    /// Test: `test_all_selector`.
    pub fn is_all_selected(&self) -> bool {
        self.selected == 0
    }

    /// The id of the currently selected single palace, if any.
    ///
    /// Why: `[Enter]` recalls and the log labels the selected palace; neither
    /// applies to a single palace when "All" is selected.
    /// What: returns `Some(id)` for the palace at cursor row `n ≥ 1`, or `None`
    /// when "All" is selected or the palace list is empty.
    /// Test: `test_selected_id`.
    pub fn selected_id(&self) -> Option<&str> {
        if self.selected == 0 {
            return None;
        }
        self.palaces.get(self.selected - 1).map(|p| p.id.as_str())
    }

    /// The scope filter for the activity feed and statistics panels.
    ///
    /// Why: the right-hand panels render the selected palace's events / stats,
    /// or every palace's when "All" is selected; this folds the cursor into the
    /// `Option<&str>` filter [`ActivityLog::tail_scoped`] expects.
    /// What: returns `None` when "All" is selected (un-filtered) or `Some(id)`
    /// for the selected single palace.
    /// Test: `test_all_selector`.
    pub fn scope_filter(&self) -> Option<&str> {
        self.selected_id()
    }
}

/// Run the trusty-memory monitor TUI.
///
/// Why: the single entry point the `monitor tui` subcommand of `trusty-memory`
/// calls.
/// What: resolves the daemon URL from the service lock file and delegates to
/// [`run_with_url`].
/// Test: the pure pieces are unit-tested; this thin glue is exercised by
/// launching the UI.
pub async fn run() -> anyhow::Result<()> {
    run_with_url(resolve_memory_url()).await
}

/// Run the memory TUI against an explicit daemon URL.
///
/// Why: separated from [`run`] so a future CLI flag can override the resolved
/// address, and so terminal setup/teardown lives in one place.
/// What: builds the client and state, enters raw mode + the alternate screen,
/// runs [`run_loop`], and unconditionally restores the terminal even on error.
/// Test: terminal glue is exercised by launching the UI.
pub async fn run_with_url(base_url: String) -> anyhow::Result<()> {
    let mut client = MemoryClient::new(base_url.clone());
    let mut state = MemoryTuiState::new(base_url);

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

/// Poll the trusty-memory daemon and fold the result into `state`.
///
/// Why: keeps the per-poll I/O out of the event loop so the loop can re-poll
/// on demand as well as on its timer.
/// What: re-resolves the URL when the daemon is offline, calls `fetch_all`, and
/// updates the status, aggregate stats, palace list, and selection clamp.
/// Test: thin I/O glue; the pure clamp is unit-tested.
async fn poll_daemon(state: &mut MemoryTuiState, client: &mut MemoryClient) {
    if !state.daemon_status.is_online() {
        let resolved = resolve_memory_url();
        if resolved != client.base_url() {
            client.set_base_url(resolved.clone());
            state.base_url = resolved;
        }
    }
    match client.fetch_all().await {
        Ok(data) => {
            state.daemon_status = DaemonStatus::Online {
                version: data.version.clone(),
                uptime_secs: 0,
            };
            state.palaces = data.palaces.clone();
            state.status = Some(data);
            state.clamp_selection();
        }
        Err(e) => {
            state.daemon_status = DaemonStatus::Offline {
                last_error: e.to_string(),
            };
        }
    }
}

/// Run a recall and append the hits to the activity log.
///
/// Why: pressing `[Enter]` in the recall bar runs a memory recall; the
/// operator sees the results inline in the ACTIVITY panel. The recall endpoint
/// is inherently cross-palace, so when a single palace is selected the hits
/// are filtered to that palace; when "All palaces" is selected every hit is
/// shown.
/// What: calls `client.recall`, then — for the "All" selection — appends a
/// daemon-wide `recall "<q>" → N results` summary plus one `palace_id`-scoped
/// `· [palace] snippet` continuation per hit. For a single palace it appends a
/// palace-scoped summary counting only that palace's hits and a continuation
/// per kept hit. An empty query is a no-op; transport errors are logged scoped
/// to the selection.
/// Test: thin I/O glue; result projection is tested in `memory_client`.
async fn run_recall(state: &mut MemoryTuiState, client: &MemoryClient) {
    let query = state.input.trim().to_string();
    if query.is_empty() {
        return;
    }
    let scope = state.selected_id().map(str::to_string);
    match client.recall(&query, RECALL_TOP_K).await {
        Ok(hits) => match &scope {
            // "All palaces": one daemon-wide summary, each hit scoped to its
            // own palace so the per-palace feed still shows it.
            None => {
                state
                    .log
                    .push(format!("recall \"{query}\" (all) → {} results", hits.len()));
                for hit in &hits {
                    let palace = if hit.palace_id.is_empty() {
                        "?"
                    } else {
                        hit.palace_id.as_str()
                    };
                    state
                        .log
                        .push_raw_scoped(palace, format!("  · [{palace}] {}", hit.snippet));
                }
            }
            // A single palace: keep only that palace's hits.
            Some(id) => {
                let kept: Vec<&RecallHit> = hits.iter().filter(|h| h.palace_id == *id).collect();
                state
                    .log
                    .push_scoped(id, format!("recall \"{query}\" → {} results", kept.len()));
                for hit in kept {
                    state
                        .log
                        .push_raw_scoped(id, format!("  · {}", hit.snippet));
                }
            }
        },
        Err(e) => match &scope {
            None => state
                .log
                .push(format!("recall \"{query}\" (all) failed: {e}")),
            Some(id) => state
                .log
                .push_scoped(id, format!("recall \"{query}\" failed: {e}")),
        },
    }
    state.input.clear();
}

/// Append a streamed `/sse` event to the activity log, scoped to its palace.
///
/// Why: the SSE task forwards [`MemoryEvent`]s through a channel; the event
/// loop drains them and this turns each into a human-readable log entry. The
/// drawer events concern one palace, so they are tagged with its id and the
/// per-palace activity feed keeps only its own events.
/// What: `DreamCompleted` records a daemon-wide header plus an indented
/// merge/prune/compact line; `DrawerAdded` / `DrawerDeleted` record a single
/// line each scoped to `palace_id`; `PalaceCreated` records a daemon-wide line
/// (the new palace has no id yet on the wire).
/// Test: `test_log_append_dream`, `test_apply_memory_event`.
pub fn apply_memory_event(state: &mut MemoryTuiState, event: MemoryEvent) {
    match event {
        MemoryEvent::DreamCompleted {
            merged,
            pruned,
            compacted,
        } => {
            state.log.push("SSE: dream_completed");
            state.log.push_raw(format!(
                "  merged: {merged}  pruned: {pruned}  compacted: {compacted}"
            ));
        }
        MemoryEvent::DrawerAdded {
            palace_id,
            drawer_count,
        } => {
            state.log.push_scoped(
                &palace_id,
                format!("SSE: drawer added → {palace_id} ({drawer_count})"),
            );
        }
        MemoryEvent::DrawerDeleted {
            palace_id,
            drawer_count,
        } => {
            state.log.push_scoped(
                &palace_id,
                format!("SSE: drawer deleted → {palace_id} ({drawer_count})"),
            );
        }
        MemoryEvent::PalaceCreated { name } => {
            state.log.push(format!("SSE: palace created → {name}"));
        }
    }
}

/// The memory TUI event loop: poll, render, handle input, drain SSE events.
///
/// Why: kept separate from [`run_with_url`] so terminal setup/teardown wraps it
/// cleanly.
/// What: polls the daemon immediately and spawns the `/sse` subscription task,
/// then renders every frame while polling the keyboard every 50 ms; re-polls on
/// the 2 s timer and drains SSE events via `try_recv`. `[d]` triggers a dream
/// cycle, `[Enter]` runs a recall; `Tab`, arrows, `?`, `q`/`Esc`, and `Ctrl-C`
/// behave per [`KEY_HINT`].
/// Test: the pure pieces (state, log, rendering helpers) are unit-tested.
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut MemoryTuiState,
    client: &mut MemoryClient,
) -> anyhow::Result<()> {
    poll_daemon(state, client).await;
    let mut last_poll = Instant::now();

    // Subscribe to the daemon's /sse stream on a background task.
    let (sse_tx, mut sse_rx) = mpsc::channel::<MemoryEvent>(64);
    let sse_client = client.clone();
    tokio::spawn(async move {
        sse_client.sse_stream(sse_tx).await;
    });

    loop {
        terminal.draw(|f| render(f, state))?;
        // `terminal.draw` requires `state` mutably (the renderer scrolls the
        // palace list); the closure reborrows it for the rest of the loop.

        // Drain any SSE events the subscription task produced since last frame.
        while let Ok(event) = sse_rx.try_recv() {
            apply_memory_event(state, event);
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
                // Filter-active bindings come first — they capture characters,
                // backspace, Esc, and Enter before the general List handlers.
                (MemoryFocus::List, KeyCode::Esc) if state.filter_active => {
                    // Keep the filter text so the user can re-activate.
                    state.filter_active = false;
                }
                (MemoryFocus::List, KeyCode::Enter) if state.filter_active => {
                    state.filter_active = false;
                }
                (MemoryFocus::List, KeyCode::Backspace) if state.filter_active => {
                    state.filter.pop();
                }
                (MemoryFocus::List, KeyCode::Char(c)) if state.filter_active => {
                    state.filter.push(c);
                }
                (_, KeyCode::Char('?')) => state.show_help = true,
                (_, KeyCode::Tab) => state.toggle_focus(),
                (_, KeyCode::Esc) => return Ok(()),
                // List-focus bindings.
                (MemoryFocus::List, KeyCode::Char('q')) => return Ok(()),
                (MemoryFocus::List, KeyCode::Up) => state.select_up(),
                (MemoryFocus::List, KeyCode::Down) => state.select_down(),
                (MemoryFocus::List, KeyCode::Char('/')) => {
                    state.filter_active = true;
                    state.filter.clear();
                }
                (MemoryFocus::List, KeyCode::Char('s')) => {
                    state.sort_key = state.sort_key.next();
                }
                (MemoryFocus::List, KeyCode::Char('g')) => {
                    state.group_by_project = !state.group_by_project;
                }
                (MemoryFocus::List, KeyCode::Char('d')) => {
                    state.log.push("dream cycle triggered");
                    match client.dream_run().await {
                        Ok(stats) => state.log.push_raw(format!(
                            "  merged: {}  pruned: {}  compacted: {}",
                            stats.merged, stats.pruned, stats.compacted
                        )),
                        Err(e) => state.log.push(format!("dream failed: {e}")),
                    }
                    poll_daemon(state, client).await;
                    last_poll = Instant::now();
                }
                // Input-focus bindings.
                (MemoryFocus::Input, KeyCode::Enter) => {
                    run_recall(state, client).await;
                }
                (MemoryFocus::Input, KeyCode::Backspace) => {
                    state.input.pop();
                }
                (MemoryFocus::Input, KeyCode::Char(c)) => state.input.push(c),
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
        "  Tab     switch focus between the palace list and the recall bar",
        "  ↑ / ↓   move the palace selection (when the list has focus)",
        "  All     the top list row fans recalls / stats across every palace",
        "  /       activate the inline palace filter (Esc / Enter close)",
        "  s       cycle palace sort: Activity → Name → Vectors",
        "  g       toggle grouping by inferred project",
        "  d       run a dream cycle across every palace",
        "  Enter   run a recall query — all palaces, or the selected one",
        "  ?       toggle this help overlay",
        "  q / Esc quit",
    ]
    .join("\n")
}

/// Compute the width of the left PALACES panel for a given terminal width.
///
/// Why: the layout caps the palace panel so the activity log gets the bulk of
/// the width on wide terminals.
/// What: returns `min(LEFT_PANEL_MAX, width / 3)`.
/// Test: `test_left_panel_width`.
pub fn left_panel_width(width: u16) -> u16 {
    LEFT_PANEL_MAX.min(width / 3)
}

/// Format one palace as a fixed-width table row.
///
/// Why: the PALACES panel lists every palace with its vector count in aligned
/// columns; isolating the formatter makes the alignment unit-testable.
/// What: returns `> <name padded to 10>  <count>v`, where the leading marker
/// is `>` for the selected row and a space otherwise.
/// Test: `test_palace_row_display`.
pub fn palace_row(palace: &PalaceRow, selected: bool) -> String {
    let marker = if selected { ">" } else { " " };
    let label = if palace.name.is_empty() {
        &palace.id
    } else {
        &palace.name
    };
    format!(
        "{marker} {:<10} {:>7}v",
        truncate(label, 10),
        format_count(palace.vector_count),
    )
}

/// One rendered row of the PALACES panel.
///
/// Why: the renderer styles four row kinds differently — the "All" row is
/// bold, group headers are bold yellow and non-selectable, the selected row is
/// highlighted, ordinary rows are plain — so the line builder must surface
/// which kind each row is rather than just a bool.
/// What: the row `text`, whether it is `selected`, whether it is the synthetic
/// `is_all` ("All palaces") row, and whether it is a group header (non-
/// selectable when grouping by project).
/// Test: `test_palace_lines`, `test_all_selector`, `test_palace_lines_grouped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PalaceListRow {
    /// The fully-formatted row text.
    pub text: String,
    /// Whether this row is the current selection.
    pub selected: bool,
    /// Whether this row is the synthetic "All palaces" entry.
    pub is_all: bool,
    /// Whether this row is a non-selectable group header.
    pub is_header: bool,
}

/// Format an indented palace row for use under a group header.
///
/// Why: when the list is grouped, palace rows are inset one extra space and
/// the name column shrinks by one to keep the count column aligned with the
/// flat layout.
/// What: returns `"  <name padded to 9>  <count>v"`, with `>` replacing the
/// leading space when `selected`.
/// Test: `test_palace_lines_grouped`.
fn palace_row_indented(palace: &PalaceRow, selected: bool) -> String {
    let marker = if selected { ">" } else { " " };
    let label = if palace.name.is_empty() {
        &palace.id
    } else {
        &palace.name
    };
    format!(
        "{marker}  {:<9} {:>7}v",
        truncate(label, 9),
        format_count(palace.vector_count),
    )
}

/// Apply [`MemoryTuiState::filter`] and [`MemoryTuiState::sort_key`] to a slice
/// of palaces, returning the visible subset in display order.
///
/// Why: filtering and sorting are pure functions of the state — extracting
/// them keeps [`palace_lines`] terse and makes both behaviours unit-testable
/// without a terminal.
/// What: case-insensitive substring match against `name` and `project()`; then
/// sort per `sort_key`. The original palace ordering is preserved when the
/// sort comparator returns `Equal`.
/// Test: `test_apply_filter`, `test_apply_sort_activity`,
/// `test_apply_sort_name`, `test_apply_sort_vectors`.
pub fn filtered_sorted_palaces(state: &MemoryTuiState) -> Vec<PalaceRow> {
    let filter_lower = state.filter.to_lowercase();
    let mut rows: Vec<PalaceRow> = state
        .palaces
        .iter()
        .filter(|p| {
            if filter_lower.is_empty() {
                return true;
            }
            p.name.to_lowercase().contains(&filter_lower)
                || p.project().to_lowercase().contains(&filter_lower)
        })
        .cloned()
        .collect();
    match state.sort_key {
        PalaceSortKey::Activity => {
            // last_write_at desc, None last; vector_count desc as tiebreak.
            rows.sort_by(|a, b| match (a.last_write_at, b.last_write_at) {
                (Some(x), Some(y)) => y.cmp(&x).then_with(|| b.vector_count.cmp(&a.vector_count)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => b.vector_count.cmp(&a.vector_count),
            });
        }
        PalaceSortKey::Name => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        PalaceSortKey::Vectors => rows.sort_by(|a, b| b.vector_count.cmp(&a.vector_count)),
    }
    rows
}

/// Build the rows for the PALACES panel body.
///
/// Why: separating row construction from the ratatui widgets lets a test
/// assert the rendered content without a terminal backend.
/// What: returns the synthetic "All palaces" row first (carrying the summed
/// vector count across every palace), then either a flat list of filtered +
/// sorted palace rows, or — when [`MemoryTuiState::group_by_project`] is set —
/// non-selectable `── <project> ──` group headers interleaved with their
/// member palaces. With no palaces the "All" row is still shown followed by a
/// placeholder line.
/// Test: `test_palace_lines`, `test_all_selector`, `test_palace_lines_grouped`.
pub fn palace_lines(state: &MemoryTuiState) -> Vec<PalaceListRow> {
    let mut rows: Vec<PalaceListRow> = Vec::with_capacity(state.palaces.len() + 1);

    // The synthetic "All palaces" row always leads the list — including when
    // filtering or grouping is active. The label may be wider than the per-
    // palace name column — it is shown in full.
    let total_vectors: u64 = state.palaces.iter().map(|p| p.vector_count).sum();
    let all_selected = state.selected == 0;
    let all_marker = if all_selected { ">" } else { " " };
    rows.push(PalaceListRow {
        text: format!("{all_marker} {ALL_LABEL}  {}v", format_count(total_vectors),),
        selected: all_selected,
        is_all: true,
        is_header: false,
    });

    if state.palaces.is_empty() {
        rows.push(PalaceListRow {
            text: "  (no palaces)".to_string(),
            selected: false,
            is_all: false,
            is_header: false,
        });
        return rows;
    }

    let visible = filtered_sorted_palaces(state);
    if visible.is_empty() {
        rows.push(PalaceListRow {
            text: "  (no matches)".to_string(),
            selected: false,
            is_all: false,
            is_header: false,
        });
        return rows;
    }

    // We need to compute the cursor row each visible palace lives at. The cursor
    // addresses the *original* `state.palaces` indices (cursor n → palaces[n-1])
    // so we look up each visible palace's original index by id.
    let cursor_for = |p: &PalaceRow| -> usize {
        state
            .palaces
            .iter()
            .position(|orig| orig.id == p.id)
            .map(|i| i + 1)
            .unwrap_or(0)
    };

    if state.group_by_project {
        // Collect distinct projects in the order they first appear in `visible`.
        let mut seen: Vec<String> = Vec::new();
        for p in &visible {
            let proj = p.project().to_string();
            if !seen.iter().any(|s| s == &proj) {
                seen.push(proj);
            }
        }
        for project in &seen {
            rows.push(PalaceListRow {
                text: format!("── {project} ─────"),
                selected: false,
                is_all: false,
                is_header: true,
            });
            for palace in visible.iter().filter(|p| p.project() == project) {
                let cursor = cursor_for(palace);
                let selected = cursor == state.selected;
                rows.push(PalaceListRow {
                    text: palace_row_indented(palace, selected),
                    selected,
                    is_all: false,
                    is_header: false,
                });
            }
        }
    } else {
        for palace in &visible {
            let cursor = cursor_for(palace);
            let selected = cursor == state.selected;
            rows.push(PalaceListRow {
                text: palace_row(palace, selected),
                selected,
                is_all: false,
                is_header: false,
            });
        }
    }
    rows
}

/// Build the STATISTICS panel lines for the current selection.
///
/// Why: the bottom-right panel shows counts and sizes for whichever palace is
/// selected, or aggregate totals plus a per-palace breakdown when "All" is
/// selected; isolating the builder makes the content testable without a
/// terminal.
/// What: for a single palace, returns its name, vector count, and id. For the
/// "All" selection, returns the palace count and the daemon's aggregate
/// vector / drawer / KG-triple totals, plus one `· <name>: <vectors>`
/// breakdown line per palace.
/// Test: `test_stats_lines`.
pub fn stats_lines(state: &MemoryTuiState) -> Vec<String> {
    if state.is_all_selected() {
        let stats = state.status.clone().unwrap_or_default();
        let mut lines = vec![
            format!("Scope:        {ALL_LABEL}"),
            format!("Palaces:      {}", state.palaces.len()),
            format!("Vectors:      {}", format_count(stats.total_vectors)),
            format!("Drawers:      {}", format_count(stats.total_drawers)),
            format!("KG triples:   {}", format_count(stats.total_kg_triples)),
        ];
        if state.palaces.is_empty() {
            lines.push("(no palaces)".to_string());
        } else {
            lines.push(String::new());
            for palace in &state.palaces {
                let label = if palace.name.is_empty() {
                    &palace.id
                } else {
                    &palace.name
                };
                lines.push(format!(
                    "  · {:<12} {:>7}v",
                    truncate(label, 12),
                    format_count(palace.vector_count),
                ));
            }
        }
        return lines;
    }

    match state.palaces.get(state.selected.saturating_sub(1)) {
        Some(palace) => {
            let label = if palace.name.is_empty() {
                "(unnamed)"
            } else {
                palace.name.as_str()
            };
            vec![
                format!("Palace:       {label}"),
                format!("Vectors:      {}", format_count(palace.vector_count)),
                format!("Id:           {}", palace.id),
            ]
        }
        None => vec!["(no palace selected)".to_string()],
    }
}

/// Truncate a string to `max` characters, appending an ellipsis when cut.
///
/// Why: palace names can be long; the fixed-width left panel needs bounded
/// labels.
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

/// Build the title-bar line for the memory UI.
///
/// Why: the top row shows the daemon name, version, and liveness badge at a
/// glance; isolating it keeps `render` terse and the text testable.
/// What: returns `trusty-memory vX  [●] <status>` — the daemon's reported
/// version is appended when it is online.
/// Test: `test_title_line`.
pub fn title_line(state: &MemoryTuiState) -> String {
    let (glyph, label) = state.daemon_status.badge();
    match &state.daemon_status {
        DaemonStatus::Online { version, .. } => {
            format!("trusty-memory v{version}  [{glyph}] {label}")
        }
        _ => format!(
            "trusty-memory v{VERSION}  [{glyph}] {label}  {}",
            state.base_url
        ),
    }
}

/// Vertical split of the right-hand pane: ACTIVITY over STATISTICS.
///
/// Why: the right side shows two things — a live event feed and the selected
/// palace's stats; isolating the split as a named constant keeps `render`
/// terse and documents the 60 / 40 ratio.
/// What: the ACTIVITY panel takes the top 60 %, STATISTICS the bottom 40 %.
const ACTIVITY_PERCENT: u16 = 60;

/// Draw the memory TUI frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a 4-row vertical layout — title bar, the PALACES / right-pane split,
/// the RECALL input bar, and the key-hint footer. The right pane is itself
/// split vertically into an ACTIVITY feed (top 60 %) and a STATISTICS panel
/// (bottom 40 %), both scoped to the selected palace — or aggregated when "All"
/// is selected. A centred help overlay floats on top when `show_help` is set.
/// Test: line content is unit-tested via the `*_lines` helpers; this glue is
/// exercised by `test_render_smoke`.
pub fn render(frame: &mut Frame, state: &mut MemoryTuiState) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title bar
            Constraint::Min(4),    // panels
            Constraint::Length(3), // recall input
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

    // PALACES on the left, the ACTIVITY / STATISTICS stack on the right.
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_panel_width(area.width)),
            Constraint::Min(10),
        ])
        .split(rows[1]);

    let list_focused = state.focus == MemoryFocus::List;
    let palace_items: Vec<ListItem> = palace_lines(state)
        .into_iter()
        .map(|row| {
            let style = if row.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if row.is_header {
                // Group headers — bold yellow, non-selectable.
                Style::default()
                    .fg(Color::Yellow)
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

    // When the inline filter is active or carries text, split the left column
    // vertically so the filter input renders above the palace list.
    let show_filter_bar = state.filter_active || !state.filter.is_empty();
    let (filter_area, list_area) = if show_filter_bar {
        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(3)])
            .split(split[0]);
        (Some(inner[0]), inner[1])
    } else {
        (None, split[0])
    };

    if let Some(area) = filter_area {
        let border_color = if state.filter_active {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("🔍 ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    state.filter.as_str().to_string(),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    if state.filter_active { "_" } else { "" },
                    Style::default().fg(Color::Cyan),
                ),
            ]))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(
                        Style::default()
                            .fg(border_color)
                            .add_modifier(Modifier::BOLD),
                    )
                    .title(Span::styled(
                        " FILTER ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )),
            ),
            area,
        );
    }

    // Scroll the PALACES list so the selected row stays visible: the panel
    // height minus its two border rows is the visible window.
    let palace_visible = list_area.height.saturating_sub(2) as usize;
    state.sync_scroll(palace_visible);
    let mut palace_state = ListState::default()
        .with_offset(state.scroll_offset)
        .with_selected(Some(state.selected));
    let palace_title = format!("PALACES [{}]", state.sort_key.label());
    frame.render_stateful_widget(
        List::new(palace_items).block(panel_block(&palace_title, list_focused)),
        list_area,
        &mut palace_state,
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

    // RECALL input bar.
    let input_focused = state.focus == MemoryFocus::Input;
    let cursor = if input_focused { "_" } else { "" };
    let input_style = if input_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("RECALL ▶ ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{}{cursor}", state.input), input_style),
        ]))
        .block(panel_block("RECALL", input_focused)),
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

    /// A state with two palaces and aggregate stats for rendering tests.
    fn sample_state() -> MemoryTuiState {
        let mut state = MemoryTuiState::new("http://127.0.0.1:7070");
        state.daemon_status = DaemonStatus::Online {
            version: "0.1.54".into(),
            uptime_secs: 0,
        };
        state.palaces = vec![
            PalaceRow {
                id: "default".into(),
                name: "default".into(),
                vector_count: 8_400,
                ..Default::default()
            },
            PalaceRow {
                id: "work".into(),
                name: "work".into(),
                vector_count: 0,
                ..Default::default()
            },
        ];
        state.status = Some(MemoryData {
            version: "0.1.54".into(),
            palace_count: 2,
            total_drawers: 14,
            total_vectors: 8_400,
            total_kg_triples: 1_200,
            palaces: state.palaces.clone(),
        });
        state
    }

    #[test]
    fn test_new_state_defaults() {
        let state = MemoryTuiState::new("http://127.0.0.1:7070");
        assert_eq!(state.base_url, "http://127.0.0.1:7070");
        assert!(matches!(state.daemon_status, DaemonStatus::Connecting));
        assert!(state.status.is_none());
        assert!(state.palaces.is_empty());
        assert_eq!(state.selected, 0);
        assert!(state.log.is_empty());
        assert_eq!(state.focus, MemoryFocus::List);
        assert!(!state.show_help);
    }

    #[test]
    fn test_toggle_focus() {
        let mut state = MemoryTuiState::new("http://x");
        assert_eq!(state.focus, MemoryFocus::List);
        state.toggle_focus();
        assert_eq!(state.focus, MemoryFocus::Input);
        state.toggle_focus();
        assert_eq!(state.focus, MemoryFocus::List);
    }

    #[test]
    fn test_selected_clamp() {
        let mut state = sample_state();
        // The list has 1 ("All") + 2 palaces = 3 rows; the cursor stops at 2.
        for _ in 0..10 {
            state.select_down();
        }
        assert_eq!(state.selected, 2, "clamped to palaces.len()");
        for _ in 0..10 {
            state.select_up();
        }
        assert_eq!(state.selected, 0);
        // A shrunk palace list re-clamps the cursor (1 "All" + 1 palace = 1).
        state.selected = 2;
        state.palaces.truncate(1);
        state.clamp_selection();
        assert_eq!(state.selected, 1);
        // An empty list leaves only the "All" row at cursor 0.
        state.palaces.clear();
        state.selected = 9;
        state.clamp_selection();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_selected_id() {
        let mut state = sample_state();
        // Cursor 0 is "All" — no single palace.
        assert!(state.is_all_selected());
        assert_eq!(state.selected_id(), None);
        // Cursor 1 is the first palace.
        state.select_down();
        assert_eq!(state.selected_id(), Some("default"));
        state.select_down();
        assert_eq!(state.selected_id(), Some("work"));
        state.palaces.clear();
        state.clamp_selection();
        assert_eq!(state.selected_id(), None);
    }

    #[test]
    fn test_all_selector() {
        let mut state = sample_state();
        // The default selection is the "All palaces" row.
        assert!(state.is_all_selected());
        assert_eq!(state.scope_filter(), None);
        // Moving down off row 0 picks a single palace and a scoped filter.
        state.select_down();
        assert!(!state.is_all_selected());
        assert_eq!(state.scope_filter(), Some("default"));
        state.select_up();
        assert!(state.is_all_selected());

        // The palace list always leads with the "All" row.
        let rows = palace_lines(&state);
        assert_eq!(rows.len(), 3, "1 'All' row + 2 palaces");
        assert!(rows[0].is_all);
        assert!(rows[0].text.contains(ALL_LABEL));
        assert!(rows[0].selected, "'All' is selected by default");
        assert!(!rows[1].is_all);
        assert!(rows[1].text.contains("default"));
    }

    #[test]
    fn test_stats_lines() {
        let mut state = sample_state();
        // "All" selected → aggregate totals + per-palace breakdown.
        let all = stats_lines(&state);
        assert!(
            all.iter()
                .any(|l| l.contains("Palaces:") && l.contains('2'))
        );
        assert!(
            all.iter()
                .any(|l| l.contains("Vectors:") && l.contains("8,400"))
        );
        assert!(
            all.iter()
                .any(|l| l.contains("KG triples:") && l.contains("1,200"))
        );
        assert!(all.iter().any(|l| l.contains("default")));

        // A single palace selected → that palace's detail.
        state.select_down(); // cursor 1 → default
        let one = stats_lines(&state);
        assert!(
            one.iter()
                .any(|l| l.contains("Palace:") && l.contains("default"))
        );
        assert!(
            one.iter()
                .any(|l| l.contains("Vectors:") && l.contains("8,400"))
        );
        assert!(one.iter().any(|l| l.contains("Id:")));
    }

    #[test]
    fn test_palace_row_display() {
        // The selected row is marked `>`; columns are aligned and the count
        // carries a trailing `v`.
        let palace = PalaceRow {
            id: "default".into(),
            name: "default".into(),
            vector_count: 8_400,
            ..Default::default()
        };
        let selected = palace_row(&palace, true);
        assert!(selected.starts_with('>'), "selected marker: {selected}");
        assert!(selected.contains("default"));
        assert!(selected.contains("8,400v"));

        let unselected = palace_row(&palace, false);
        assert!(unselected.starts_with(' '), "unselected: {unselected}");

        // A nameless palace falls back to its id; a zero count still renders.
        let nameless = PalaceRow {
            id: "p-xyz".into(),
            name: String::new(),
            vector_count: 0,
            ..Default::default()
        };
        let row = palace_row(&nameless, false);
        assert!(row.contains("p-xyz"));
        assert!(row.contains("0v"));

        // A long name is truncated with an ellipsis.
        let long = PalaceRow {
            id: "x".into(),
            name: "a-very-long-palace-name".into(),
            vector_count: 1,
            ..Default::default()
        };
        assert!(palace_row(&long, false).contains('…'));
    }

    #[test]
    fn test_palace_lines() {
        let state = sample_state();
        let rows = palace_lines(&state);
        // 1 "All" row + 2 palace rows.
        assert_eq!(rows.len(), 3);
        // Row 0 is "All", selected by default.
        assert!(rows[0].is_all);
        assert!(rows[0].selected);
        assert!(rows[0].text.contains(ALL_LABEL));
        assert!(rows[0].text.starts_with('>'));
        // Rows 1..3 are the palaces, unselected.
        assert!(!rows[1].is_all && !rows[1].selected);
        assert!(rows[1].text.contains("default"));
        assert!(rows[2].text.contains("work"));

        // An empty palace list still shows the "All" row plus a placeholder.
        let empty = MemoryTuiState::new("http://x");
        let rows = palace_lines(&empty);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_all);
        assert!(rows[1].text.contains("no palaces"));
    }

    #[test]
    fn test_log_append_dream() {
        // A dream_completed SSE event appends a header line plus an indented
        // merge/prune/compact stats line.
        let mut state = MemoryTuiState::new("http://x");
        apply_memory_event(
            &mut state,
            MemoryEvent::DreamCompleted {
                merged: 3,
                pruned: 1,
                compacted: 0,
            },
        );
        let lines: Vec<&String> = state.log.iter().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("SSE: dream_completed"));
        assert!(lines[0].starts_with('['), "header is timestamped");
        assert!(lines[1].contains("merged: 3"));
        assert!(lines[1].contains("pruned: 1"));
        assert!(lines[1].contains("compacted: 0"));
        // The continuation line is not timestamped — it reads as a sub-line.
        assert!(lines[1].starts_with("  "));
    }

    #[test]
    fn test_apply_memory_event() {
        let mut state = MemoryTuiState::new("http://x");
        apply_memory_event(
            &mut state,
            MemoryEvent::DrawerAdded {
                palace_id: "default".into(),
                drawer_count: 14,
            },
        );
        apply_memory_event(
            &mut state,
            MemoryEvent::DrawerDeleted {
                palace_id: "work".into(),
                drawer_count: 2,
            },
        );
        apply_memory_event(
            &mut state,
            MemoryEvent::PalaceCreated {
                name: "notes".into(),
            },
        );
        let lines: Vec<&String> = state.log.iter().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("drawer added → default (14)"));
        assert!(lines[1].contains("drawer deleted → work (2)"));
        assert!(lines[2].contains("palace created → notes"));

        // Drawer events are scoped to their palace; the per-palace feed keeps
        // only its own drawer event plus the daemon-wide palace-created line.
        let default_feed: Vec<&String> = state.log.tail_scoped(Some("default"), 100).collect();
        assert_eq!(default_feed.len(), 2);
        assert!(
            default_feed
                .iter()
                .any(|l| l.contains("drawer added → default"))
        );
        assert!(
            default_feed
                .iter()
                .any(|l| l.contains("palace created → notes"))
        );
        assert!(
            !default_feed
                .iter()
                .any(|l| l.contains("drawer deleted → work"))
        );
    }

    #[test]
    fn test_log_capacity() {
        let mut state = MemoryTuiState::new("http://x");
        for i in 0..(ActivityLog::MAX_ENTRIES + 30) {
            state.log.push(format!("event {i}"));
        }
        assert_eq!(state.log.len(), ActivityLog::MAX_ENTRIES);
    }

    #[test]
    fn test_timestamped_format() {
        let line = timestamped("recall complete");
        assert!(line.starts_with('['));
        assert!(line.ends_with(" recall complete"));
        assert_eq!(line.as_bytes()[9], b']');
    }

    #[test]
    fn test_left_panel_width() {
        assert_eq!(left_panel_width(200), LEFT_PANEL_MAX);
        assert_eq!(left_panel_width(60), 20);
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("work", 10), "work");
        assert_eq!(truncate("a-very-long-palace", 8), "a-very-…");
    }

    #[test]
    fn test_title_line() {
        let state = sample_state();
        let title = title_line(&state);
        assert!(title.contains("trusty-memory v0.1.54"));
        assert!(title.contains("online"));

        let mut offline = MemoryTuiState::new("http://127.0.0.1:7070");
        offline.daemon_status = DaemonStatus::Offline {
            last_error: "refused".into(),
        };
        let title = title_line(&offline);
        assert!(title.contains("offline"));
        assert!(title.contains("http://127.0.0.1:7070"));
    }

    #[test]
    fn test_palace_sort_key_cycle() {
        assert_eq!(PalaceSortKey::default(), PalaceSortKey::Activity);
        assert_eq!(PalaceSortKey::Activity.next(), PalaceSortKey::Name);
        assert_eq!(PalaceSortKey::Name.next(), PalaceSortKey::Vectors);
        assert_eq!(PalaceSortKey::Vectors.next(), PalaceSortKey::Activity);
        assert_eq!(PalaceSortKey::Activity.label(), "Activity");
        assert_eq!(PalaceSortKey::Name.label(), "Name");
        assert_eq!(PalaceSortKey::Vectors.label(), "Vectors");
    }

    /// State with four palaces spanning two projects, varied vector counts,
    /// and varied last_write_at timestamps. Used by the sort / filter / group
    /// tests.
    fn diverse_state() -> MemoryTuiState {
        use chrono::{TimeZone, Utc};
        let mut state = MemoryTuiState::new("http://127.0.0.1:7070");
        state.palaces = vec![
            PalaceRow {
                id: "trusty-search".into(),
                name: "trusty-search".into(),
                vector_count: 12,
                last_write_at: Some(Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()),
                description: Some(
                    "Auto-registered from /Users/masa/Projects/trusty-tools/trusty-search".into(),
                ),
                ..Default::default()
            },
            PalaceRow {
                id: "trusty-memory".into(),
                name: "trusty-memory".into(),
                vector_count: 3_775,
                last_write_at: Some(Utc.with_ymd_and_hms(2026, 5, 18, 22, 29, 50).unwrap()),
                description: Some(
                    "Auto-registered from /Users/masa/Projects/trusty-tools/trusty-memory".into(),
                ),
                ..Default::default()
            },
            PalaceRow {
                id: "claude-mpm".into(),
                name: "claude-mpm".into(),
                vector_count: 6_163,
                last_write_at: Some(Utc.with_ymd_and_hms(2026, 5, 10, 0, 0, 0).unwrap()),
                description: Some("Auto-registered from /Users/masa/Projects/claude-mpm".into()),
                ..Default::default()
            },
            PalaceRow {
                id: "notes".into(),
                name: "notes".into(),
                vector_count: 100,
                last_write_at: None,
                description: None,
                ..Default::default()
            },
        ];
        state
    }

    #[test]
    fn test_apply_sort_activity() {
        // Activity: last_write_at desc, None last; vector_count desc tiebreak.
        let mut state = diverse_state();
        state.sort_key = PalaceSortKey::Activity;
        let rows = filtered_sorted_palaces(&state);
        assert_eq!(rows[0].id, "trusty-memory");
        assert_eq!(rows[1].id, "claude-mpm");
        assert_eq!(rows[2].id, "trusty-search");
        // None sorts last.
        assert_eq!(rows[3].id, "notes");
    }

    #[test]
    fn test_apply_sort_name() {
        let mut state = diverse_state();
        state.sort_key = PalaceSortKey::Name;
        let rows = filtered_sorted_palaces(&state);
        let names: Vec<&str> = rows.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["claude-mpm", "notes", "trusty-memory", "trusty-search"]
        );
    }

    #[test]
    fn test_apply_sort_vectors() {
        let mut state = diverse_state();
        state.sort_key = PalaceSortKey::Vectors;
        let rows = filtered_sorted_palaces(&state);
        assert_eq!(rows[0].id, "claude-mpm");
        assert_eq!(rows[1].id, "trusty-memory");
        assert_eq!(rows[2].id, "notes");
        assert_eq!(rows[3].id, "trusty-search");
    }

    #[test]
    fn test_apply_filter() {
        let mut state = diverse_state();
        // Case-insensitive substring match against name OR project.
        state.filter = "TRUSTY".into();
        let rows = filtered_sorted_palaces(&state);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|p| p.name.contains("trusty")));

        // Match by project (description path basename).
        state.filter = "claude-mpm".into();
        let rows = filtered_sorted_palaces(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "claude-mpm");

        // No match → empty.
        state.filter = "nothing-here".into();
        assert!(filtered_sorted_palaces(&state).is_empty());

        // Empty filter → everything.
        state.filter.clear();
        assert_eq!(filtered_sorted_palaces(&state).len(), 4);
    }

    #[test]
    fn test_palace_lines_grouped() {
        let mut state = diverse_state();
        state.group_by_project = true;
        state.sort_key = PalaceSortKey::Name;
        let rows = palace_lines(&state);

        // "All" leads the list.
        assert!(rows[0].is_all);

        // Group headers appear and are non-selectable.
        let headers: Vec<&PalaceListRow> = rows.iter().filter(|r| r.is_header).collect();
        assert!(
            !headers.is_empty(),
            "grouping must emit at least one header"
        );
        for h in &headers {
            assert!(h.text.contains("──"));
            assert!(!h.selected);
        }
        // Project names appear in the header text.
        let header_text: String = headers
            .iter()
            .map(|h| h.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(header_text.contains("trusty-memory") || header_text.contains("trusty-search"));
        assert!(header_text.contains("claude-mpm"));

        // Filter narrows grouping to matching projects only.
        state.filter = "claude".into();
        let rows = palace_lines(&state);
        let headers: Vec<&PalaceListRow> = rows.iter().filter(|r| r.is_header).collect();
        assert_eq!(headers.len(), 1);
        assert!(headers[0].text.contains("claude-mpm"));
    }

    #[test]
    fn test_help_text_lists_bindings() {
        let text = help_text();
        for token in ["Tab", "d ", "Enter", "?", "q ", "/", "s ", "g "] {
            assert!(text.contains(token), "help text missing {token}");
        }
    }

    #[test]
    fn test_scroll_offset() {
        // A list taller than its viewport must scroll so the cursor stays in
        // view; a list that fits leaves the offset pinned at zero.
        let mut state = sample_state();
        // 2 palaces + the "All" row = 3 rows; a 6-row window holds them all.
        for row in 0..=state.last_row() {
            state.selected = row;
            state.sync_scroll(6);
            assert_eq!(state.scroll_offset, 0, "no scroll while the list fits");
        }

        // Grow the list well past a 5-row window and walk the cursor down.
        state.palaces = (0..40)
            .map(|n| PalaceRow {
                id: format!("p-{n}"),
                name: format!("palace-{n}"),
                vector_count: 1,
                ..Default::default()
            })
            .collect();
        let window = 5;
        for row in 0..=state.last_row() {
            state.selected = row;
            state.sync_scroll(window);
            assert!(
                row >= state.scroll_offset && row < state.scroll_offset + window,
                "row {row} must be inside [{}, {})",
                state.scroll_offset,
                state.scroll_offset + window,
            );
        }
        // The cursor at the bottom pins the window against the list end.
        assert_eq!(state.scroll_offset, state.last_row() + 1 - window);

        // Walking back up drags the window up with the cursor.
        for row in (0..=state.last_row()).rev() {
            state.selected = row;
            state.sync_scroll(window);
            assert!(
                row >= state.scroll_offset && row < state.scroll_offset + window,
                "row {row} must stay visible while scrolling up",
            );
        }
        assert_eq!(state.scroll_offset, 0, "back at the top");
    }

    #[test]
    fn test_render_smoke() {
        // A full render in several states must not panic — exercise both the
        // "All" selection (aggregated panels) and a single-palace selection.
        let mut state = sample_state();
        state.log.push("SSE: dream_completed");
        state
            .log
            .push_scoped("default", "recall \"auth flow\" → 3 results");
        state.input = "auth flow".into();
        state.focus = MemoryFocus::Input;
        for (w, h) in [(120u16, 30u16), (80, 24)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| render(f, &mut state))
                .expect("render (All) must not panic");
        }
        // Single-palace selection — the right panels scope to that palace.
        state.selected = 1;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &mut state))
            .expect("render (single palace) must not panic");

        // A list far longer than the panel must render (and scroll) cleanly.
        state.palaces = (0..60)
            .map(|n| PalaceRow {
                id: format!("p-{n}"),
                name: format!("palace-{n}"),
                vector_count: 100,
                ..Default::default()
            })
            .collect();
        state.selected = state.last_row();
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &mut state))
            .expect("overflowing list render must not panic");
        assert!(state.scroll_offset > 0, "long list scrolled to the cursor");

        state.show_help = true;
        state.daemon_status = DaemonStatus::Connecting;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &mut state))
            .expect("help render must not panic");
    }
}
