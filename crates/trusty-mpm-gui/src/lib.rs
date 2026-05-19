//! trusty-mpm desktop shell (Tauri 2).
//!
//! Why: Provides a native window for the trusty-mpm fleet dashboard while the
//! same Svelte frontend also ships as a standalone browser build. The Rust
//! side is intentionally thin — it owns no fleet state and only proxies IPC
//! calls to the trusty-mpm daemon REST API (`TRUSTY_MPM_URL`, default
//! `http://127.0.0.1:7880`).
//! What: Registers `GuiState` and five proxy commands, then runs the Tauri
//! event loop.
//! Test: `cargo check` on this crate passes once the crate is added to the
//! workspace; launching the app and clicking the health dot reflects daemon
//! liveness.

mod commands;
mod state;

use state::GuiState;

/// Build and run the Tauri application.
///
/// Why: Single entry point shared by `main.rs` (and any future test harness)
/// so the builder configuration lives in one place.
/// What: Manages `GuiState`, registers all IPC commands, and runs the
/// generated Tauri context.
/// Test: Invoked from `main`; app window opens and `invoke('check_health')`
/// returns a boolean.
pub fn run() {
    tauri::Builder::default()
        .manage(GuiState::new())
        .invoke_handler(tauri::generate_handler![
            commands::get_daemon_url,
            commands::check_health,
            commands::list_sessions,
            commands::pause_session,
            commands::resume_session,
            commands::stop_session,
            commands::get_breakers,
            commands::session_output,
            commands::coordinator_context,
            commands::coordinator_chat,
        ])
        .run(tauri::generate_context!())
        .expect("error while running trusty-mpm-gui");
}
