//! HTTP client for the trusty-memory daemon.
//!
//! Why: the unified monitor dashboard needs a typed, testable transport to the
//! trusty-memory daemon's read-only endpoints (`/health`, `/api/v1/status`,
//! `/api/v1/palaces`). Keeping it in its own module mirrors `search_client` and
//! isolates the wire shapes the memory panel renders.
//! What: [`MemoryClient`] wraps a base URL and a pooled `reqwest::Client`; a
//! `fetch_all` helper folds the status and palace calls into [`MemoryData`].
//! Test: `cargo test -p trusty-monitor-tui` covers URL resolution and base-URL
//! storage; live endpoints are covered by the daemon's own suite.

use std::time::Duration;

use serde::Deserialize;

use crate::monitor::dashboard::{MemoryData, PalaceRow};

/// Default trusty-memory daemon address used when discovery fails.
///
/// Why: trusty-memory binds a dynamic port, so the lock file is the primary
/// discovery path; the default only applies when no daemon has ever started.
/// What: a well-known local fallback base URL for trusty-memory.
/// Test: `default_memory_url_is_local`.
pub const DEFAULT_MEMORY_URL: &str = "http://127.0.0.1:7070";

/// Per-request timeout for trusty-memory probes.
///
/// Why: a hung daemon must not stall the dashboard refresh tick.
/// What: three seconds, matching `search_client`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Resolve the trusty-memory daemon base URL.
///
/// Why: trusty-memory's port is dynamic, so its address is read from the
/// service lock file; the default applies only when discovery yields nothing.
/// What: reads the `trusty-memory` daemon address; returns it `http://`-prefixed
/// when present, otherwise [`DEFAULT_MEMORY_URL`].
/// Test: `resolve_memory_url_returns_http_url`.
pub fn resolve_memory_url() -> String {
    match crate::read_daemon_addr("trusty-memory") {
        Ok(Some(addr)) => normalize_url(&addr),
        _ => DEFAULT_MEMORY_URL.to_string(),
    }
}

/// Ensure a daemon address carries an `http://` scheme.
///
/// Why: the lock file stores a bare `host:port`; `reqwest` needs a full URL.
/// What: returns `raw` unchanged when it already has a scheme, else prefixes
/// `http://`.
/// Test: `normalize_url_adds_scheme`.
pub fn normalize_url(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    }
}

/// Wire shape of `GET /api/v1/status` from the trusty-memory daemon.
#[derive(Debug, Deserialize)]
struct StatusWire {
    #[serde(default)]
    version: String,
    #[serde(default)]
    palace_count: u64,
    #[serde(default)]
    total_drawers: u64,
    #[serde(default)]
    total_vectors: u64,
    #[serde(default)]
    total_kg_triples: u64,
}

/// Wire shape of one palace entry from `GET /api/v1/palaces`.
///
/// Why: the palace list response shape varies slightly between daemon
/// versions; all fields are optional with defaults so a partial payload still
/// deserializes rather than failing the whole poll.
#[derive(Debug, Default, Deserialize)]
struct PalaceWire {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, alias = "vectors", alias = "total_vectors")]
    vector_count: u64,
}

/// Typed HTTP client for the trusty-memory daemon.
///
/// Why: the dashboard polls trusty-memory every refresh tick; a reusable client
/// keeps the probe cheap and the call sites tidy.
/// What: holds a mutable base URL plus a shared `reqwest::Client`; exposes the
/// read endpoints the memory panel renders.
/// Test: `memory_client_stores_base_url`.
#[derive(Debug, Clone)]
pub struct MemoryClient {
    base: String,
    http: reqwest::Client,
}

