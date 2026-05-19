//! HTTP JSON-RPC transport.
//!
//! Why: Some JSON-RPC services expose plain HTTP endpoints rather than stdio.
//! What: POSTs each request as `application/json` to the configured URL and
//! deserialises the response body. For notifications, fires the request but
//! ignores any response body.
//! Test: covered by `tests/integration.rs::http_transport_roundtrip` using a
//! lightweight inline tokio TCP server.

use anyhow::{Context, Result};
use serde_json::Value;

use super::{Transport, is_notification};

/// HTTP JSON-RPC transport (POST + JSON body).
/// Why: HTTP JSON-RPC endpoints are common; a thin wrapper avoids boilerplate at call sites.
/// What: Holds the target URL and a `reqwest::Client`; implements the `Transport` trait.
/// Test: `tests/integration.rs` mocks an HTTP server and round-trips requests.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
}

impl HttpTransport {
    /// Construct a transport pointing at the given URL.
    ///
    /// Why: separated from `send` so the underlying `reqwest::Client` can be
    /// reused across requests (connection pooling).
    /// What: builds a default `reqwest::Client`; callers needing custom auth
    /// headers or TLS config should extend this constructor.
    /// Test: `tests/integration.rs::http_transport_roundtrip`.
    pub fn new(url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
        }
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn send(&self, request: Value) -> Result<Value> {
        let notif = is_notification(&request);
        let resp = self
            .client
            .post(&self.url)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("POST {} failed", self.url))?;

        let status = resp.status();
        let body = resp.text().await.context("reading HTTP response body")?;

        if notif {
            // Fire-and-forget; ignore body even if present.
            return Ok(Value::Null);
        }

        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {body}");
        }

        if body.trim().is_empty() {
            return Ok(Value::Null);
        }

        let val: Value = serde_json::from_str(&body)
            .with_context(|| format!("parsing JSON response: {body}"))?;
        Ok(val)
    }
}
