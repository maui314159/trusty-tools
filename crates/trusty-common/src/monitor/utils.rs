//! Shared helpers for the service-specific monitor TUIs.
//!
//! Why: the trusty-search and trusty-memory TUIs ([`super::search_tui`] and
//! [`super::memory_tui`]) both need the same small primitives — a bounded,
//! timestamped activity log, an uptime formatter, and the daemon liveness
//! status enum. Centralising them here keeps the two TUIs consistent and lets
//! the pure pieces be unit-tested without a terminal.
//! What: [`DaemonStatus`] models the connection state both headers render;
//! [`fmt_uptime`] turns a second count into `Xh Ym`; [`timestamped`] prefixes a
//! line with `[HH:MM:SS]`; [`ActivityLog`] is a [`VecDeque`] capped at
//! [`ActivityLog::MAX_ENTRIES`] entries.
//! Test: `cargo test -p trusty-common --features monitor-tui` covers every
//! function in this module.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// The liveness state of a monitored daemon.
///
/// Why: both service TUIs render a coloured liveness badge in their title bar;
/// a shared typed enum keeps that rendering exhaustive and consistent.
/// What: `Connecting` before the first poll, `Online` with the daemon version
/// and uptime, or `Offline` carrying the last error string.
/// Test: `test_daemon_status_is_online`, `test_daemon_status_badge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    /// The first poll has not completed yet.
    Connecting,
    /// The daemon answered its health probe.
    Online {
        /// The daemon's reported version string.
        version: String,
        /// Daemon uptime in whole seconds.
        uptime_secs: u64,
    },
    /// The daemon is unreachable; carries the last poll error.
    Offline {
        /// The error captured from the most recent failed poll.
        last_error: String,
    },
}

impl DaemonStatus {
    /// Whether the daemon is currently online.
    ///
    /// Why: the title-bar badge and several key handlers branch on
    /// reachability.
    /// What: returns `true` only for [`DaemonStatus::Online`].
    /// Test: `test_daemon_status_is_online`.
    pub fn is_online(&self) -> bool {
        matches!(self, DaemonStatus::Online { .. })
    }

    /// The status badge `(glyph, label)` for this daemon state.
    ///
    /// Why: the title bar shows a compact liveness indicator; centralising the
    /// mapping keeps both TUIs in sync.
    /// What: `● online`, `◌ connecting`, or `○ offline`.
    /// Test: `test_daemon_status_badge`.
    pub fn badge(&self) -> (char, &'static str) {
        match self {
            DaemonStatus::Online { .. } => ('●', "online"),
            DaemonStatus::Connecting => ('◌', "connecting"),
            DaemonStatus::Offline { .. } => ('○', "offline"),
        }
    }
}

/// Format a daemon uptime in seconds as a compact `Xh Ym` string.
///
/// Why: the title bar shows uptime; raw seconds are hard to read.
/// What: returns `"{hours}h {minutes}m"`, e.g. `7440` → `"2h 4m"`. Sub-minute
/// uptimes show `"0h 0m"`.
/// Test: `test_fmt_uptime`.
pub fn fmt_uptime(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    format!("{hours}h {minutes}m")
}

/// Prefix a log line with the current wall-clock time as `[HH:MM:SS]`.
///
/// Why: every activity-log entry is timestamped so the operator can correlate
/// events; the TUIs avoid pulling in `chrono` for this one formatter.
/// What: derives `HH:MM:SS` from [`SystemTime::now`] in UTC and returns
/// `"[HH:MM:SS] {msg}"`. A clock before the Unix epoch (impossible in
/// practice) falls back to `[00:00:00]`.
/// Test: `test_timestamped_format`.
pub fn timestamped(msg: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    let hh = day_secs / 3600;
    let mm = (day_secs % 3600) / 60;
    let ss = day_secs % 60;
    format!("[{hh:02}:{mm:02}:{ss:02}] {msg}")
}

/// A bounded, append-only activity log shared by both service TUIs.
///
/// Why: each TUI streams indexing / recall / dream events into a scrolling
/// "ACTIVITY" panel; an unbounded log would grow without limit over a long
/// session, so the buffer is capped and the oldest lines are dropped.
/// What: wraps a [`VecDeque<String>`] capped at [`Self::MAX_ENTRIES`]; `push`
/// timestamps and appends a line, `push_raw` appends an already-formatted
/// (e.g. indented continuation) line.
/// Test: `test_log_max_capacity`, `test_log_push_timestamps`.
#[derive(Debug, Clone, Default)]
pub struct ActivityLog {
    entries: VecDeque<String>,
}

impl ActivityLog {
    /// Hard cap on the number of retained log lines.
    ///
    /// Why: bounds the memory the activity panel can consume over a long-lived
    /// session; 500 lines is far more than any terminal can show at once.
    /// What: the maximum [`VecDeque`] length; the oldest line is evicted on
    /// overflow.
    /// Test: `test_log_max_capacity`.
    pub const MAX_ENTRIES: usize = 500;

    /// Build an empty activity log.
    ///
    /// Why: each TUI starts with no recorded activity.
    /// What: returns a log with an empty backing deque.
    /// Test: `test_log_starts_empty`.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    /// Timestamp `msg` and append it, evicting the oldest line on overflow.
    ///
    /// Why: the common case — record a fresh event with a `[HH:MM:SS]` prefix.
    /// What: pushes `timestamped(msg)`; when the deque exceeds
    /// [`Self::MAX_ENTRIES`] the front (oldest) line is dropped.
    /// Test: `test_log_max_capacity`, `test_log_push_timestamps`.
    pub fn push(&mut self, msg: impl AsRef<str>) {
        self.push_raw(timestamped(msg.as_ref()));
    }

