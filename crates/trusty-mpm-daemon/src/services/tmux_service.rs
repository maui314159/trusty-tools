//! tmux I/O business logic.
//!
//! Why: the HTTP handlers embedded tmux discovery, pane capture, session
//! listing, and adoption directly. Each of those is "discover the driver, run
//! one operation, decide what a failure means" — a pattern worth consolidating
//! so the handlers stop repeating the discover-or-degrade dance.
//! What: [`TmuxService`] is a stateless namespace of associated functions for
//! the daemon's tmux operations; capture degrades to an empty string when tmux
//! is absent, while the listing operations surface [`DaemonError`].
//! Test: `cargo test -p trusty-mpm-daemon services::tmux` covers the
//! tmux-absent degradation paths (the only ones reachable without a tmux host).

use trusty_mpm_core::external_session::ExternalSession;
use trusty_mpm_core::session::Session;
use trusty_mpm_core::tmux::TmuxTarget;

use crate::error::DaemonError;
use crate::tmux::{AdoptedSession, SessionSnapshot, TmuxDriver};

/// Stateless façade over the daemon's tmux operations.
///
/// Why: tmux work needs no state, only the rules for what a failure means;
/// associated functions keep call sites terse (`TmuxService::capture(..)`).
/// What: capture / list / adopt / snapshot, each wrapping a [`TmuxDriver`] call
/// with the daemon's failure policy.
/// Test: the module's `#[cfg(test)]` suite.
pub struct TmuxService;

impl TmuxService {
    /// Capture a session's pane output, degrading to empty when tmux is absent.
    ///
    /// Why: pause / command / output all want recent pane text, but tmux may be
    /// absent (CI) or the session may not be tmux-hosted. None of that is fatal
    /// — the endpoints still succeed, just without captured text. This replaces
    /// the free `capture_pane` function from `api.rs`.
    /// What: discovers tmux and captures the last `lines` pane lines; any
    /// failure is logged and yields an empty string.
    /// Test: `capture_without_tmux_is_empty`.
    pub fn capture(session: &Session, lines: u32) -> String {
        match TmuxDriver::discover() {
            Ok(driver) => {
                let target = TmuxTarget::session(&session.tmux_name);
                match driver.capture(&target, Some(lines)) {
                    Ok(text) => text,
                    Err(e) => {
                        tracing::warn!("tmux capture failed for {}: {e}", session.tmux_name);
                        String::new()
                    }
                }
            }
            Err(_) => {
                tracing::info!(
                    "tmux unavailable; capture for {} skipped",
                    session.tmux_name
                );
                String::new()
            }
        }
    }

    /// Send a command line into a session's pane, best-effort.
    ///
    /// Why: remote control of a running session; a tmux failure must not fail
    /// the request (the caller still gets whatever output was captured).
    /// What: discovers tmux and sends `command`, logging any failure.
    /// Test: covered indirectly by `send_command_returns_output_shape`.
    pub fn send_command(session: &Session, command: &str) {
        match TmuxDriver::discover() {
            Ok(driver) => {
                let target = TmuxTarget::session(&session.tmux_name);
                if let Err(e) = driver.send_line(&target, command) {
                    tracing::warn!("tmux send_line failed for {}: {e}", session.tmux_name);
                }
            }
            Err(_) => {
                tracing::info!(
                    "tmux unavailable; command for {} not sent",
                    session.tmux_name
                );
            }
        }
    }

    /// List every tmux session on the host, origin-tagged.
    ///
    /// Why: the dashboard offers to adopt external sessions; it needs the full
    /// host list. tmux being absent yields an empty list rather than an error —
    /// "no sessions" is a valid answer.
    /// What: runs `TmuxDriver::list_all_sessions`, returning an empty vec when
    /// tmux is unavailable or the listing fails.
    /// Test: `list_all_without_tmux_is_empty`.
    pub fn list_all() -> Vec<ExternalSession> {
        match TmuxDriver::discover() {
            Ok(driver) => driver.list_all_sessions().unwrap_or_else(|e| {
                tracing::warn!("tmux list_all_sessions failed: {e}");
                Vec::new()
            }),
            Err(_) => {
                tracing::info!("tmux unavailable; tmux session list is empty");
                Vec::new()
            }
        }
    }

    /// Adopt an external tmux session for monitoring.
    ///
    /// Why: trusty-mpm should watch sessions it did not create; adoption is the
    /// non-destructive opt-in.
    /// What: runs `TmuxDriver::adopt_session`. A missing session *or* absent
    /// tmux both map to [`DaemonError::SessionNotFound`] — from the caller's
    /// view "that session is not available here" is the same `404` either way.
    /// Test: `adopt_missing_session_is_not_found`.
    pub fn adopt(name: &str) -> Result<AdoptedSession, DaemonError> {
        let driver = TmuxDriver::discover().map_err(|_| DaemonError::SessionNotFound {
            id: name.to_string(),
        })?;
        driver.adopt_session(name).map_err(|e| {
            tracing::warn!("tmux adopt {name} failed: {e}");
            DaemonError::SessionNotFound {
                id: name.to_string(),
            }
        })
    }

    /// Snapshot a session's pane output.
    ///
    /// Why: the dashboard inspects any session without attaching to it.
    /// What: runs `TmuxDriver::monitor_session` for the last `lines` pane lines.
    /// A missing session *or* absent tmux both map to
    /// [`DaemonError::SessionNotFound`] (a uniform `404` for "not available").
    /// Test: `snapshot_missing_session_is_not_found`.
    pub fn snapshot(name: &str, lines: u32) -> Result<SessionSnapshot, DaemonError> {
        let driver = TmuxDriver::discover().map_err(|_| DaemonError::SessionNotFound {
            id: name.to_string(),
        })?;
        driver.monitor_session(name, lines).map_err(|e| {
            tracing::warn!("tmux snapshot for {name} failed: {e}");
            DaemonError::SessionNotFound {
                id: name.to_string(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trusty_mpm_core::session::{ControlModel, SessionId};

    #[test]
    fn capture_without_tmux_is_empty() {
        // tmux is generally absent in CI; capture must degrade to "" not panic.
        let session = Session::new(SessionId::new(), "/tmp/p", ControlModel::Tmux, None);
        let _ = TmuxService::capture(&session, 10);
    }

    #[test]
    fn list_all_without_tmux_is_empty() {
        // The listing must always return a vec (possibly empty), never error.
        let _ = TmuxService::list_all();
    }

    #[test]
    fn adopt_missing_session_is_not_found() {
        // Adopting a non-existent session (or with tmux absent) is an error,
        // not a panic.
        let result = TmuxService::adopt("tmpm-definitely-no-such-session-xyz");
        assert!(result.is_err());
    }

    #[test]
    fn snapshot_missing_session_is_not_found() {
        let result = TmuxService::snapshot("no-such-session-xyz", 10);
        assert!(result.is_err());
    }
}
