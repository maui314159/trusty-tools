//! Integration tests for the optional axum server.
//!
//! Why: Confirms the HTTP surface compiles and `/health` answers OK with
//! the registry's symbol count.
//! What: Builds a router over an empty registry, calls `/health` via tower's
//! `Service` directly (no network bind needed), asserts 200 + JSON body.
//! Test: `cargo test -p trusty-symgraph --features server --test server_tests`.

#![cfg(feature = "server")]

use trusty_symgraph::SymbolRegistry;
use trusty_symgraph::server::{AppState, router};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn health_endpoint() {
    let reg = SymbolRegistry::new(std::path::PathBuf::from("/tmp"));
    let state = AppState::new(reg);
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("\"status\":\"ok\""));
    assert!(body.contains("\"symbols\":0"));
}
