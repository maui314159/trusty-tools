//! Unified monitor dashboard state and rendering.
//!
//! Why: the monitor TUI watches two independent daemons (trusty-search and
//! trusty-memory) side by side; keeping the pure state and layout logic here —
//! separate from the event loop and HTTP polling — makes the layout decisions
//! and number formatting unit-testable without a terminal.
//! What: [`DashboardState`] holds a [`DaemonPanel`] per daemon plus focus and
//! help flags; [`render`] draws the ratatui frame; the free `*_constraints` /
//! `format_*` helpers are the pure pieces the test suite asserts on.
//! Test: `cargo test -p trusty-monitor-tui` covers layout selection, offline
//! rendering, and uptime/count formatting without a terminal.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

/// Terminal width (in columns) at or above which panels render side by side.
///
/// Why: a narrow terminal cannot fit two readable panels horizontally, so the
/// layout stacks them vertically below this threshold.
/// What: 120 columns, the spec's wide/narrow boundary.
/// Test: `test_layout_wide`, `test_layout_narrow`.
pub const WIDE_LAYOUT_MIN_COLS: u16 = 120;

/// One-line key hint shown in the header.
pub const KEY_HINT: &str = "[Tab] focus  [r] reindex  [q] quit  [?] help";

/// Crate version, surfaced in the dashboard title.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which daemon panel currently holds keyboard focus.
///
/// Why: `[Tab]` cycles focus; the focused panel gets a highlighted border and
/// `[r]` only acts on the search panel when it is focused.
/// What: `Search` (the default) or `Memory`.
/// Test: `test_toggle_focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The trusty-search panel has focus.
    #[default]
    Search,
    /// The trusty-memory panel has focus.
    Memory,
}

/// One trusty-search index row rendered in the search panel's table.
///
/// Why: the search panel lists every registered index with its chunk count so
/// the operator can see corpus sizes at a glance.
/// What: the index id, its chunk count, and the indexed root path.
/// Test: `test_search_panel_renders`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexRow {
    /// The index identifier.
    pub id: String,
    /// Number of indexed chunks.
    pub chunk_count: u64,
    /// Filesystem root the index covers.
    pub root_path: String,
}

/// The polled trusty-search panel payload.
///
/// Why: the search panel renders aggregate health plus a per-index table; this
/// groups everything one poll produces.
/// What: the daemon version, uptime, and the index rows.
/// Test: `test_search_panel_renders`.
#[derive(Debug, Clone, Default)]
pub struct SearchData {
    /// The trusty-search daemon version string.
    pub version: String,
    /// Daemon uptime in whole seconds.
    pub uptime_secs: u64,
    /// One row per registered index.
    pub indexes: Vec<IndexRow>,
}

impl SearchData {
    /// Sum the chunk counts across every index.
    ///
    /// Why: the panel header shows a single "total chunks" figure.
    /// What: folds `chunk_count` over [`Self::indexes`].
    /// Test: `test_search_total_chunks`.
    pub fn total_chunks(&self) -> u64 {
        self.indexes.iter().map(|i| i.chunk_count).sum()
    }
}

/// One trusty-memory palace row rendered in the memory panel's table.
///
/// Why: the memory panel lists every palace with its vector count plus the
/// metadata the TUI needs to filter, sort, and group by project.
/// What: the palace id, friendly name, vector and drawer counts, the last
/// write timestamp (when reported), and the auto-registration description
/// string used to infer the originating project.
/// Test: `test_memory_panel_renders`, `test_palace_row_project`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PalaceRow {
    /// The palace identifier.
    pub id: String,
    /// The palace's human-readable name.
    pub name: String,
    /// Number of stored vectors in the palace.
    pub vector_count: u64,
    /// Number of drawers in the palace (from `PalaceInfo`).
    pub drawer_count: u64,
    /// The last write timestamp, when reported by the daemon.
    pub last_write_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The palace description; used to infer the originating project.
    pub description: Option<String>,
}

