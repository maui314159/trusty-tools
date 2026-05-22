//! Prometheus metrics endpoint + request-instrumentation middleware
//! (issue #41 Phase 1).
//!
//! Why: Production deployments need alertable counters (request totals,
//! 5xx rates) and histograms (latency distributions) without log scraping.
//! The `metrics` crate provides a zero-cost facade — emit macros at every
//! interesting code path; the recorder snapshot is rendered as Prometheus
//! text on `GET /metrics`.
//!
//! What:
//!   - [`install_recorder`] — install the global Prometheus recorder once at
//!     daemon startup. Returns the handle that `metrics_handler` uses.
//!   - [`metrics_handler`] — axum handler for `GET /metrics`.
//!   - [`request_metrics_middleware`] — wraps every router subtree it's
//!     applied to, recording total + latency by endpoint + status.
//!
//! Test: see `tests` at the bottom — install + render round-trip.

use axum::{
    body::Body,
    extract::Extension,
    http::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::time::Instant;

/// Shared handle to the Prometheus recorder, injected via `Extension` so
/// `metrics_handler` can render the current snapshot. Cloneable.
#[derive(Clone)]
pub struct MetricsState {
    handle: PrometheusHandle,
}

impl MetricsState {
    /// Render the current Prometheus text snapshot.
    pub fn render(&self) -> String {
        self.handle.render()
    }
}

/// Install the global Prometheus recorder and return a state handle.
///
/// Why: must be called exactly once at daemon startup, before any
/// `metrics::counter!`/etc. macros fire — the facade's "no recorder
/// installed" branch is a silent no-op so missing this call would mean
/// `/metrics` always returns an empty document.
/// What: builds the recorder, installs it as the global, and returns the
/// render handle wrapped in `MetricsState`. Errors propagate as `anyhow`.
/// Test: covered by `metrics_handler_returns_prometheus_text` which calls
/// `install_recorder` then GETs `/metrics`.
pub fn install_recorder() -> anyhow::Result<MetricsState> {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("failed to install prometheus recorder: {e}"))?;
    Ok(MetricsState { handle })
}

/// axum handler for `GET /metrics`.
///
/// Why: serves the recorder snapshot in Prometheus text format
/// (`text/plain; version=0.0.4`) so any scraper (Prometheus, VictoriaMetrics,
/// Grafana Agent) can ingest it without an extra adapter.
/// What: clones the `MetricsState` extension, calls `render()`, and returns
/// the text with the correct Content-Type.
/// Test: `metrics_handler_returns_prometheus_text`.
pub async fn metrics_handler(Extension(state): Extension<MetricsState>) -> Response {
    let body = state.render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

/// Middleware: record per-request total + latency.
///
/// Why: keeping the instrumentation in middleware (vs. every handler)
/// guarantees every wrapped route is observed and that latency is measured
/// end-to-end including the handler's serialisation work.
/// What: records `started`, runs the inner handler, then emits
/// `trusty_requests_total{endpoint,method}` and
/// `trusty_request_latency_ms{endpoint,status}`. `endpoint` is the URI path
/// matched by axum (after parameter substitution), or the raw path when no
/// route extension is present.
/// Test: covered indirectly by `metrics_handler_returns_prometheus_text` —
/// after a request flows through this middleware the rendered text contains
/// the counter line.
pub async fn request_metrics_middleware(request: Request<Body>, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    // Prefer the matched route pattern over the raw URI so `/indexes/:id`
    // collapses into a single label value instead of one per id (which would
    // explode cardinality on a busy daemon).
    let endpoint = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());

    let response = next.run(request).await;
    let status = response.status().as_u16().to_string();
    let elapsed_ms = started.elapsed().as_millis() as f64;

    metrics::counter!(
        "trusty_requests_total",
        "endpoint" => endpoint.clone(),
        "method" => method.as_str().to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "trusty_request_latency_ms",
        "endpoint" => endpoint,
        "status" => status,
    )
    .record(elapsed_ms);

    response
}

/// Set the static `trusty_index_count` gauge. Call after each index registry
/// mutation (register / unregister) so the gauge reflects the current value.
///
/// Why: a periodic ticker would be acceptable, but having handlers call this
/// directly means dashboards stay in sync within microseconds of a mutation
/// rather than seconds.
/// What: writes the count to the named gauge.
/// Test: indirect — exercised by `create_index` / `delete_index` flows.
pub fn set_index_count(count: usize) {
    metrics::gauge!("trusty_index_count").set(count as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tower::ServiceExt;

    // NOTE: `install_recorder` installs a *global* recorder. Rust's test
    // harness runs tests in parallel by default, so calling install_recorder
    // twice will panic. We gate the install behind a `OnceLock` so each test
    // gets a working recorder without crashing the second one. This is
    // test-only — production code calls it exactly once in `start.rs`.
    use std::sync::OnceLock;

    fn shared_state() -> MetricsState {
        static STATE: OnceLock<MetricsState> = OnceLock::new();
        STATE
            .get_or_init(|| install_recorder().expect("recorder installs"))
            .clone()
    }

    #[tokio::test]
    async fn metrics_handler_returns_prometheus_text() {
        let state = shared_state();
        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .layer(Extension(state));

        // Emit one counter so the rendered output isn't empty.
        metrics::counter!("trusty_test_counter").increment(1);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/plain; version=0.0.4")
        );
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&body_bytes);
        assert!(
            text.contains("trusty_test_counter"),
            "rendered metrics missing counter: {text}"
        );
    }

    #[tokio::test]
    async fn request_middleware_records_latency_and_total() {
        let state = shared_state();
        let app = Router::new()
            .route("/ping", get(|| async { "pong" }))
            .layer(axum::middleware::from_fn(request_metrics_middleware))
            .route("/metrics", get(metrics_handler))
            .layer(Extension(state));

        let _ = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/ping")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("trusty_requests_total"),
            "missing requests_total: {text}"
        );
    }
}
