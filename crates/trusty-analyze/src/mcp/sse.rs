//! HTTP/SSE transport for the analyzer MCP server.
//!
//! Why: Some MCP clients (and remote integrations) prefer HTTP/SSE over stdio.
//! axum gives us native SSE via `response::sse::Sse`. We expose:
//! - `POST /mcp`     — synchronous JSON-RPC request/response
//! - `GET  /mcp/sse` — long-lived SSE stream with periodic keep-alive
//!
//! What: [`router`] returns an axum `Router` with the dispatcher cloned into
//! shared state. The SSE endpoint emits a `ready` event immediately, then
//! sends keep-alive pings every 15s.
//!
//! Test: `router_builds` smoke-tests construction; full SSE semantics are
//! exercised by the Claude Code MCP harness in CI.

use super::{AnalyzerMcpServer, Request, Response};
use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::{self, Stream};
use std::{convert::Infallible, sync::Arc, time::Duration};

/// Build the axum router carrying an [`AnalyzerMcpServer`] in shared state.
pub fn router(server: AnalyzerMcpServer) -> Router {
    Router::new()
        .route("/mcp", post(json_rpc_handler))
        .route("/mcp/sse", get(sse_handler))
        .with_state(Arc::new(server))
}

async fn json_rpc_handler(
    State(server): State<Arc<AnalyzerMcpServer>>,
    Json(req): Json<Request>,
) -> Json<Response> {
    Json(server.dispatch(req).await)
}

async fn sse_handler(
    State(_server): State<Arc<AnalyzerMcpServer>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Single-shot 'ready' event; KeepAlive emits comments every 15s so
    // intermediaries (nginx, Cloudflare) don't time out the connection.
    let ready = Event::default().event("ready").data("{}");
    let stream = stream::iter(vec![Ok(ready)]);
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn router_builds() {
        let server = AnalyzerMcpServer::new("http://127.0.0.1:1");
        let _r: Router = router(server);
    }
}
