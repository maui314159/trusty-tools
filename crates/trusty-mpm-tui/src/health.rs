//! Combined search + memory health screen (`[2]`) for the trusty-mpm TUI.
//!
//! Why: operators want one glance to confirm that the two daemons the
//! coordinator depends on — trusty-search and trusty-memory — are alive and
//! healthy, without leaving the TUI to run two `status` commands. Keeping the
//! poller, the typed wire shapes, and the pure rendering helpers here (away
//! from the coordinator chat in `dashboard.rs`) keeps both surfaces small and
//! independently testable.
//! What: [`HealthClient`] is a typed `reqwest` transport for one daemon's
//! `/health` + list endpoints; [`PanelData`] is the projected per-daemon
//! payload; [`PanelState`] is `Connecting` / `Online` / `Offline`;
//! [`HealthScreen`] holds both panels plus focus and renders the side-by-side
//! layout. A background tokio task drives polling and pushes [`HealthUpdate`]s
//! down a channel into the TUI event loop.
//! Test: `cargo test -p trusty-mpm-tui` covers the wire projections, panel
//! line building, the focus toggle, and a `TestBackend` render smoke test.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::Deserialize;

/// Default trusty-search daemon address used when no override is supplied.
///
/// Why: the health screen must always have a target to probe; the search
/// daemon binds `127.0.0.1:7878` by convention.
/// What: the canonical local trusty-search HTTP base URL.
/// Test: `default_urls_are_local`.
pub const DEFAULT_SEARCH_URL: &str = "http://127.0.0.1:7878";

/// Default trusty-memory daemon address used when no override is supplied.
///
/// Why: mirrors [`DEFAULT_SEARCH_URL`]; the memory daemon's health endpoint is
/// reached at `127.0.0.1:7990` for the monitor surface.
/// What: the canonical local trusty-memory HTTP base URL.
/// Test: `default_urls_are_local`.
pub const DEFAULT_MEMORY_URL: &str = "http://127.0.0.1:7990";

/// Interval between health polls for each panel.
///
/// Why: the ticket mandates a 5-second refresh cadence for both the online and
/// the offline (retry) paths.
/// What: five seconds.
/// Test: exercised indirectly by the background poller.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Per-request timeout for a daemon health probe.
///
/// Why: a hung daemon must not stall the poll task; a short timeout turns an
/// unresponsive daemon into a clean "offline" state on the next tick.
/// What: three seconds, comfortably above a healthy local round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Which daemon a panel (or a poll result) refers to.
///
/// Why: the background poller probes two daemons and the event loop must route
/// each [`HealthUpdate`] to the correct panel; a typed tag keeps that routing
/// exhaustive.
/// What: `Search` for trusty-search, `Memory` for trusty-memory.
/// Test: `toggle_focus_cycles_panels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Daemon {
    /// The trusty-search daemon.
    Search,
    /// The trusty-memory daemon.
    Memory,
}

/// Wire shape of `GET /health` shared by both daemons (issue #35).
///
/// Why: trusty-search and trusty-memory return a compatible health block —
/// `version`, `rss_mb`, `cpu_pct`, `uptime_secs`, `disk_bytes` — so one
/// deserialization target serves both. Every field is `#[serde(default)]` so a
/// daemon on an older build (missing the issue-#35 fields) still deserializes.
/// What: the resource block both `/health` endpoints emit.
/// Test: `health_wire_deserializes_partial_payload`.
#[derive(Debug, Default, Deserialize)]
struct HealthWire {
    #[serde(default)]
    version: String,
    #[serde(default)]
    rss_mb: u64,
    #[serde(default)]
    cpu_pct: f32,
    #[serde(default)]
    uptime_secs: u64,
    #[serde(default)]
    disk_bytes: u64,
}

