//! Transport abstraction for JSON-RPC clients.
//!
//! Why: `trpc` needs to talk to both subprocess-style MCP servers (stdin/stdout
//! framed JSON) and HTTP JSON-RPC endpoints with a single client implementation.
//! What: Defines a single `Transport` trait with one `send` method; concrete
//! implementations own their own connection lifecycle.
//! Test: covered indirectly through `RpcClient` unit tests and integration tests.

use anyhow::Result;
use serde_json::Value;

pub mod http;
pub mod stdio;

pub use http::HttpTransport;
pub use stdio::StdioTransport;

/// Abstract send-one-JSON-RPC-request channel.
///
/// Why: hides stdio-subprocess vs HTTP behind a uniform async surface so
/// `RpcClient` is transport-agnostic.
/// What: a single `send` method that takes a fully-formed JSON-RPC `Value`
/// and returns the response `Value`. For notifications (no `id`), implementations
/// should write the message and return `Value::Null` without blocking on a reply.
/// Test: exercised through client.rs and the integration tests under `tests/`.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Send one JSON-RPC request, return the full response Value.
    ///
    /// For requests with no `id` field (JSON-RPC notifications), implementations
    /// must NOT wait for a response; they should return `Ok(Value::Null)` after
    /// writing the message.
    async fn send(&self, request: Value) -> Result<Value>;
}

/// Helper: a request is a notification if it has no `id` field.
pub(crate) fn is_notification(req: &Value) -> bool {
    req.get("id").is_none()
}
