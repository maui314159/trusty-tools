//! JSON-RPC client wrapper.
//!
//! Why: encapsulates JSON-RPC envelope construction, id generation, and error
//! extraction so callers (the CLI and any library consumer) don't have to know
//! the wire format.
//! What: `RpcClient` owns an `Arc<dyn Transport>` and exposes typed helpers for
//! the MCP methods we care about (`initialize`, `tools/list`, `tools/call`) plus
//! a generic `request` for everything else.
//! Test: `parse_args` is unit-tested in `main.rs`; client behaviour through the
//! integration tests under `tests/`.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::transport::Transport;

/// Generate a fresh JSON-RPC request id (UUID v4).
/// Why: JSON-RPC servers correlate requests/responses by id; we just need uniqueness per call.
/// What: Returns a fresh UUID v4 as a string.
/// Test: Used implicitly by every `client::*` test.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Extract the `result` from a JSON-RPC response, or surface the error.
///
/// Why: every method should fail loudly when the server returns a JSON-RPC
/// error object.
/// What: returns `Ok(result)` if present; if `error` is present, formats it
/// as an `anyhow::Error`. If neither is present (e.g. notification reply),
/// returns the whole response.
/// Test: `extract_result_*` unit tests below.
pub fn extract_result(resp: Value) -> Result<Value> {
    if let Some(error) = resp.get("error") {
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!("JSON-RPC error {code}: {message}");
    }
    Ok(resp.get("result").cloned().unwrap_or(resp))
}

/// JSON-RPC client wrapping any transport.
/// Why: Single facade so callers don't hand-roll JSON-RPC envelopes per call.
/// What: Wraps an `Arc<dyn Transport>` and exposes `initialize`, `tools_list`, `tools_call`, and a generic `request`.
/// Test: Integration tests under `tests/` drive every method.
pub struct RpcClient {
    transport: Arc<dyn Transport>,
}

impl RpcClient {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }

    /// Send `initialize` + `notifications/initialized` per the MCP handshake.
    ///
    /// Why: MCP requires the client send `initialize` and wait for the response
    /// before any other request; the follow-up `notifications/initialized`
    /// notification signals readiness.
    /// What: builds the initialize payload with this crate's version as the
    /// client info, sends it, then fires the notification (no response).
    /// Test: covered indirectly by smoke tests against real MCP servers.
    pub async fn initialize(&self) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": new_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "trpc",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        });
        let resp = self
            .transport
            .send(req)
            .await
            .context("sending initialize")?;
        let result = extract_result(resp)?;

        // Fire-and-forget the initialized notification. Errors here are
        // non-fatal: some HTTP gateways won't accept it.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = self.transport.send(notif).await;

        Ok(result)
    }

    /// Call `tools/list`.
    pub async fn tools_list(&self) -> Result<Value> {
        self.request("tools/list", Some(json!({}))).await
    }

    /// Call `tools/call` with the given name and arguments object.
    pub async fn tools_call(&self, name: &str, args: Value) -> Result<Value> {
        self.request("tools/call", Some(json!({"name": name, "arguments": args})))
            .await
    }

    /// Send an arbitrary JSON-RPC method with optional params.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": new_id(),
            "method": method,
            "params": params.unwrap_or_else(|| json!({})),
        });
        let resp = self
            .transport
            .send(req)
            .await
            .with_context(|| format!("sending {method}"))?;
        extract_result(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_result_returns_inner_result() {
        let resp = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        let out = extract_result(resp).unwrap();
        assert_eq!(out, json!({"ok": true}));
    }

    #[test]
    fn extract_result_bails_on_error_object() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "Method not found"}
        });
        let err = extract_result(resp).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("-32601"), "msg = {msg}");
        assert!(msg.contains("Method not found"), "msg = {msg}");
    }

    #[test]
    fn extract_result_passthrough_when_no_result_or_error() {
        let resp = json!({"jsonrpc": "2.0", "id": 1});
        let out = extract_result(resp.clone()).unwrap();
        assert_eq!(out, resp);
    }

    #[test]
    fn new_id_is_unique() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
    }
}
