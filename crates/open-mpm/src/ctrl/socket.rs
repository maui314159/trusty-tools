//! Controller singleton socket — Unix-domain-socket front door for the CTRL REPL.
//!
//! Why: When a user runs `open-mpm` from a project directory, we want the
//! second invocation to route into the first one's controller instead of
//! spawning an independent process tree. A short probe on a well-known
//! per-project socket path lets the second process detect a live controller
//! and forward the user's input to it via NDJSON, exiting once the
//! controller acknowledges completion.
//! What: `CtrlSocket` exposes three building blocks:
//!   - `probe(path, timeout)` — try to connect with a short timeout and
//!     return the open `UnixStream` if a controller is listening.
//!   - `bind(path)` — clean up any stale file at `path` and bind a fresh
//!     `UnixListener` for inbound CLI connections.
//!   - `cleanup(path)` — remove a stale socket file (best-effort).
//! Plus path helpers (`ctrl_socket_path` / `cwd_project_id`) so main.rs and
//! the controller share one canonical location.
//! Test: see `tests` module at the bottom — exercises path generation,
//! stale-file cleanup, and the probe-timeout fallback when no listener is
//! bound.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};

/// Default probe timeout: long enough that a slow-but-alive controller is
/// detected, short enough that the user does not perceive a stall when no
/// controller is running.
///
/// Why: 50ms is well above typical Unix-socket connect latency (sub-millisecond
/// on the same host) but well below the ~200ms human-perceptible delay
/// threshold.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(50);

/// Compute a stable, filesystem-safe project id from the current working
/// directory's basename.
///
/// Why: Multiple projects share `~/.open-mpm/sockets/`; the per-project id
/// keeps each controller's socket distinct without needing a registry.
/// What: Returns the sanitized basename of `cwd` (alphanumerics, `-`, `_`,
/// `.`; everything else replaced with `_`). Falls back to `"unknown"` when
/// the cwd has no basename or is unreadable.
/// Test: `cwd_project_id_basename_from_path` covers the happy path.
pub fn cwd_project_id() -> String {
    match std::env::current_dir() {
        Ok(p) => project_id_from_path(&p),
        Err(_) => "unknown".to_string(),
    }
}

/// Derive a sanitized project id from any path's basename.
///
/// Why: Split out from `cwd_project_id` so unit tests don't need to mutate
/// the process's cwd.
pub fn project_id_from_path(path: &Path) -> String {
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    sanitize_project_id(base)
}

/// Replace any character outside `[A-Za-z0-9._-]` with `_`.
fn sanitize_project_id(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// Resolve `~/.open-mpm/sockets/<project_id>.ctrl.sock`.
///
/// Why: Centralizes path construction so every caller (probe + bind +
/// cleanup) agrees on the location. The `.ctrl.sock` suffix distinguishes
/// the controller socket from the existing `<id>.sock` inter-project
/// message-bus socket (`src/bus/mod.rs`), which lives in the same directory.
/// What: Joins the user's home dir with the canonical sockets folder.
/// Falls back to a relative `./.open-mpm/sockets/...` when no home dir is
/// available (e.g., minimal container without `$HOME`).
/// Test: `ctrl_socket_path_uses_home_when_set` and
/// `ctrl_socket_path_distinct_from_bus_path`.
pub fn ctrl_socket_path(project_id: &str) -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".open-mpm")
        .join("sockets")
        .join(format!("{project_id}.ctrl.sock"))
}

