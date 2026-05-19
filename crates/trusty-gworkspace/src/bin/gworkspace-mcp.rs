//! `gworkspace-mcp` binary — stdio MCP server.
//!
//! Why: Claude Code (and other MCP hosts) launch this process per session
//! and talk to it over stdin/stdout JSON-RPC.
//! What: Initialises tracing, builds an `AppState` with a shared
//! `BaseClient`, then runs the stdio loop until EOF.
//! Test: Manual via `claude mcp add` / direct stdin piping.

use std::sync::Arc;

use trusty_gworkspace::api::client::BaseClient;
use trusty_gworkspace::server::{AppState, run_stdio};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    trusty_common::init_tracing(0);
    let client = BaseClient::new()?;
    let state = AppState {
        client: Arc::new(client),
    };
    run_stdio(state).await
}
