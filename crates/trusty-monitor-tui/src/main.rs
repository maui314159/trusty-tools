//! `trusty-monitor` — unified TUI dashboard for trusty-search and trusty-memory.
//!
//! Why: a thin binary entry point so the dashboard logic lives in the library
//! and stays unit-testable; the binary just initializes tracing and hands off.
//! What: initializes the shared tracing subscriber (logs to stderr so they do
//! not corrupt the alternate-screen TUI) and calls [`trusty_monitor_tui::run`].
//! Test: `cargo run -p trusty-monitor-tui` launches the live dashboard.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Verbosity 0 → warn-level; RUST_LOG overrides. Logs go to stderr.
    trusty_common::init_tracing(0);
    trusty_monitor_tui::run().await
}