    /// Append an already-formatted line verbatim, evicting on overflow.
    ///
    /// Why: continuation lines (indented search results, dream sub-stats) are
    /// written without their own timestamp so they read as part of the event
    /// above them.
    /// What: pushes `line` unchanged; enforces the [`Self::MAX_ENTRIES`] cap.
    /// Test: `test_log_max_capacity`.
    pub fn push_raw(&mut self, line: impl Into<String>) {
        self.entries.push_back(line.into());
        while self.entries.len() > Self::MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// Number of retained log lines.
    ///
    /// Why: the renderer scrolls to the tail; tests assert the cap.
    /// What: returns the backing deque length.
    /// Test: `test_log_max_capacity`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log has no entries.
    ///
    /// Why: clippy's `len_without_is_empty` lint, and the renderer shows a
    /// placeholder when empty.
    /// What: returns `true` when the backing deque is empty.
    /// Test: `test_log_starts_empty`.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The last `n` lines, oldest-first, for rendering the visible tail.
    ///
    /// Why: the activity panel shows only the lines that fit; the renderer
    /// asks for as many as the panel height allows.
    /// What: returns a borrowed iterator over the final `min(n, len)` lines.
    /// Test: `test_log_tail`.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = &String> {
        let skip = self.entries.len().saturating_sub(n);
        self.entries.iter().skip(skip)
    }

    /// Every line, oldest-first.
    ///
    /// Why: the renderer maps lines to ratatui `ListItem`s; some tests assert
    /// on the full contents.
    /// What: returns a borrowed iterator over all retained lines.
    /// Test: `test_log_push_timestamps`.
    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_status_is_online() {
        assert!(
            DaemonStatus::Online {
                version: "1.0".into(),
                uptime_secs: 10,
            }
            .is_online()
        );
        assert!(!DaemonStatus::Connecting.is_online());
        assert!(
            !DaemonStatus::Offline {
                last_error: "x".into(),
            }
            .is_online()
        );
    }

    #[test]
    fn test_daemon_status_badge() {
        let online = DaemonStatus::Online {
            version: "1.0".into(),
            uptime_secs: 0,
        };
        assert_eq!(online.badge(), ('●', "online"));
        assert_eq!(DaemonStatus::Connecting.badge(), ('◌', "connecting"));
        assert_eq!(
            DaemonStatus::Offline {
                last_error: "x".into()
            }
            .badge(),
            ('○', "offline")
        );
    }

    #[test]
    fn test_fmt_uptime() {
        assert_eq!(fmt_uptime(7440), "2h 4m");
        assert_eq!(fmt_uptime(0), "0h 0m");
        assert_eq!(fmt_uptime(59), "0h 0m");
        assert_eq!(fmt_uptime(3600), "1h 0m");
        assert_eq!(fmt_uptime(3661), "1h 1m");
    }

    #[test]
    fn test_timestamped_format() {
        // The shape must be exactly `[HH:MM:SS] message` — two digits per
        // field, colon-separated, single space before the payload.
        let line = timestamped("hello world");
        assert!(line.ends_with(" hello world"), "payload preserved: {line}");
        assert!(line.starts_with('['), "starts with bracket: {line}");
        let bytes = line.as_bytes();
        // [HH:MM:SS] is 10 chars: '[' + 8 + ']'.
        assert_eq!(bytes[0], b'[');
        assert_eq!(bytes[9], b']');
        assert_eq!(bytes[10], b' ');
        for i in [1, 2, 4, 5, 7, 8] {
            assert!(bytes[i].is_ascii_digit(), "digit at {i}: {line}");
        }
        assert_eq!(bytes[3], b':');
        assert_eq!(bytes[6], b':');
    }

    #[test]
    fn test_log_starts_empty() {
        let log = ActivityLog::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn test_log_max_capacity() {
        // Pushing well past the cap must drop the oldest entries, never grow
        // beyond MAX_ENTRIES, and retain the most recent line.
        let mut log = ActivityLog::new();
        for i in 0..(ActivityLog::MAX_ENTRIES + 250) {
            log.push_raw(format!("line {i}"));
        }
        assert_eq!(log.len(), ActivityLog::MAX_ENTRIES);
        // The oldest surviving line is entry #250 (250 evicted).
        let first = log.iter().next().expect("non-empty log");
        assert_eq!(first, "line 250");
        let last = log.iter().last().expect("non-empty log");
        assert_eq!(last, "line 749");
    }

    #[test]
    fn test_log_push_timestamps() {
        let mut log = ActivityLog::new();
        log.push("event happened");
        let line = log.iter().next().expect("one entry");
        assert!(line.starts_with('['), "timestamped: {line}");
        assert!(line.ends_with(" event happened"));
    }

    #[test]
    fn test_log_tail() {
        let mut log = ActivityLog::new();
        for i in 0..10 {
            log.push_raw(format!("l{i}"));
        }
        let tail: Vec<&String> = log.tail(3).collect();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0], "l7");
        assert_eq!(tail[2], "l9");
        // Asking for more than exist clamps to the available count.
        assert_eq!(log.tail(100).count(), 10);
    }
}