/// Projected health payload for one daemon panel.
///
/// Why: the panel renders a fixed set of fields; a small typed struct keeps the
/// renderer free of raw JSON and lets the line builder be unit-tested.
/// What: the version string, resource metrics, and the two key-count fields
/// (`count_a` / `count_b`) whose labels differ per daemon.
/// Test: `search_panel_lines_format_fields`, `memory_panel_lines_format_fields`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanelData {
    /// The daemon version string (e.g. `0.3.67`).
    pub version: String,
    /// Resident set size of the daemon process, in megabytes.
    pub rss_mb: u64,
    /// CPU usage as a percentage (`100.0` == one saturated core).
    pub cpu_pct: f32,
    /// Seconds elapsed since the daemon started.
    pub uptime_secs: u64,
    /// On-disk footprint of the daemon's data directory, in bytes.
    pub disk_bytes: u64,
    /// First key count — indexes (search) or palaces (memory).
    pub count_a: u64,
    /// Second key count — total chunks (search) or total vectors (memory).
    pub count_b: u64,
    /// Third key count — `0` for search; total drawers for memory.
    pub count_c: u64,
    /// Fourth key count — `0` for search; total KG triples for memory.
    pub count_d: u64,
}

/// The connection state of one daemon panel.
///
/// Why: each panel renders distinctly whether it is still connecting, has a
/// fresh payload, or is offline with a captured error; a typed enum keeps that
/// rendering exhaustive.
/// What: `Connecting` before the first poll, `Online` with a payload, or
/// `Offline` with the last error string.
/// Test: `panel_lines_render_each_state`.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelState {
    /// The first poll for this panel has not completed yet.
    Connecting,
    /// The daemon answered; carries the latest projected payload.
    Online(PanelData),
    /// The daemon is unreachable; carries the last error message.
    Offline {
        /// The error captured from the most recent failed poll.
        last_error: String,
    },
}

impl PanelState {
    /// Whether this panel is currently online.
    ///
    /// Why: the `[●]`/`[○]` indicator and the badge colour branch on liveness.
    /// What: returns `true` only for [`PanelState::Online`].
    /// Test: `panel_state_is_online`.
    pub fn is_online(&self) -> bool {
        matches!(self, PanelState::Online(_))
    }
}

/// A health poll result delivered from the background task to the event loop.
///
/// Why: polling runs off-thread so a slow daemon never freezes input handling;
/// the loop drains these messages and folds them into the [`HealthScreen`].
/// What: the [`Daemon`] the result is for, plus the new [`PanelState`].
/// Test: `apply_update_routes_to_panel`.
#[derive(Debug, Clone)]
pub struct HealthUpdate {
    /// Which daemon this update describes.
    pub daemon: Daemon,
    /// The freshly-polled panel state.
    pub state: PanelState,
}

/// Typed HTTP client for one daemon's health + list endpoints.
///
/// Why: the background poller needs a small, testable transport that yields a
/// projected [`PanelData`] or a clean error string; keeping it here mirrors the
/// `trusty-common` monitor clients without depending on that crate's feature.
/// What: holds a base URL, the [`Daemon`] tag (which decides the list
/// endpoints), and a pooled `reqwest::Client` with a request timeout.
/// Test: `health_client_stores_base_url`.
#[derive(Debug, Clone)]
pub struct HealthClient {
    base: String,
    daemon: Daemon,
    http: reqwest::Client,
}

