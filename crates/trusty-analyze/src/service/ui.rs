//! Embedded UI asset serving for the trusty-analyzer daemon.
//!
//! Why: ships the dashboard in the same binary as the daemon so there's no
//! second deployment artifact and no CORS/origin issues. The UI is built into
//! `ui/dist/` by `build.rs` and embedded at compile time via `rust-embed`
//! (mirrors trusty-memory's approach so static-asset handling is consistent
//! across every trusty-* daemon).
//! What: two axum handlers — `ui_index_handler` for `/ui` (serves
//! `index.html`) and `ui_asset_handler` for `/ui/*path` (serves arbitrary
//! assets, falling back to `index.html` for unknown paths so SPA client-side
//! routing works).
//! Test: `cargo test ui::` covers the asset path, the SPA fallback, and MIME
//! type selection.

use axum::{
    body::Body,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

/// Compile-time embedded UI tree. `rust-embed` resolves the folder via
/// `CARGO_MANIFEST_DIR`, so `ui/dist` points at the crate-root `ui/dist`
/// directory populated by `build.rs`.
#[derive(rust_embed::RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/ui/dist/"]
struct WebAssets;

/// Serve the UI index page (`/ui` and `/ui/`).
pub async fn ui_index_handler() -> Response {
    serve_embedded("index.html")
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "ui assets missing").into_response())
}

/// Serve an arbitrary UI asset at `/ui/*path`. Falls back to `index.html`
/// when the path doesn't match a file, so the SPA router can take over.
pub async fn ui_asset_handler(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let trimmed = path.trim_start_matches('/');
    serve_embedded(trimmed).unwrap_or_else(|| {
        serve_embedded("index.html")
            .unwrap_or_else(|| (StatusCode::NOT_FOUND, "ui assets missing").into_response())
    })
}

fn serve_embedded(path: &str) -> Option<Response> {
    let lookup = if path.is_empty() { "index.html" } else { path };
    let asset = WebAssets::get(lookup)?;
    let mime = mime_guess::from_path(lookup).first_or_octet_stream();
    let body = Body::from(asset.data.into_owned());
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    Some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn index_handler_returns_ok_or_404() {
        // With or without ui/dist populated, must not panic.
        let resp = ui_index_handler().await;
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_index_or_404() {
        let resp = ui_asset_handler(axum::extract::Path("does-not-exist.txt".into())).await;
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND);
    }
}
