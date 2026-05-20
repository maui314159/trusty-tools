//! Thin async HTTP client to the trusty-search daemon.
//!
//! Why: the analyzer is a sidecar — it never reads trusty-search's redb files
//! directly. Instead it pulls chunks over HTTP and runs analysis in-process.
//! Keeping the client tiny (one struct, three GETs) makes failure modes
//! obvious and lets us swap to a different transport later if needed.

use crate::types::CodeChunk;
use anyhow::{Context, Result};
use futures_util::stream::{FuturesOrdered, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Page size used when paging `GET /indexes/:id/chunks`. The server caps each
/// page at 1000 chunks.
const CHUNK_PAGE_LIMIT: usize = 1000;

/// Number of chunk pages to fetch concurrently. Sized to match the connection
/// pool depth so the HTTP/2 multiplexed stream stays saturated without
/// queueing requests on the client side.
const CHUNK_PAGE_CONCURRENCY: usize = 4;

/// Summary of one registered index, as returned by `GET /indexes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSummary {
    pub id: String,
}

/// HTTP/2 client for trusty-search (port 7878).
/// Uses `http2_prior_knowledge` (loopback, no TLS) for low-latency multiplexed
/// chunk paging. Pool depth matches page concurrency (4).
///
/// Cheap to clone — internally a `reqwest::Client` (which is already an `Arc`
/// under the hood).
#[derive(Clone)]
pub struct TrustySearchClient {
    base_url: String,
    http: reqwest::Client,
}

impl TrustySearchClient {
    /// Construct a client pointed at `base_url` (e.g. `http://127.0.0.1:7878`).
    /// Trailing slashes are tolerated.
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut base = base_url.into();
        if base.ends_with('/') {
            base.pop();
        }
        let http = reqwest::ClientBuilder::new()
            .tcp_keepalive(Duration::from_secs(60))
            .pool_max_idle_per_host(CHUNK_PAGE_CONCURRENCY)
            .http2_prior_knowledge() // both processes are on 127.0.0.1, no TLS needed
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");
        Self {
            base_url: base,
            http,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /health` — true if the daemon answers 2xx.
    pub async fn health(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        let resp = self.http.get(&url).send().await.context("GET /health")?;
        Ok(resp.status().is_success())
    }

    /// `GET /indexes` — list every registered index id.
    pub async fn list_indexes(&self) -> Result<Vec<IndexSummary>> {
        #[derive(Deserialize)]
        struct Listing {
            indexes: Vec<String>,
        }
        let url = format!("{}/indexes", self.base_url);
        let body: Listing = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("non-2xx from {url}"))?
            .json()
            .await
            .with_context(|| format!("decode {url}"))?;
        Ok(body
            .indexes
            .into_iter()
            .map(|id| IndexSummary { id })
            .collect())
    }

    /// `GET /indexes/:id/chunks` — bulk export of every chunk for `index_id`.
    /// Trusty-search must expose this endpoint (added as part of issue #40).
    ///
    /// Pipelines page fetches concurrently (window of `CHUNK_PAGE_CONCURRENCY`)
    /// using `?offset=&limit=` (server caps each page at 1000). Pages are
    /// returned in order to preserve a stable corpus iteration order.
    pub async fn get_chunks(&self, index_id: &str) -> Result<Vec<CodeChunk>> {
        let base = format!("{}/indexes/{}/chunks", self.base_url, index_id);

        let mut all_chunks: Vec<CodeChunk> = Vec::new();
        let mut next_offset: usize = 0;
        let mut exhausted = false;

        // Fetch in concurrent windows: launch up to CHUNK_PAGE_CONCURRENCY page
        // requests in parallel, drain them in order, repeat until any page
        // comes back short (signaling we've hit the end of the corpus).
        while !exhausted {
            let mut window: FuturesOrdered<_> = (0..CHUNK_PAGE_CONCURRENCY)
                .map(|i| {
                    let offset = next_offset + i * CHUNK_PAGE_LIMIT;
                    fetch_chunk_page(&self.http, &base, offset, CHUNK_PAGE_LIMIT)
                })
                .collect();

            let mut window_consumed: usize = 0;
            while let Some(page) = window.next().await {
                let page = page?;
                let received = page.len();
                all_chunks.extend(page);
                window_consumed += 1;
                // Short page (or empty page) means the server has no more
                // chunks beyond this offset — stop scheduling more windows.
                if received < CHUNK_PAGE_LIMIT {
                    exhausted = true;
                    // Drain any in-flight pages already issued (they're
                    // ordered, so the rest will also be short/empty), but
                    // do not start a new window.
                    while let Some(extra) = window.next().await {
                        all_chunks.extend(extra?);
                    }
                    break;
                }
            }

            if !exhausted {
                next_offset += window_consumed * CHUNK_PAGE_LIMIT;
            }
        }

        Ok(all_chunks)
    }
}

/// Fetch a single chunk page. Extracted to a free function so it can be
/// collected into a `FuturesOrdered` without capturing `&self` in the future.
async fn fetch_chunk_page(
    http: &reqwest::Client,
    base_url: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<CodeChunk>> {
    #[derive(Deserialize)]
    struct ChunksBody {
        chunks: Vec<CodeChunk>,
    }
    let url = format!("{base_url}?offset={offset}&limit={limit}");
    let body: ChunksBody = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?
        .json()
        .await
        .with_context(|| format!("decode {url}"))?;
    Ok(body.chunks)
}