impl HealthClient {
    /// Build a client targeting `base` for the given `daemon`.
    ///
    /// Why: the health screen is pointed at a daemon address from a CLI flag or
    /// the documented default.
    /// What: stores the base URL and a pooled `reqwest::Client` whose request
    /// timeout bounds a hung daemon.
    /// Test: `health_client_stores_base_url`.
    pub fn new(base: impl Into<String>, daemon: Daemon) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            base: base.into(),
            daemon,
            http,
        }
    }

    /// The base URL this client targets.
    ///
    /// Why: the offline panel renders the daemon address it failed to reach.
    /// What: returns the stored base URL.
    /// Test: `health_client_stores_base_url`.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Poll the daemon and project the result into a [`PanelState`].
    ///
    /// Why: the background task wants one infallible call per tick that always
    /// yields a renderable state — `Online` on success, `Offline` on any
    /// transport or decode failure.
    /// What: GETs `/health`, then the daemon-specific list endpoints for the
    /// key counts, folding everything into [`PanelData`]. Any error along the
    /// way becomes `Offline` carrying the error string.
    /// Test: live behaviour is covered by the daemon suites; the offline path
    /// is exercised by `poll_unreachable_daemon_is_offline`.
    pub async fn poll(&self) -> PanelState {
        match self.fetch().await {
            Ok(data) => PanelState::Online(data),
            Err(e) => PanelState::Offline {
                last_error: e.to_string(),
            },
        }
    }

    /// Fetch and project the panel payload, returning a `Result` for `?`.
    ///
    /// Why: keeps [`Self::poll`]'s error-to-`Offline` mapping in one place
    /// while the happy path stays terse with `?`.
    /// What: GETs `/health` and the daemon's list endpoints; for search the
    /// counts are index count + summed chunk counts, for memory they come from
    /// `/api/v1/status`.
    /// Test: covered indirectly by `poll`; the count projections are unit-tested
    /// via `project_search_counts` / `project_memory_counts`.
    async fn fetch(&self) -> anyhow::Result<PanelData> {
        let health_path = match self.daemon {
            Daemon::Search => "/health",
            Daemon::Memory => "/health",
        };
        let health: HealthWire = self
            .http
            .get(format!("{}{health_path}", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let (count_a, count_b, count_c, count_d) = match self.daemon {
            Daemon::Search => self.search_counts().await,
            Daemon::Memory => self.memory_counts().await,
        };

        Ok(PanelData {
            version: health.version,
            rss_mb: health.rss_mb,
            cpu_pct: health.cpu_pct,
            uptime_secs: health.uptime_secs,
            disk_bytes: health.disk_bytes,
            count_a,
            count_b,
            count_c,
            count_d,
        })
    }

    /// Resolve the search key counts: `(indexes, total_chunks, 0, 0)`.
    ///
    /// Why: the search panel shows index count and summed chunk count; a
    /// failure to enumerate indexes degrades to zeroes rather than failing the
    /// whole poll, since the resource block already rendered.
    /// What: GETs `/indexes`, then `/indexes/:id/status` per index, summing
    /// `chunk_count`. Any error yields all zeroes.
    /// Test: the JSON projection is unit-tested via `project_search_counts`.
    async fn search_counts(&self) -> (u64, u64, u64, u64) {
        let Ok(list) = self.get_json(format!("{}/indexes", self.base)).await else {
            return (0, 0, 0, 0);
        };
        let ids = list
            .get("indexes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut total_chunks = 0u64;
        for id in &ids {
            if let Ok(status) = self
                .get_json(format!("{}/indexes/{id}/status", self.base))
                .await
            {
                total_chunks = total_chunks.saturating_add(
                    status
                        .get("chunk_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                );
            }
        }
        (ids.len() as u64, total_chunks, 0, 0)
    }

    /// Resolve the memory key counts from `/api/v1/status`.
    ///
    /// Why: the memory panel shows palaces, vectors, drawers, and KG triples;
    /// the status endpoint returns all four in one call.
    /// What: GETs `/api/v1/status` and projects `palace_count`, `total_vectors`,
    /// `total_drawers`, `total_kg_triples`. Any error yields all zeroes.
    /// Test: the JSON projection is unit-tested via `project_memory_counts`.
    async fn memory_counts(&self) -> (u64, u64, u64, u64) {
        match self.get_json(format!("{}/api/v1/status", self.base)).await {
            Ok(status) => project_memory_counts(&status),
            Err(_) => (0, 0, 0, 0),
        }
    }

    /// GET `url` and decode the response body as JSON.
    ///
    /// Why: the count probes share the same GET-and-decode shape.
    /// What: GETs `url`, maps a non-2xx response to an error, and decodes the
    /// body into a [`serde_json::Value`].
    /// Test: covered indirectly by `search_counts` / `memory_counts`.
    async fn get_json(&self, url: String) -> anyhow::Result<serde_json::Value> {
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Request a graceful shutdown of the daemon via its `admin/stop` endpoint.
    ///
    /// Why: the `[X]` key stops the focused daemon without the operator
    /// resolving a PID; both daemons expose an unauthenticated stop route.
    /// What: POSTs an empty body to the daemon's stop path (`/admin/stop` for
    /// search, `/api/v1/admin/stop` for memory). A non-2xx response is an error.
    /// Test: live behaviour is covered by the daemon suites; the dashboard
    /// records the outcome string in `last_action`.
    pub async fn stop(&self) -> anyhow::Result<()> {
        let path = match self.daemon {
            Daemon::Search => "/admin/stop",
            Daemon::Memory => "/api/v1/admin/stop",
        };
        self.http
            .post(format!("{}{path}", self.base))
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Project a `/api/v1/status` payload into `(palaces, vectors, drawers, kg)`.
///
/// Why: centralising the projection keeps [`HealthClient::memory_counts`]
/// testable without a live daemon and resilient to absent optional fields.
/// What: reads `palace_count`, `total_vectors`, `total_drawers`, and
/// `total_kg_triples`, defaulting any absent field to zero.
/// Test: `project_memory_counts`.
fn project_memory_counts(status: &serde_json::Value) -> (u64, u64, u64, u64) {
    let u = |key: &str| status.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    (
        u("palace_count"),
        u("total_vectors"),
        u("total_drawers"),
        u("total_kg_triples"),
    )
}

/// Format a `uptime` in seconds as a compact `Xh Ym` string.
///
/// Why: the panel shows uptime; raw seconds are hard to read at a glance.
/// What: returns `"{hours}h {minutes}m"` — e.g. `3720` → `"1h 2m"`.
/// Test: `format_uptime_is_compact`.
pub fn format_uptime(secs: u64) -> String {
    format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
}

/// Format a byte count as a human-readable size (`KB` / `MB` / `GB`).
///
/// Why: raw `disk_bytes` and `rss_mb` figures are easier to scan abbreviated.
/// What: returns one decimal place with a unit suffix, picking the largest
/// unit under which the value is `>= 1`.
/// Test: `format_bytes_picks_unit`.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1}GB", b / GB)
    } else if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{bytes}B")
    }
}

/// Format an RSS figure (already in megabytes) as a human-readable size.
///
/// Why: `/health` reports RSS in whole megabytes; the panel shows it as `GB`
/// once it crosses 1024 MB so the figure stays short.
/// What: returns `"{x.x}GB"` above 1024 MB, otherwise `"{n}MB"`.
/// Test: `format_rss_picks_unit`.
pub fn format_rss(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1}GB", mb as f64 / 1024.0)
    } else {
        format!("{mb}MB")
    }
}

/// Format a large count compactly: exact below 10k, `Xk` above.
///
/// Why: chunk and vector counts run into the tens of thousands; an abbreviated
/// form keeps the fixed-width panel readable.
/// What: counts below 10,000 are shown exactly; larger counts as `{n}k` with
/// one decimal.
/// Test: `format_count_abbreviates_large`.
pub fn format_count(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Build the text lines for the trusty-search panel body.
///
/// Why: separating line construction from the ratatui widgets lets a test
/// assert the rendered content without a terminal backend.
/// What: returns the panel body as plain strings — a header line with the
/// online indicator and version, then resource and count lines; an offline
/// panel shows its error, a connecting panel a placeholder.
/// Test: `search_panel_lines_format_fields`, `panel_lines_render_each_state`.
pub fn search_panel_lines(state: &PanelState, base_url: &str) -> Vec<String> {
    panel_lines(state, base_url, "SEARCH", |data| {
        vec![
            format!(
                "Indexes: {}  Chunks: {}",
                data.count_a,
                format_count(data.count_b)
            ),
            format!("Disk: {}", format_bytes(data.disk_bytes)),
        ]
    })
}

/// Build the text lines for the trusty-memory panel body.
///
/// Why: mirrors [`search_panel_lines`] for testable, terminal-free rendering.
/// What: returns the panel body as plain strings — header, resource lines, then
/// palace / vector / drawer / KG counts; offline and connecting states render
/// as for search.
/// Test: `memory_panel_lines_format_fields`, `panel_lines_render_each_state`.
pub fn memory_panel_lines(state: &PanelState, base_url: &str) -> Vec<String> {
    panel_lines(state, base_url, "MEMORY", |data| {
        vec![
            format!(
                "Palaces: {}  Vectors: {}",
                data.count_a,
                format_count(data.count_b)
            ),
            format!(
                "Drawers: {}  KG: {}",
                data.count_c,
                format_count(data.count_d)
            ),
        ]
    })
}

/// Shared panel-body builder for the search and memory panels.
///
/// Why: both panels share the header / resource / footer structure; only the
/// count lines differ, so they are supplied by the `counts` closure.
/// What: returns the header (indicator + version), the RSS / CPU / uptime line,
/// the daemon-specific count lines, a blank spacer, and the `[S]start [X]stop`
/// hint. Offline panels show the unreachable address, last error, and a retry
/// note; connecting panels show a placeholder.
/// Test: `panel_lines_render_each_state`.
fn panel_lines(
    state: &PanelState,
    base_url: &str,
    name: &str,
    counts: impl Fn(&PanelData) -> Vec<String>,
) -> Vec<String> {
    match state {
        PanelState::Connecting => vec![format!("{name} [○] connecting to {base_url}…")],
        PanelState::Offline { last_error } => vec![
            format!("{name} [○] OFFLINE"),
            format!("unreachable at {base_url}"),
            format!("last error: {last_error}"),
            "retrying every 5s…".to_string(),
            String::new(),
            "[S]start [X]stop".to_string(),
        ],
        PanelState::Online(data) => {
            let version = if data.version.is_empty() {
                "?".to_string()
            } else {
                format!("v{}", data.version)
            };
            let mut lines = vec![
                format!("{name} [●] {version}"),
                format!(
                    "RSS: {}  CPU: {:.0}%  Uptime: {}",
                    format_rss(data.rss_mb),
                    data.cpu_pct,
                    format_uptime(data.uptime_secs),
                ),
            ];
            lines.extend(counts(data));
            lines.push(String::new());
            lines.push("[S]start [X]stop".to_string());
            lines
        }
    }
}

/// The combined search + memory health screen (`[2]`).
///
/// Why: the event loop polls both daemons on a background task and folds the
/// results here; a clean data struct keeps the loop terse and the rendering
/// pure. Held alongside the chat `DashboardState` so switching screens never
/// resets either surface.
/// What: a [`PanelState`] and base URL per daemon, plus the focused [`Daemon`]
/// that the `[S]`/`[X]` keys act on.
/// Test: `toggle_focus_cycles_panels`, `apply_update_routes_to_panel`.
#[derive(Debug, Clone)]
pub struct HealthScreen {
    /// The trusty-search panel state.
    pub search: PanelState,
    /// The trusty-search daemon base URL.
    pub search_url: String,
    /// The trusty-memory panel state.
    pub memory: PanelState,
    /// The trusty-memory daemon base URL.
    pub memory_url: String,
    /// Which panel `[S]`/`[X]` act on; `[Tab]` cycles it.
    pub focus: Daemon,
}

impl HealthScreen {
    /// Build a health screen targeting the two given daemon URLs.
    ///
    /// Why: the TUI resolves both daemon addresses once at startup and seeds
    /// the panels; both start `Connecting` until the first poll lands.
    /// What: stores both URLs, sets both panels to [`PanelState::Connecting`],
    /// and defaults focus to the search panel.
    /// Test: `new_screen_starts_connecting`.
    pub fn new(search_url: impl Into<String>, memory_url: impl Into<String>) -> Self {
        Self {
            search: PanelState::Connecting,
            search_url: search_url.into(),
            memory: PanelState::Connecting,
            memory_url: memory_url.into(),
            focus: Daemon::Search,
        }
    }

    /// Cycle keyboard focus between the search and memory panels (`[Tab]`).
    ///
    /// Why: `[Tab]` decides which panel the `[S]`/`[X]` service keys act on.
    /// What: flips [`Self::focus`].
    /// Test: `toggle_focus_cycles_panels`.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Daemon::Search => Daemon::Memory,
            Daemon::Memory => Daemon::Search,
        };
    }

    /// Fold a background poll result into the matching panel.
    ///
    /// Why: the event loop drains [`HealthUpdate`]s and must route each to the
    /// correct panel without touching the other.
    /// What: replaces the [`PanelState`] of the daemon named in `update`.
    /// Test: `apply_update_routes_to_panel`.
    pub fn apply_update(&mut self, update: HealthUpdate) {
        match update.daemon {
            Daemon::Search => self.search = update.state,
            Daemon::Memory => self.memory = update.state,
        }
    }

    /// The base URL of the currently-focused panel.
    ///
    /// Why: the `[X]` stop action targets the focused daemon.
    /// What: returns the search or memory URL per [`Self::focus`].
    /// Test: `focused_url_follows_focus`.
    pub fn focused_url(&self) -> &str {
        match self.focus {
            Daemon::Search => &self.search_url,
            Daemon::Memory => &self.memory_url,
        }
    }
}

/// Build a [`HealthClient`] for the given daemon at the given base URL.
///
/// Why: the background poller and the `[S]`/`[X]` key handlers all need a
/// client; centralising construction keeps the daemon→client mapping in one
/// place.
/// What: returns a [`HealthClient`] tagged with `daemon`.
/// Test: covered by `health_client_stores_base_url`.
pub fn client_for(daemon: Daemon, base_url: &str) -> HealthClient {
    HealthClient::new(base_url, daemon)
}

/// Render the health screen into `area`-spanning `frame`.
///
/// Why: the single entry point the event loop calls when screen `[2]` is
/// active; keeps all the ratatui widget assembly in one place.
/// What: a vertical layout — a one-line title, a body split horizontally into
/// the two daemon panels (the focused one gets a bold cyan border), and the
/// shared status bar is drawn by the caller. Panel bodies come from the pure
/// `*_panel_lines` helpers.
/// Test: line content is unit-tested via `search_panel_lines` /
/// `memory_panel_lines`; this glue is exercised by `render_health_smoke`.
pub fn render(frame: &mut Frame, screen: &HealthScreen) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(6)])
        .split(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " trusty-mpm health ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
        outer[0],
    );

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[1]);

    render_panel(
        frame,
        panels[0],
        "SEARCH",
        &search_panel_lines(&screen.search, &screen.search_url),
        screen.search.is_online(),
        screen.focus == Daemon::Search,
    );
    render_panel(
        frame,
        panels[1],
        "MEMORY",
        &memory_panel_lines(&screen.memory, &screen.memory_url),
        screen.memory.is_online(),
        screen.focus == Daemon::Memory,
    );
}