/// Returns true when an `io::Error` indicates "no peer is listening on this
/// socket" — i.e., the socket file exists but no process owns it.
///
/// Why: We treat this case as "stale socket" and cleanup + retry binding
/// rather than aborting. Other I/O errors (permission denied, I/O failure)
/// should bubble up.
/// What: Matches `ConnectionRefused` (the most common stale-socket signal)
/// plus a couple of platform variants we have observed.
pub fn is_connection_refused(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

/// Helpers grouped under one type for clear ergonomics at the call site.
pub struct CtrlSocket;

impl CtrlSocket {
    /// Try to connect to an existing controller at `path` within `timeout`.
    ///
    /// Why: Lets the CLI determine "is a controller already running?" with
    /// a hard upper bound on latency. Used at startup before the process
    /// commits to becoming the controller itself.
    /// What: Wraps `UnixStream::connect` in `tokio::time::timeout`. Returns
    /// `Err(io::ErrorKind::TimedOut)` when the connect doesn't complete in
    /// time so the caller can treat timeout the same as a non-listening
    /// socket. All other connect errors propagate unchanged.
    /// Test: `probe_times_out_when_no_listener`.
    pub async fn probe(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
        match tokio::time::timeout(timeout, UnixStream::connect(path)).await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "ctrl socket probe timed out",
            )),
        }
    }

    /// Convenience: probe with the default `DEFAULT_PROBE_TIMEOUT`.
    pub async fn probe_default(path: &Path) -> io::Result<UnixStream> {
        Self::probe(path, DEFAULT_PROBE_TIMEOUT).await
    }

    /// Bind the socket and return a listener for incoming CLI connections.
    ///
    /// Why: Encapsulates parent-dir creation + stale-file cleanup + the
    /// actual bind, which would otherwise be duplicated between the
    /// controller startup path and any future "restart the listener" code.
    /// What: Creates `~/.open-mpm/sockets/`, removes any stale file at
    /// `path`, then binds. Caller is responsible for `tokio::spawn`-ing the
    /// accept loop and removing the socket file on shutdown.
    /// Test: `bind_creates_listener_and_replaces_stale_file`.
    pub async fn bind(path: &Path) -> io::Result<UnixListener> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Remove stale socket before binding (otherwise bind fails with EADDRINUSE).
        Self::cleanup(path);
        UnixListener::bind(path)
    }

    /// Remove a stale socket file. Best-effort: errors are ignored.
    ///
    /// Why: Called from two paths (after a failed probe in main.rs, and
    /// inside `bind` before re-binding) so factoring it out keeps the
    /// behavior consistent.
    /// What: Synchronous `std::fs::remove_file`. Doesn't care if the file
    /// is already gone.
    /// Test: `cleanup_removes_stale_file_and_is_idempotent`.
    pub fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sanitize_project_id_replaces_unsafe_chars() {
        assert_eq!(sanitize_project_id("open-mpm"), "open-mpm");
        assert_eq!(sanitize_project_id("my project"), "my_project");
        assert_eq!(sanitize_project_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_project_id(""), "unknown");
    }

    #[test]
    fn cwd_project_id_basename_from_path() {
        let p = PathBuf::from("/Users/masa/Projects/open-mpm");
        assert_eq!(project_id_from_path(&p), "open-mpm");

        let p = PathBuf::from("/tmp/has space/proj!");
        assert_eq!(project_id_from_path(&p), "proj_");
    }

    #[test]
    fn ctrl_socket_path_uses_home_when_set() {
        let path = ctrl_socket_path("demo");
        let s = path.to_string_lossy().into_owned();
        assert!(s.ends_with("/.open-mpm/sockets/demo.ctrl.sock"), "{s}");
    }

    /// Why: The bus already uses `<id>.sock` in the same directory; a name
    /// collision would silently break inter-project messaging when both
    /// systems try to bind. We assert path divergence to lock in the
    /// invariant.
    #[test]
    fn ctrl_socket_path_distinct_from_bus_path() {
        let ctrl = ctrl_socket_path("foo");
        let bus = crate::bus::MessageBus::socket_path_for("foo").unwrap();
        assert_ne!(ctrl, bus);
    }

    #[tokio::test]
    async fn probe_times_out_when_no_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ghost.ctrl.sock");
        let result = CtrlSocket::probe(&path, Duration::from_millis(50)).await;
        // No file exists, so connect returns NotFound (or ConnectionRefused
        // on some platforms). Either way it should be classified as
        // "controller not present" via `is_connection_refused`.
        let err = result.expect_err("expected probe failure");
        assert!(
            is_connection_refused(&err) || err.kind() == io::ErrorKind::TimedOut,
            "unexpected probe error kind: {:?}",
            err.kind()
        );
    }

    #[tokio::test]
    async fn cleanup_removes_stale_file_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stale.ctrl.sock");
        tokio::fs::write(&path, b"").await.unwrap();
        assert!(path.exists());
        CtrlSocket::cleanup(&path);
        assert!(!path.exists());
        // Second cleanup must not panic / error.
        CtrlSocket::cleanup(&path);
    }

    #[tokio::test]
    async fn bind_creates_listener_and_replaces_stale_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ctrl.sock");
        // Pre-seed a stale file so bind has to clean up.
        tokio::fs::write(&path, b"stale").await.unwrap();

        let listener = CtrlSocket::bind(&path).await.expect("bind should succeed");

        // Probe it to confirm the listener is live.
        let stream = CtrlSocket::probe(&path, Duration::from_millis(200))
            .await
            .expect("probe should connect to fresh listener");
        drop(stream);
        drop(listener);
        CtrlSocket::cleanup(&path);
    }

    #[tokio::test]
    async fn probe_succeeds_against_live_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("live.ctrl.sock");
        let _listener = CtrlSocket::bind(&path).await.unwrap();
        let stream = CtrlSocket::probe_default(&path)
            .await
            .expect("probe should connect");
        drop(stream);
        CtrlSocket::cleanup(&path);
    }
}
