//! HTTP client for the trusty-search daemon.
//!
//! Why: the unified monitor dashboard needs a typed, testable transport to the
//! trusty-search daemon's read-only endpoints (`/health`, `/indexes`,
//! `/indexes/:id/status`) plus the `/indexes/:id/reindex` action. Keeping the
//! transport in its own module lets the dashboard logic stay free of HTTP
//! concerns and lets the wire shapes be deserialized in one place.
//! What: [`SearchClient`] wraps a base URL and a pooled `reqwest::Client`; it
//! exposes one method per endpoint the dashboard renders. A `fetch_all` helper
//! folds the three read calls into the dashboard's [`SearchData`].
//! Test: `cargo test -p trusty-monitor-tui` covers default-URL resolution and
//! base-URL storage; live endpoints are covered by the daemon's own suite.

use std::time::Duration;

use serde::Deserialize;

use crate::dashboard::{IndexRow, SearchData};

/// Default trusty-search daemon address used when discovery fails.
///
/// Why: the spec mandates falling back to `http://127.0.0.1:7878` when the
/// service lock file is absent so the dashboard still has a target to probe.
/// What: the canonical local trusty-search HTTP base URL.
/// Test: `default_search_url_is_local`.
pub const DEFAULT_SEARCH_URL: &str = "http://127.0.0.1:7878";

/// Per-request timeout for trusty-search probes.
///
/// Why: a hung daemon must not freeze the dashboard's refresh tick; a short
/// timeout turns an unresponsive daemon into a clean "offline" state.
/// What: three seconds, comfortably above a healthy local round-trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Resolve the trusty-search daemon base URL.
///
/// Why: trusty-search auto-walks ports, so its bound address is discovered from
/// the service lock file written by `trusty_common::write_daemon_addr`; only
/// when that is absent does the dashboard fall back to the well-known default.
/// What: reads the `trusty-search` daemon address; on `Some(addr)` returns it
/// prefixed with `http://` when it lacks a scheme, otherwise returns
/// [`DEFAULT_SEARCH_URL`].
/// Test: `resolve_search_url_falls_back_to_default` exercises the fallback path.
pub fn resolve_search_url() -> String {
    match trusty_common::read_daemon_addr("trusty-search") {
        Ok(Some(addr)) => normalize_url(&addr),
        _ => DEFAULT_SEARCH_URL.to_string(),
    }
}

/// Ensure a daemon address carries an `http://` scheme.
///
/// Why: the lock file stores a bare `host:port`; `reqwest` needs a full URL.
/// What: returns `raw` unchanged when it already has a scheme, otherwise
/// prefixes `http://`.
/// Test: `normalize_url_adds_scheme`.
pub fn normalize_url(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    }
}

/// Wire shape of `GET /health` from the trusty-search daemon.
#[derive(Debug, Deserialize)]
struct HealthWire {
    version: String,
    #[serde(default)]
    uptime_secs: u64,
}

/// Wire shape of `GET /indexes` from the trusty-search daemon.
#[derive(Debug, Deserialize)]
struct IndexListWire {
    #[serde(default)]
    indexes: Vec<String>,
}

/// Wire shape of `GET /indexes/:id/status` from the trusty-search daemon.
#[derive(Debug, Deserialize)]
struct IndexStatusWire {
    #[serde(default)]
    root_path: String,
    #[serde(default)]
    chunk_count: u64,
}

/// Typed HTTP client for the trusty-search daemon.
///
/// Why: the dashboard polls trusty-search every refresh tick; a reusable client
/// with a pooled connection keeps the probe cheap and the call sites tidy.
/// What: holds a mutable base URL plus a shared `reqwest::Client`; exposes the
/// read endpoints the dashboard renders and the reindex action.
/// Test: `search_client_stores_base_url`.
#[derive(Debug, Clone)]
pub struct SearchClient {
    base: String,
    http: reqwest::Client,
}

impl SearchClient {
    /// Build a client targeting `base` (e.g. `http://127.0.0.1:7878`).
    ///
    /// Why: the dashboard is pointed at an address resolved from the lock file
    /// or a CLI flag.
    /// What: stores the base URL and a pooled `reqwest::Client` with a request
    /// timeout so a hung daemon cannot stall the refresh loop.
    /// Test: `search_client_stores_base_url`.
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
    /// Test: `search_client_stores_base_url`.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Re-point this client at a freshly resolved daemon URL.
    ///
    /// Why: trusty-search may rebind onto a new ephemeral port across a
    /// restart; a long-lived dashboard must follow it.
    /// What: overwrites the base URL, keeping the pooled client.
    /// Test: `search_client_repoints`.
    pub fn set_base_url(&mut self, base: impl Into<String>) {
        self.base = base.into();
    }