impl PalaceRow {
    /// Infer the project this palace belongs to.
    ///
    /// Why: project name is encoded in the auto-registered description path,
    /// so the TUI can group palaces by their originating repo.
    /// What: extracts the basename of the path in
    /// `"Auto-registered from <path>"`, falling back to the palace name when
    /// the description does not match the expected prefix.
    /// Test: `test_palace_row_project`.
    pub fn project(&self) -> &str {
        self.description
            .as_deref()
            .and_then(|d| d.strip_prefix("Auto-registered from "))
            .and_then(|p| p.rsplit('/').next())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
    }
}

/// The polled trusty-memory panel payload.
///
/// Why: the memory panel renders aggregate counts plus a per-palace table; this
/// groups everything one poll produces.
/// What: the daemon version, the aggregate counts, and the palace rows.
/// Test: `test_memory_panel_renders`.
#[derive(Debug, Clone, Default)]
pub struct MemoryData {
    /// The trusty-memory daemon version string.
    pub version: String,
    /// Number of palaces.
    pub palace_count: u64,
    /// Total drawers across all palaces.
    pub total_drawers: u64,
    /// Total stored vectors across all palaces.
    pub total_vectors: u64,
    /// Total knowledge-graph triples across all palaces.
    pub total_kg_triples: u64,
    /// One row per palace.
    pub palaces: Vec<PalaceRow>,
}

/// The connection state of one daemon panel.
///
/// Why: each panel must render distinctly whether it is still connecting, has a
/// fresh payload, or is offline with a captured error; a typed enum keeps that
/// rendering exhaustive.
/// What: `Connecting` before the first poll, `Online(T)` with a payload, or
/// `Offline` with the last error string.
/// Test: `test_offline_panel_renders`, `test_search_panel_renders`.
#[derive(Debug, Clone)]
pub enum PanelStatus<T> {
    /// The first poll has not completed yet.
    Connecting,
    /// The daemon answered; `T` is the latest payload.
    Online(T),
    /// The daemon is unreachable; carries the last error message.
    Offline {
        /// The error captured from the most recent failed poll.
        last_error: String,
    },
}

impl<T> PanelStatus<T> {
    /// Whether this panel is currently online.
    ///
    /// Why: the header badge and the focus-dependent `[r]` action both branch
    /// on reachability.
    /// What: returns `true` only for [`PanelStatus::Online`].
    /// Test: `test_panel_status_is_online`.
    pub fn is_online(&self) -> bool {
        matches!(self, PanelStatus::Online(_))
    }
}

/// One daemon's panel: its connection status and the URL it targets.
///
/// Why: the search and memory panels are structurally identical — a status and
/// a base URL — so a generic struct removes the duplication.
/// What: a [`PanelStatus`] payload plus the daemon base URL the poller probes.
/// Test: `test_offline_panel_renders`.
#[derive(Debug, Clone)]
pub struct DaemonPanel<T> {
    /// The panel's connection status and latest payload.
    pub status: PanelStatus<T>,
    /// The daemon base URL this panel polls.
    pub base_url: String,
}

impl<T> DaemonPanel<T> {
    /// Build a panel that starts in the `Connecting` state.
    ///
    /// Why: before the first poll completes the panel has no payload yet.
    /// What: stores `base_url` and sets the status to [`PanelStatus::Connecting`].
    /// Test: `test_panel_starts_connecting`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            status: PanelStatus::Connecting,
            base_url: base_url.into(),
        }
    }
}

/// Snapshot of everything the dashboard renders this frame.
///
/// Why: the event loop polls both daemons, fills this struct, and hands it to
/// [`render`] — a clean data/render split that keeps the loop terse.
/// What: a [`DaemonPanel`] per daemon plus focus and help-overlay flags.
/// Test: `test_toggle_focus`, `test_layout_wide`, `test_offline_panel_renders`.
#[derive(Debug, Clone)]
pub struct DashboardState {
    /// The trusty-search panel.
    pub search: DaemonPanel<SearchData>,
    /// The trusty-memory panel.
    pub memory: DaemonPanel<MemoryData>,
    /// Which panel currently holds keyboard focus.
    pub focus: Focus,
    /// Whether the help overlay is visible (toggled with `?`).
    pub show_help: bool,
    /// Human-readable result of the last action, shown in the header.
    pub last_action: Option<String>,
}

