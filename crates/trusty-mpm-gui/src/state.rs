//! Shared application state for the trusty-mpm Tauri shell.
//!
//! Why: The Tauri shell is a thin proxy — every command needs the daemon base
//! URL and a reusable HTTP client. Holding both in one managed struct avoids
//! re-reading the env var and re-building a `reqwest::Client` (connection-pool
//! owner) on every IPC call.
//! What: `GuiState` carries the resolved daemon URL and a shared `reqwest`
//! client; it is registered once via `tauri::Manager::manage`.
//! Test: Construct `GuiState::new()` with `TRUSTY_MPM_URL` unset and assert
//! `daemon_url` equals the default `http://127.0.0.1:7880`; set the env var
//! and assert it is honored.

/// Default daemon REST endpoint when `TRUSTY_MPM_URL` is not set.
pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7880";

/// Managed Tauri state — daemon URL plus a pooled HTTP client.
pub struct GuiState {
    /// Base URL of the trusty-mpm daemon REST API (no trailing slash).
    pub daemon_url: String,
    /// Shared HTTP client used by every command to proxy to the daemon.
    pub client: reqwest::Client,
}

impl GuiState {
    /// Build state from the environment.
    ///
    /// Why: Lets ops point the desktop app at a non-local daemon without a
    /// rebuild by exporting `TRUSTY_MPM_URL`.
    /// What: Reads `TRUSTY_MPM_URL` (falling back to the default), trims any
    /// trailing slash, and constructs a default `reqwest::Client`.
    /// Test: Unset the env var → `daemon_url == DEFAULT_DAEMON_URL`.
    pub fn new() -> Self {
        let daemon_url = std::env::var("TRUSTY_MPM_URL")
            .unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        Self {
            daemon_url,
            client: reqwest::Client::new(),
        }
    }
}

impl Default for GuiState {
    fn default() -> Self {
        Self::new()
    }
}