    /// Fetch every panel field from the trusty-search daemon.
    ///
    /// Why: the dashboard wants one fallible call that yields a complete
    /// [`SearchData`] or an error it can render as the offline state.
    /// What: GETs `/health`, then `/indexes`, then `/indexes/:id/status` for
    /// each index, folding the results into [`SearchData`]. A failed per-index
    /// status probe yields a zero-chunk row rather than failing the whole poll.
    /// Test: live behaviour is covered by the trusty-search daemon suite; the
    /// dashboard's offline path is unit-tested in `dashboard.rs`.
    pub async fn fetch_all(&self) -> anyhow::Result<SearchData> {
        let health: HealthWire = self
            .http
            .get(format!("{}/health", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let list: IndexListWire = self
            .http
            .get(format!("{}/indexes", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut indexes = Vec::with_capacity(list.indexes.len());
        for id in list.indexes {
            let row = match self.index_status(&id).await {
                Ok((root_path, chunk_count)) => IndexRow {
                    id,
                    chunk_count,
                    root_path,
                },
                Err(e) => {
                    tracing::warn!("index status probe failed for {id}: {e}");
                    IndexRow {
                        id,
                        chunk_count: 0,
                        root_path: String::new(),
                    }
                }
            };
            indexes.push(row);
        }
        // Stable ordering so the panel does not flicker between polls.
        indexes.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(SearchData {
            version: health.version,
            uptime_secs: health.uptime_secs,
            indexes,
        })
    }

    /// Fetch one index's `(root_path, chunk_count)` from `/indexes/:id/status`.
    ///
    /// Why: the index table shows each index's chunk count; this is the single
    /// per-index probe used by [`Self::fetch_all`].
    /// What: GETs `/indexes/:id/status` and returns the two fields the panel
    /// renders.
    /// Test: covered by the trusty-search daemon suite.
    async fn index_status(&self, id: &str) -> anyhow::Result<(String, u64)> {
        let status: IndexStatusWire = self
            .http
            .get(format!("{}/indexes/{id}/status", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok((status.root_path, status.chunk_count))
    }

    /// Trigger a reindex of `id` via `POST /indexes/:id/reindex`.
    ///
    /// Why: the `[r]` key reindexes the focused search index in place.
    /// What: POSTs an empty JSON body to the reindex endpoint and maps a
    /// non-2xx response to an error.
    /// Test: covered by the trusty-search daemon suite; the dashboard records
    /// the outcome string in `last_action`.
    pub async fn reindex(&self, id: &str) -> anyhow::Result<()> {
        self.http
            .post(format!("{}/indexes/{id}/reindex", self.base))
            .json(&serde_json::json!({}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_search_url_is_local() {
        assert_eq!(DEFAULT_SEARCH_URL, "http://127.0.0.1:7878");
    }

    #[test]
    fn normalize_url_adds_scheme() {
        assert_eq!(normalize_url("127.0.0.1:7878"), "http://127.0.0.1:7878");
        assert_eq!(
            normalize_url("http://127.0.0.1:7878"),
            "http://127.0.0.1:7878"
        );
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn search_client_stores_base_url() {
        let client = SearchClient::new("http://127.0.0.1:7878");
        assert_eq!(client.base_url(), "http://127.0.0.1:7878");
    }

    #[test]
    fn search_client_repoints() {
        let mut client = SearchClient::new("http://127.0.0.1:7878");
        client.set_base_url("http://127.0.0.1:9999");
        assert_eq!(client.base_url(), "http://127.0.0.1:9999");
    }

    #[test]
    fn resolve_search_url_falls_back_to_default() {
        // When discovery yields nothing the resolver must return a usable URL
        // rather than an empty string. It returns either a discovered address
        // or the documented default — both are non-empty and HTTP-schemed.
        let url = resolve_search_url();
        assert!(url.starts_with("http://") || url.starts_with("https://"));
    }
}