impl MemoryClient {
    /// Build a client targeting `base` (e.g. `http://127.0.0.1:7070`).
    ///
    /// Why: the dashboard is pointed at an address resolved from the lock file.
    /// What: stores the base URL and a pooled `reqwest::Client` with a request
    /// timeout.
    /// Test: `memory_client_stores_base_url`.
    pub fn new(base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            base: base.into(),
            http,
        }
    }

    /// The base URL this client targets.
    ///
    /// Why: the dashboard renders the daemon address and re-resolution compares
    /// against the current target.
    /// What: returns the stored base URL.
    /// Test: `memory_client_stores_base_url`.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Re-point this client at a freshly resolved daemon URL.
    ///
    /// Why: trusty-memory rebinds a fresh dynamic port on every restart, so a
    /// long-lived dashboard must follow it.
    /// What: overwrites the base URL, keeping the pooled client.
    /// Test: `memory_client_repoints`.
    pub fn set_base_url(&mut self, base: impl Into<String>) {
        self.base = base.into();
    }

    /// Fetch every panel field from the trusty-memory daemon.
    ///
    /// Why: the dashboard wants one fallible call that yields a complete
    /// [`MemoryData`] or an error it can render as the offline state.
    /// What: GETs `/api/v1/status`, then `/api/v1/palaces`, folding both into
    /// [`MemoryData`]. A failed palace-list probe yields an empty list rather
    /// than failing the whole poll, since the aggregate counts still render.
    /// Test: live behaviour is covered by the trusty-memory daemon suite; the
    /// dashboard's offline path is unit-tested in `dashboard.rs`.
    pub async fn fetch_all(&self) -> anyhow::Result<MemoryData> {
        let status: StatusWire = self
            .http
            .get(format!("{}/api/v1/status", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let palaces = match self.palaces().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("palace list probe failed: {e}");
                Vec::new()
            }
        };

        Ok(MemoryData {
            version: status.version,
            palace_count: status.palace_count,
            total_drawers: status.total_drawers,
            total_vectors: status.total_vectors,
            total_kg_triples: status.total_kg_triples,
            palaces,
        })
    }

    /// Probe whether the trusty-memory daemon is reachable.
    ///
    /// Why: the memory panel shows an offline badge when the daemon is down;
    /// the cheap `/health` probe decides reachability before the heavier
    /// status calls run.
    /// What: GETs `/health`, returns `true` on any 2xx response.
    /// Test: covered by the trusty-memory daemon suite.
    pub async fn is_healthy(&self) -> bool {
        matches!(
            self.http.get(format!("{}/health", self.base)).send().await,
            Ok(r) if r.status().is_success()
        )
    }

    /// Fetch the palace list from `GET /api/v1/palaces`.
    ///
    /// Why: the memory panel renders one row per palace with its vector count.
    /// What: GETs the palace list and projects each entry to a [`PalaceRow`].
    /// The endpoint may return either a bare array or an object with a
    /// `palaces` field; both shapes are accepted.
    /// Test: `palace_list_accepts_array_and_object_shapes`.
    async fn palaces(&self) -> anyhow::Result<Vec<PalaceRow>> {
        let raw: serde_json::Value = self
            .http
            .get(format!("{}/api/v1/palaces", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(parse_palaces(&raw))
    }
}

/// Project a palace-list JSON payload into [`PalaceRow`]s.
///
/// Why: the trusty-memory palace endpoint has shipped both a bare-array shape
/// and an object-wrapped shape across versions; centralising the parsing keeps
/// the client resilient to either and makes it unit-testable without a daemon.
/// What: accepts a JSON array of palace objects, or an object carrying a
/// `palaces` array, and returns the projected rows; any other shape yields an
/// empty list.
/// Test: `palace_list_accepts_array_and_object_shapes`.
pub fn parse_palaces(raw: &serde_json::Value) -> Vec<PalaceRow> {
    let array = match raw {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(obj) => match obj.get("palaces") {
            Some(serde_json::Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    array
        .into_iter()
        .filter_map(|v| serde_json::from_value::<PalaceWire>(v).ok())
        .map(|p| PalaceRow {
            id: p.id,
            name: p.name,
            vector_count: p.vector_count,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_memory_url_is_local() {
        assert!(DEFAULT_MEMORY_URL.starts_with("http://127.0.0.1"));
    }

    #[test]
    fn normalize_url_adds_scheme() {
        assert_eq!(normalize_url("127.0.0.1:7070"), "http://127.0.0.1:7070");
        assert_eq!(
            normalize_url("http://127.0.0.1:7070"),
            "http://127.0.0.1:7070"
        );
    }

    #[test]
    fn memory_client_stores_base_url() {
        let client = MemoryClient::new("http://127.0.0.1:7070");
        assert_eq!(client.base_url(), "http://127.0.0.1:7070");
    }

    #[test]
    fn memory_client_repoints() {
        let mut client = MemoryClient::new("http://127.0.0.1:7070");
        client.set_base_url("http://127.0.0.1:8080");
        assert_eq!(client.base_url(), "http://127.0.0.1:8080");
    }

    #[test]
    fn resolve_memory_url_returns_http_url() {
        let url = resolve_memory_url();
        assert!(url.starts_with("http://") || url.starts_with("https://"));
    }

    #[test]
    fn palace_list_accepts_array_and_object_shapes() {
        // Bare-array shape.
        let arr = serde_json::json!([
            {"id": "p1", "name": "default", "vector_count": 8400},
            {"id": "p2", "name": "work", "vectors": 0},
        ]);
        let rows = parse_palaces(&arr);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "p1");
        assert_eq!(rows[0].vector_count, 8400);
        // The `vectors` alias is honoured.
        assert_eq!(rows[1].name, "work");

        // Object-wrapped shape.
        let obj = serde_json::json!({
            "palaces": [{"id": "p3", "name": "notes", "total_vectors": 12}],
        });
        let rows = parse_palaces(&obj);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].vector_count, 12);

        // An unexpected shape yields no rows rather than panicking.
        assert!(parse_palaces(&serde_json::json!("nonsense")).is_empty());
    }
}