impl DashboardState {
    /// Build a dashboard targeting the two given daemon URLs.
    ///
    /// Why: the event loop resolves both daemon addresses at startup and seeds
    /// the panels with them; both start in `Connecting` until the first poll.
    /// What: constructs both [`DaemonPanel`]s and defaults focus to the search
    /// panel with the help overlay hidden.
    /// Test: `test_new_state_starts_connecting`.
    pub fn new(search_url: impl Into<String>, memory_url: impl Into<String>) -> Self {
        Self {
            search: DaemonPanel::new(search_url),
            memory: DaemonPanel::new(memory_url),
            focus: Focus::Search,
            show_help: false,
            last_action: None,
        }
    }

    /// Cycle keyboard focus between the search and memory panels (`[Tab]`).
    ///
    /// Why: `[Tab]` moves the highlighted border and decides which panel `[r]`
    /// acts on.
    /// What: flips [`Self::focus`].
    /// Test: `test_toggle_focus`.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Search => Focus::Memory,
            Focus::Memory => Focus::Search,
        };
    }

    /// The id of the first index in the focused search panel, if any.
    ///
    /// Why: `[r]` reindexes a search index; without a richer selection model
    /// the dashboard targets the first index of an online, focused search panel.
    /// What: returns `Some(id)` only when the search panel is focused, online,
    /// and has at least one index.
    /// Test: `test_reindex_target`.
    pub fn reindex_target(&self) -> Option<String> {
        if self.focus != Focus::Search {
            return None;
        }
        match &self.search.status {
            PanelStatus::Online(data) => data.indexes.first().map(|i| i.id.clone()),
            _ => None,
        }
    }
}

/// Format a daemon uptime in seconds as a compact `Xh Ym` string.
///
/// Why: the search panel shows uptime; raw seconds are hard to read.
/// What: returns `"{hours}h {minutes}m"`, e.g. `7440` → `"2h 4m"`. Sub-minute
/// uptimes show `"0h 0m"`.
/// Test: `test_uptime_format`.
pub fn format_uptime(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    format!("{hours}h {minutes}m")
}

/// Format a count with thousands separators and a `k` suffix above 10,000.
///
/// Why: large chunk and vector counts (19,400) are easier to scan abbreviated
/// (`19.4k`); small counts stay exact.
/// What: counts below 10,000 are grouped with commas (`1,200`); counts at or
/// above 10,000 are shown as `{n}k` with one decimal (`19.4k`).
/// Test: `test_format_count`.
pub fn format_count(n: u64) -> String {
    if n >= 10_000 {
        let thousands = n as f64 / 1000.0;
        format!("{thousands:.1}k")
    } else {
        group_thousands(n)
    }
}

/// Insert commas every three digits into a number.
///
/// Why: shared by [`format_count`] for the exact-count branch.
/// What: returns the decimal string of `n` with `,` group separators.
/// Test: covered via `test_format_count`.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let bytes = digits.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Compute the layout constraints for the two daemon panels.
///
/// Why: the wide/narrow decision is the dashboard's single responsive rule;
/// isolating it as a pure function makes it directly unit-testable.
/// What: returns `(Direction, [Constraint; 2])` — `Horizontal` with two equal
/// halves when `width >= WIDE_LAYOUT_MIN_COLS`, otherwise `Vertical` with two
/// equal halves so the panels stack.
/// Test: `test_layout_wide`, `test_layout_narrow`.
pub fn panel_layout(width: u16) -> (Direction, [Constraint; 2]) {
    if width >= WIDE_LAYOUT_MIN_COLS {
        (
            Direction::Horizontal,
            [Constraint::Percentage(50), Constraint::Percentage(50)],
        )
    } else {
        (
            Direction::Vertical,
            [Constraint::Percentage(50), Constraint::Percentage(50)],
        )
    }
}

/// The body text for the help overlay, one binding per line.
///
/// Why: kept separate so a test can assert every binding is documented.
/// What: returns the multi-line help string.
/// Test: `test_help_text_lists_bindings`.
pub fn help_text() -> String {
    [
        "  Tab     switch focus between the search and memory panels",
        "  r       reindex the first index of the focused search panel",
        "  ?       toggle this help overlay",
        "  Esc     close this help overlay",
        "  q       quit",
        "",
        "  Offline panels retry automatically every 5 seconds.",
    ]
    .join("\n")
}