/// Render one daemon panel into `area`.
///
/// Why: the two panels share their bordered-block + line-list layout; only the
/// title, body, and highlight differ.
/// What: draws a bordered [`Paragraph`] whose title carries the panel name,
/// coloured by liveness; a focused panel gets a bold cyan border.
fn render_panel(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    name: &str,
    lines: &[String],
    online: bool,
    focused: bool,
) {
    let title_color = if online { Color::Green } else { Color::Red };
    let border_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let body: Vec<Line> = lines.iter().map(|l| Line::from(l.clone())).collect();
    frame.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(Span::styled(
                    format!(" {name} "),
                    Style::default()
                        .fg(title_color)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    /// A sample online search payload for rendering tests.
    fn sample_search() -> PanelData {
        PanelData {
            version: "0.3.67".into(),
            rss_mb: 1280,
            cpu_pct: 4.0,
            uptime_secs: 3720,
            disk_bytes: 2_469_606_195,
            count_a: 3,
            count_b: 71_000,
            count_c: 0,
            count_d: 0,
        }
    }

    /// A sample online memory payload for rendering tests.
    fn sample_memory() -> PanelData {
        PanelData {
            version: "0.1.56".into(),
            rss_mb: 410,
            cpu_pct: 1.0,
            uptime_secs: 3720,
            disk_bytes: 104_857_600,
            count_a: 2,
            count_b: 8_400,
            count_c: 14,
            count_d: 1_200,
        }
    }

    #[test]
    fn default_urls_are_local() {
        assert_eq!(DEFAULT_SEARCH_URL, "http://127.0.0.1:7878");
        assert_eq!(DEFAULT_MEMORY_URL, "http://127.0.0.1:7990");
    }

    #[test]
    fn health_wire_deserializes_partial_payload() {
        // A daemon on an older build omits the issue-#35 resource fields; the
        // wire shape must still deserialize, defaulting the missing fields.
        let wire: HealthWire = serde_json::from_value(serde_json::json!({
            "status": "ok",
            "version": "0.3.67",
        }))
        .expect("partial health payload must deserialize");
        assert_eq!(wire.version, "0.3.67");
        assert_eq!(wire.rss_mb, 0);
        assert_eq!(wire.uptime_secs, 0);
    }

    #[test]
    fn project_memory_counts_reads_status_fields() {
        let status = serde_json::json!({
            "palace_count": 2,
            "total_vectors": 8400,
            "total_drawers": 14,
            "total_kg_triples": 1200,
        });
        assert_eq!(project_memory_counts(&status), (2, 8_400, 14, 1_200));
        // Absent fields default to zero rather than panicking.
        assert_eq!(project_memory_counts(&serde_json::json!({})), (0, 0, 0, 0));
    }

    #[test]
    fn format_uptime_is_compact() {
        assert_eq!(format_uptime(0), "0h 0m");
        assert_eq!(format_uptime(3720), "1h 2m");
        assert_eq!(format_uptime(7_380), "2h 3m");
    }

    #[test]
    fn format_bytes_picks_unit() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2_048), "2.0KB");
        assert_eq!(format_bytes(5_242_880), "5.0MB");
        assert_eq!(format_bytes(2_469_606_195), "2.3GB");
    }

    #[test]
    fn format_rss_picks_unit() {
        assert_eq!(format_rss(410), "410MB");
        assert_eq!(format_rss(1_023), "1023MB");
        // 1280 MB / 1024 = 1.25 GB → one-decimal rounding yields 1.2GB.
        assert_eq!(format_rss(1_280), "1.2GB");
        assert_eq!(format_rss(1_536), "1.5GB");
    }

    #[test]
    fn format_count_abbreviates_large() {
        assert_eq!(format_count(3), "3");
        assert_eq!(format_count(9_999), "9999");
        assert_eq!(format_count(71_000), "71.0k");
    }

    #[test]
    fn panel_state_is_online() {
        assert!(PanelState::Online(PanelData::default()).is_online());
        assert!(!PanelState::Connecting.is_online());
        assert!(
            !PanelState::Offline {
                last_error: "x".into()
            }
            .is_online()
        );
    }

    #[test]
    fn search_panel_lines_format_fields() {
        let lines = search_panel_lines(
            &PanelState::Online(sample_search()),
            "http://127.0.0.1:7878",
        );
        assert!(lines.iter().any(|l| l.contains("SEARCH [●] v0.3.67")));
        // 1280 MB renders as 1.2GB (1280 / 1024 = 1.25, rounded to one place).
        assert!(lines.iter().any(|l| l.contains("RSS: 1.2GB")));
        assert!(lines.iter().any(|l| l.contains("CPU: 4%")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Indexes: 3") && l.contains("Chunks: 71.0k"))
        );
        assert!(lines.iter().any(|l| l.contains("Disk: 2.3GB")));
        assert!(lines.iter().any(|l| l.contains("[S]start [X]stop")));
    }

    #[test]
    fn memory_panel_lines_format_fields() {
        let lines = memory_panel_lines(
            &PanelState::Online(sample_memory()),
            "http://127.0.0.1:7990",
        );
        assert!(lines.iter().any(|l| l.contains("MEMORY [●] v0.1.56")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Palaces: 2") && l.contains("Vectors: 8400"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Drawers: 14") && l.contains("KG: 1200"))
        );
    }

    #[test]
    fn panel_lines_render_each_state() {
        // Connecting: a single placeholder line naming the target.
        let connecting = search_panel_lines(&PanelState::Connecting, "http://x");
        assert_eq!(connecting.len(), 1);
        assert!(connecting[0].contains("connecting"));

        // Offline: carries the unreachable address and the captured error.
        let offline = memory_panel_lines(
            &PanelState::Offline {
                last_error: "connection refused".into(),
            },
            "http://127.0.0.1:7990",
        );
        assert!(offline.iter().any(|l| l.contains("OFFLINE")));
        assert!(offline.iter().any(|l| l.contains("connection refused")));
        assert!(offline.iter().any(|l| l.contains("retrying every 5s")));
        assert!(offline.iter().any(|l| l.contains("[S]start [X]stop")));
    }

    #[test]
    fn online_panel_renders_missing_version_safely() {
        // A daemon that omitted `version` must not render `v` with nothing.
        let mut data = sample_search();
        data.version.clear();
        let lines = search_panel_lines(&PanelState::Online(data), "http://x");
        assert!(lines.iter().any(|l| l.contains("SEARCH [●] ?")));
    }

    #[test]
    fn new_screen_starts_connecting() {
        let screen = HealthScreen::new("http://a", "http://b");
        assert_eq!(screen.search, PanelState::Connecting);
        assert_eq!(screen.memory, PanelState::Connecting);
        assert_eq!(screen.search_url, "http://a");
        assert_eq!(screen.focus, Daemon::Search);
    }

    #[test]
    fn toggle_focus_cycles_panels() {
        let mut screen = HealthScreen::new("http://a", "http://b");
        assert_eq!(screen.focus, Daemon::Search);
        screen.toggle_focus();
        assert_eq!(screen.focus, Daemon::Memory);
        screen.toggle_focus();
        assert_eq!(screen.focus, Daemon::Search);
    }

    #[test]
    fn apply_update_routes_to_panel() {
        let mut screen = HealthScreen::new("http://a", "http://b");
        screen.apply_update(HealthUpdate {
            daemon: Daemon::Search,
            state: PanelState::Online(sample_search()),
        });
        assert!(screen.search.is_online());
        // The memory panel must be untouched by a search-targeted update.
        assert_eq!(screen.memory, PanelState::Connecting);

        screen.apply_update(HealthUpdate {
            daemon: Daemon::Memory,
            state: PanelState::Offline {
                last_error: "timeout".into(),
            },
        });
        assert!(matches!(screen.memory, PanelState::Offline { .. }));
        // The search panel must still be online.
        assert!(screen.search.is_online());
    }

    #[test]
    fn focused_url_follows_focus() {
        let mut screen = HealthScreen::new("http://search", "http://memory");
        assert_eq!(screen.focused_url(), "http://search");
        screen.toggle_focus();
        assert_eq!(screen.focused_url(), "http://memory");
    }

    #[test]
    fn health_client_stores_base_url() {
        let client = client_for(Daemon::Search, "http://127.0.0.1:7878");
        assert_eq!(client.base_url(), "http://127.0.0.1:7878");
    }

    #[tokio::test]
    async fn poll_unreachable_daemon_is_offline() {
        // Port 0 is never bound; a poll against it must yield Offline carrying
        // a non-empty error string rather than panicking.
        let client = client_for(Daemon::Memory, "http://127.0.0.1:0");
        match client.poll().await {
            PanelState::Offline { last_error } => assert!(!last_error.is_empty()),
            other => panic!("expected Offline, got {other:?}"),
        }
    }

    #[test]
    fn render_health_smoke() {
        // A whole-frame render in every panel state must not panic.
        let mut screen = HealthScreen::new(DEFAULT_SEARCH_URL, DEFAULT_MEMORY_URL);
        screen.search = PanelState::Online(sample_search());
        screen.memory = PanelState::Offline {
            last_error: "connection refused".into(),
        };
        screen.focus = Daemon::Memory;
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|f| render(f, &screen))
            .expect("health render must not panic");
    }
}
