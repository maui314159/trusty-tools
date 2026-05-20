//! Thin `open-mpm` binary wrapper.
//!
//! Why: The entire startup pipeline (argv parsing, env load, tracing init,
//!      subcommand dispatch, REPL/CTRL fallback) lives in the library at
//!      `open_mpm::runtime::run`. Hosting it in the library lets private
//!      launchers (`open-mpm-local`) install additional agent plugins via
//!      `open_mpm::install_plugins(...)` BEFORE invoking `run()`, without
//!      polluting the published `open-mpm` crate with references to
//!      `publish = false` agent crates such as `cto-assistant`.
//! What: Standard `#[tokio::main]` entry point that delegates straight to
//!       `open_mpm::run()`. No additional setup, no plugin wiring — the
//!       published binary ships with an empty plugin registry by default.
//! Test: `cargo run -p open-mpm -- --version` prints the build banner.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    open_mpm::run().await
}