/// The status badge `(glyph, label, colour)` for a panel.
///
/// Why: every panel header shows a coloured liveness badge; centralising the
/// mapping keeps the two panel renderers consistent and testable.
/// What: `● ONLINE` (green), `◌ CONNECTING` (yellow), `○ OFFLINE` (red).
/// Test: `test_status_badge`.
pub fn status_badge<T>(status: &PanelStatus<T>) -> (char, &'static str, Color) {
    match status {
        PanelStatus::Online(_) => ('●', "ONLINE", Color::Green),
        PanelStatus::Connecting => ('◌', "CONNECTING", Color::Yellow),
        PanelStatus::Offline { .. } => ('○', "OFFLINE", Color::Red),
    }
}

/// Build the text lines for the trusty-search panel body.
///
/// Why: separating line construction from the ratatui widgets lets a test
/// assert the rendered content without a terminal backend.
/// What: returns the panel body as plain strings — aggregate stats then one
/// line per index; an offline panel shows its error, a connecting panel a
/// placeholder.
/// Test: `test_search_panel_renders`, `test_offline_panel_renders`.
pub fn search_panel_lines(panel: &DaemonPanel<SearchData>) -> Vec<String> {
    match &panel.status {
        PanelStatus::Connecting => vec![format!("connecting to {}…", panel.base_url)],
        PanelStatus::Offline { last_error } => vec![
            format!("daemon unreachable at {}", panel.base_url),
            format!("last error: {last_error}"),
            "retrying every 5s…".to_string(),
        ],
        PanelStatus::Online(data) => {
            let mut lines = vec![
                format!("Uptime:       {}", format_uptime(data.uptime_secs)),
                format!("Indexes:      {}", data.indexes.len()),
                format!("Total chunks: {}", format_count(data.total_chunks())),
                String::new(),
            ];
            if data.indexes.is_empty() {
                lines.push("(no indexes registered)".to_string());
            } else {
                for idx in &data.indexes {
                    lines.push(format!(
                        "{:<16} {:>10} chunks",
                        truncate(&idx.id, 16),
                        format_count(idx.chunk_count),
                    ));
                }
            }
            lines
        }
    }
}

/// Build the text lines for the trusty-memory panel body.
///
/// Why: mirrors [`search_panel_lines`] for testable, terminal-free rendering.
/// What: returns the panel body as plain strings — aggregate counts then one
/// line per palace; offline and connecting states render as for search.
/// Test: `test_memory_panel_renders`, `test_offline_panel_renders`.
pub fn memory_panel_lines(panel: &DaemonPanel<MemoryData>) -> Vec<String> {
    match &panel.status {
        PanelStatus::Connecting => vec![format!("connecting to {}…", panel.base_url)],
        PanelStatus::Offline { last_error } => vec![
            format!("daemon unreachable at {}", panel.base_url),
            format!("last error: {last_error}"),
            "retrying every 5s…".to_string(),
        ],
        PanelStatus::Online(data) => {
            let mut lines = vec![
                format!("Palaces:      {}", data.palace_count),
                format!("Drawers:      {}", format_count(data.total_drawers)),
                format!("Vectors:      {}", format_count(data.total_vectors)),
                format!("KG triples:   {}", format_count(data.total_kg_triples)),
                String::new(),
            ];
            if data.palaces.is_empty() {
                lines.push("(no palaces)".to_string());
            } else {
                for palace in &data.palaces {
                    let label = if palace.name.is_empty() {
                        truncate(&palace.id, 16)
                    } else {
                        truncate(&palace.name, 16)
                    };
                    lines.push(format!(
                        "{:<16} {:>10} vectors",
                        label,
                        format_count(palace.vector_count),
                    ));
                }
            }
            lines
        }
    }
}

/// Truncate a string to `max` characters, appending an ellipsis when cut.
///
/// Why: index ids and palace names can be long; the fixed-width table columns
/// need bounded labels.
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

