//! `tickets-mcp` binary — stdio MCP server.
//!
//! Why: Claude Code (and other MCP hosts) launch this process per session
//! and talk to it over stdin/stdout JSON-RPC.
//! What: Initialises tracing, loads config + builds a `BackendClient`,
//! and runs the stdio loop until EOF.
//! Test: Manual via `claude mcp add` / piping JSON-RPC to stdin.

use std::sync::Arc;

use trusty_tickets::api::client::BackendClient;
use trusty_tickets::api::config::Config;
use trusty_tickets::server::{AppState, run_stdio};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    trusty_common::init_tracing(0);
    let config = Config::load()?;
    let client = BackendClient::from_config(config).await?;
    let state = AppState {
        client: Arc::new(client),
    };
    run_stdio(state).await
}