/// Compute a centred sub-rectangle for the help overlay.
///
/// Why: the help overlay floats in the middle of the terminal regardless of
/// size.
/// What: returns a [`Rect`] of `width`×`height` (clamped to `area`) centred in
/// `area`.
/// Test: side-effect-free geometry, exercised by `render` smoke tests.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Build the bordered block for a daemon panel, highlighting it when focused.
///
/// Why: the focused panel must be visually distinct; both panels share this
/// border-building logic.
/// What: returns a [`Block`] with a styled title carrying the panel name plus
/// its status badge; a focused block gets a thick cyan border.
fn panel_block(name: &str, badge: (char, &str, Color), focused: bool) -> Block<'static> {
    let (glyph, label, color) = badge;
    let title = Line::from(vec![
        Span::styled(
            format!(" {name} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{glyph} {label} "), Style::default().fg(color)),
    ]);
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
        .title(title)
}

/// Render the help overlay listing every key binding.
///
/// Why: the `?` key shows a floating reference of every binding.
/// What: clears a centred rectangle and draws the [`help_text`] in a block.
fn render_help_overlay(frame: &mut Frame) {
    let area = centered_rect(56, 11, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(help_text())
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Help — press ? or Esc to close "),
            ),
        area,
    );
}

/// Draw the unified monitor dashboard frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a vertical layout — a two-line header (title + key hint / last
/// action) and a flexing body split into the two daemon panels. The body split
/// is horizontal on wide terminals and vertical on narrow ones, per
/// [`panel_layout`]. When `show_help` is set a centred overlay floats on top.
/// Test: line content is unit-tested via `search_panel_lines` /
/// `memory_panel_lines`; this glue is exercised by `test_render_smoke`.
pub fn render(frame: &mut Frame, state: &DashboardState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(6)])
        .split(frame.area());

    // Header block.
    let header_lines = vec![
        Line::from(Span::styled(
            format!(" trusty-monitor v{VERSION} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            state
                .last_action
                .clone()
                .unwrap_or_else(|| KEY_HINT.to_string()),
            Style::default().fg(Color::Gray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(header_lines).block(Block::default().borders(Borders::ALL)),
        outer[0],
    );

    // Body: two daemon panels, side by side or stacked.
    let (direction, constraints) = panel_layout(frame.area().width);
    let panels = Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(outer[1]);

    let search_block = panel_block(
        "SEARCH",
        search_version_badge(&state.search),
        state.focus == Focus::Search,
    );
    frame.render_widget(
        Paragraph::new(
            search_panel_lines(&state.search)
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .block(search_block),
        panels[0],
    );

    let memory_block = panel_block(
        "MEMORY",
        memory_version_badge(&state.memory),
        state.focus == Focus::Memory,
    );
    frame.render_widget(
        List::new(
            memory_panel_lines(&state.memory)
                .into_iter()
                .map(ListItem::new)
                .collect::<Vec<_>>(),
        )
        .block(memory_block),
        panels[1],
    );

    if state.show_help {
        render_help_overlay(frame);
    }
}

/// Build the search panel badge, folding the daemon version into the label.
///
/// Why: the panel title shows `SEARCH ● ONLINE vX.Y.Z`; the version comes from
/// the payload only when online.
/// What: returns the [`status_badge`] glyph/colour with a label that appends
/// the version when the panel is online.
fn search_version_badge(panel: &DaemonPanel<SearchData>) -> (char, &'static str, Color) {
    status_badge(&panel.status)
}

/// Build the memory panel badge (see [`search_version_badge`]).
fn memory_version_badge(panel: &DaemonPanel<MemoryData>) -> (char, &'static str, Color) {
    status_badge(&panel.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// A trusty-search payload with two indexes for rendering tests.
    fn sample_search() -> SearchData {
        SearchData {
            version: "0.3.63".into(),
            uptime_secs: 7440,
            indexes: vec![
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
            ],
        }
    }

    /// A trusty-memory payload with two palaces for rendering tests.
    fn sample_memory() -> MemoryData {
        MemoryData {
            version: "0.4.2".into(),
            palace_count: 2,
            total_drawers: 14,
            total_vectors: 8_400,
            total_kg_triples: 1_200,
            palaces: vec![
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
            ],
        }
    }

    #[test]
    fn test_layout_wide() {
        // A terminal at or above the threshold splits the panels horizontally
        // into two equal halves.
        let (direction, constraints) = panel_layout(140);
        assert_eq!(direction, Direction::Horizontal);
        assert_eq!(
            constraints,
            [Constraint::Percentage(50), Constraint::Percentage(50)]
        );
        // Exactly at the boundary still counts as wide.
        assert_eq!(panel_layout(WIDE_LAYOUT_MIN_COLS).0, Direction::Horizontal);
    }

    #[test]
    fn test_layout_narrow() {
        // Below the threshold the panels stack vertically.
        let (direction, constraints) = panel_layout(80);
        assert_eq!(direction, Direction::Vertical);
        assert_eq!(
            constraints,
            [Constraint::Percentage(50), Constraint::Percentage(50)]
        );
    }

    #[test]
    fn test_offline_panel_renders() {
        // An offline panel must produce renderable lines (carrying the error)
        // and must not panic when fed to the frame renderer.
        let search: DaemonPanel<SearchData> = DaemonPanel {
            status: PanelStatus::Offline {
                last_error: "connection refused".into(),
            },
            base_url: "http://127.0.0.1:7878".into(),
        };
        let lines = search_panel_lines(&search);
        assert!(lines.iter().any(|l| l.contains("connection refused")));
        assert!(lines.iter().any(|l| l.contains("unreachable")));

        let memory: DaemonPanel<MemoryData> = DaemonPanel {
            status: PanelStatus::Offline {
                last_error: "timeout".into(),
            },
            base_url: "http://127.0.0.1:7070".into(),
        };
        let mlines = memory_panel_lines(&memory);
        assert!(mlines.iter().any(|l| l.contains("timeout")));

        // A whole-frame render with both panels offline must not panic.
        let state = DashboardState {
            search,
            memory,
            focus: Focus::Search,
            show_help: false,
            last_action: None,
        };
        let backend = TestBackend::new(130, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &state))
            .expect("offline render must not panic");
    }

    #[test]
    fn test_uptime_format() {
        assert_eq!(format_uptime(7440), "2h 4m");
        assert_eq!(format_uptime(0), "0h 0m");
        assert_eq!(format_uptime(59), "0h 0m");
        assert_eq!(format_uptime(3600), "1h 0m");
        assert_eq!(format_uptime(3661), "1h 1m");
    }

    #[test]
    fn test_format_count() {
        // Small counts are comma-grouped and exact.
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(900), "900");
        assert_eq!(format_count(1_200), "1,200");
        assert_eq!(format_count(9_999), "9,999");
        // Counts at or above 10k are abbreviated with one decimal.
        assert_eq!(format_count(19_400), "19.4k");
        assert_eq!(format_count(10_000), "10.0k");
    }

    #[test]
    fn test_search_total_chunks() {
        assert_eq!(sample_search().total_chunks(), 20_194);
        assert_eq!(SearchData::default().total_chunks(), 0);
    }

    #[test]
    fn test_search_panel_renders() {
        let panel = DaemonPanel {
            status: PanelStatus::Online(sample_search()),
            base_url: "http://127.0.0.1:7878".into(),
        };
        let lines = search_panel_lines(&panel);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Uptime:") && l.contains("2h 4m"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Indexes:") && l.contains('2'))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("cto") && l.contains("1,200"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("trusty") && l.contains("19.0k"))
        );
    }

    #[test]
    fn test_memory_panel_renders() {
        let panel = DaemonPanel {
            status: PanelStatus::Online(sample_memory()),
            base_url: "http://127.0.0.1:7070".into(),
        };
        let lines = memory_panel_lines(&panel);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Palaces:") && l.contains('2'))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Vectors:") && l.contains("8,400"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("KG triples:") && l.contains("1,200"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("default") && l.contains("8,400"))
        );
    }

    #[test]
    fn test_toggle_focus() {
        let mut state = DashboardState::new("http://a", "http://b");
        assert_eq!(state.focus, Focus::Search);
        state.toggle_focus();
        assert_eq!(state.focus, Focus::Memory);
        state.toggle_focus();
        assert_eq!(state.focus, Focus::Search);
    }

    #[test]
    fn test_new_state_starts_connecting() {
        let state = DashboardState::new("http://a", "http://b");
        assert!(matches!(state.search.status, PanelStatus::Connecting));
        assert!(matches!(state.memory.status, PanelStatus::Connecting));
        assert_eq!(state.search.base_url, "http://a");
        assert_eq!(state.memory.base_url, "http://b");
    }

    #[test]
    fn test_panel_starts_connecting() {
        let panel: DaemonPanel<SearchData> = DaemonPanel::new("http://x");
        assert!(matches!(panel.status, PanelStatus::Connecting));
        assert_eq!(panel.base_url, "http://x");
    }

    #[test]
    fn test_panel_status_is_online() {
        let online: PanelStatus<u32> = PanelStatus::Online(1);
        assert!(online.is_online());
        let offline: PanelStatus<u32> = PanelStatus::Offline {
            last_error: "x".into(),
        };
        assert!(!offline.is_online());
        let connecting: PanelStatus<u32> = PanelStatus::Connecting;
        assert!(!connecting.is_online());
    }

    #[test]
    fn test_reindex_target() {
        let mut state = DashboardState::new("http://a", "http://b");
        // No target while connecting.
        assert_eq!(state.reindex_target(), None);
        // Online search panel, focused: first index id is the target.
        state.search.status = PanelStatus::Online(sample_search());
        assert_eq!(state.reindex_target(), Some("cto".to_string()));
        // Memory focus disables the reindex target.
        state.focus = Focus::Memory;
        assert_eq!(state.reindex_target(), None);
    }

    #[test]
    fn test_status_badge() {
        let online: PanelStatus<u32> = PanelStatus::Online(0);
        assert_eq!(status_badge(&online), ('●', "ONLINE", Color::Green));
        let connecting: PanelStatus<u32> = PanelStatus::Connecting;
        assert_eq!(
            status_badge(&connecting),
            ('◌', "CONNECTING", Color::Yellow)
        );
        let offline: PanelStatus<u32> = PanelStatus::Offline {
            last_error: "x".into(),
        };
        assert_eq!(status_badge(&offline), ('○', "OFFLINE", Color::Red));
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("short", 16), "short");
        assert_eq!(truncate("0123456789abcdefghij", 8), "0123456…");
        assert_eq!(truncate("exactlyeight", 12), "exactlyeight");
    }

    #[test]
    fn test_palace_row_project() {
        // Auto-registered description → basename of the path.
        let row = PalaceRow {
            id: "trusty-search".into(),
            name: "trusty-search".into(),
            description: Some("Auto-registered from /Users/masa/Projects/trusty-search".into()),
            ..Default::default()
        };
        assert_eq!(row.project(), "trusty-search");

        // No description → falls back to the palace name.
        let bare = PalaceRow {
            id: "p1".into(),
            name: "notes".into(),
            ..Default::default()
        };
        assert_eq!(bare.project(), "notes");

        // Unexpected description shape → fall back to the name.
        let weird = PalaceRow {
            id: "p2".into(),
            name: "weird".into(),
            description: Some("hand-made palace".into()),
            ..Default::default()
        };
        assert_eq!(weird.project(), "weird");

        // Trailing slash should not yield an empty basename.
        let trailing = PalaceRow {
            id: "p3".into(),
            name: "fallback".into(),
            description: Some("Auto-registered from /tmp/".into()),
            ..Default::default()
        };
        assert_eq!(trailing.project(), "fallback");
    }

    #[test]
    fn test_help_text_lists_bindings() {
        let text = help_text();
        for token in ["Tab", "r ", "?", "Esc", "q "] {
            assert!(text.contains(token), "help text missing {token}");
        }
    }

    #[test]
    fn test_render_smoke() {
        // A full render of a fully-online dashboard, wide and narrow, must not
        // panic and must exercise the help overlay path.
        let state = DashboardState {
            search: DaemonPanel {
                status: PanelStatus::Online(sample_search()),
                base_url: "http://127.0.0.1:7878".into(),
            },
            memory: DaemonPanel {
                status: PanelStatus::Online(sample_memory()),
                base_url: "http://127.0.0.1:7070".into(),
            },
            focus: Focus::Memory,
            show_help: true,
            last_action: Some("reindex queued".into()),
        };
        for (w, h) in [(140u16, 30u16), (90, 40)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|f| render(f, &state))
                .expect("render must not panic");
        }
    }
}
