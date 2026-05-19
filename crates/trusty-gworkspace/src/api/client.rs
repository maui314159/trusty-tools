//! BaseClient: authenticated HTTP wrapper over Google APIs.
//!
//! Why: Every service module needs the same auth + 401-retry pattern.
//! Centralising avoids drift and lets us add tracing/rate-limiting in
//! one place.
//! What: Wraps `reqwest::Client`, resolves access tokens through
//! `TokenStorage`, refreshes via `OAuthManager` on 401 (once).
//! Test: Logic exercised indirectly via service-level integration tests.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tracing::{debug, warn};

use crate::api::auth::{OAuthManager, StoredToken, TokenStorage};

/// Authenticated Google API client.
///
/// Why: One handle per process; `Arc<TokenStorage>` because tools may run
/// concurrently and we don't want to re-read the file each call.
/// What: Holds an `Option<OAuthManager>` — `None` when env credentials are
/// missing (read-only token mode: requests still work until the token
/// expires).
/// Test: Smoke test asserts construction succeeds with no env vars.
pub struct BaseClient {
    http: reqwest::Client,
    pub(crate) storage: Arc<TokenStorage>,
    oauth: Option<OAuthManager>,
}

impl BaseClient {
    /// Construct with default token storage and optional refresh manager.
    pub fn new() -> Result<Self> {
        let storage = Arc::new(TokenStorage::new());
        let oauth = OAuthManager::from_env()?;
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("trusty-gworkspace/0.1")
                .build()
                .context("build reqwest client")?,
            storage,
            oauth,
        })
    }

    /// Expose storage for tools that list profiles.
    pub fn storage(&self) -> &TokenStorage {
        &self.storage
    }

    /// Resolve a stored token entry, preferring (in order):
    /// 1. The explicit `account` parameter
    /// 2. `GWORKSPACE_ACCOUNT` environment variable
    /// 3. The default profile (`is_default=true` or single entry)
    fn resolve_stored(&self, account: Option<&str>) -> Result<(String, StoredToken)> {
        if let Some(name) = account {
            if let Some(t) = self.storage.get_profile(name)? {
                return Ok((name.to_string(), t));
            }
            return Err(anyhow!("no stored token for account '{name}'"));
        }
        if let Ok(env_name) = std::env::var("GWORKSPACE_ACCOUNT")
            && let Some(t) = self.storage.get_profile(&env_name)?
        {
            return Ok((env_name, t));
        }
        let t = self
            .storage
            .get_default()?
            .ok_or_else(|| anyhow!("no default Google Workspace profile found — run setup"))?;
        let name = t.metadata.service_name.clone();
        Ok((name, t))
    }

    /// Return an access token, refreshing if expired and possible.
    pub async fn get_access_token(&self, account: Option<&str>) -> Result<String> {
        let (profile, stored) = self.resolve_stored(account)?;
        if !stored.token.is_expired() {
            return Ok(stored.token.access_token);
        }
        if let Some(oauth) = &self.oauth {
            debug!(profile = %profile, "refreshing expired token");
            let new = oauth.refresh(&self.storage, &profile).await?;
            return Ok(new.access_token);
        }
        warn!(
            profile = %profile,
            "token expired and refresh disabled (no GOOGLE_OAUTH_CLIENT_ID/SECRET); returning stale token"
        );
        Ok(stored.token.access_token)
    }

    /// Returns true for status codes that represent operational, not
    /// programmer, errors. We surface these as JSON `{"error": ...}`
    /// payloads rather than failing the MCP call hard.
    pub fn is_operational_error(status: reqwest::StatusCode) -> bool {
        matches!(
            status,
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
        )
    }

    async fn send_with_retry(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
        account: Option<&str>,
    ) -> Result<reqwest::Response> {
        let token = self.get_access_token(account).await?;
        let mut req = self.http.request(method.clone(), url).bearer_auth(&token);
        if let Some(b) = body {
            req = req.json(b);
        }
        debug!(method = %method, url = %url, "google api request");
        let resp = req
            .send()
            .await
            .with_context(|| format!("{method} {url}"))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            warn!(url = %url, "401 received — forcing refresh and retrying once");
            if let (Some(oauth), Ok((profile, _))) = (&self.oauth, self.resolve_stored(account)) {
                let new = oauth.refresh(&self.storage, &profile).await?;
                let mut retry = self
                    .http
                    .request(method.clone(), url)
                    .bearer_auth(new.access_token);
                if let Some(b) = body {
                    retry = retry.json(b);
                }
                return retry.send().await.context("retry after refresh");
            }
        }
        Ok(resp)
    }

    async fn json_or_error(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text)
                .with_context(|| format!("parse response JSON: {text}"));
        }
        if Self::is_operational_error(status) {
            warn!(status = %status, body = %text, "operational error from Google API");
            return Ok(serde_json::json!({
                "error": text,
                "status": status.as_u16(),
            }));
        }
        Err(anyhow!("google api error {status}: {text}"))
    }

    pub async fn get(&self, url: &str, account: Option<&str>) -> Result<Value> {
        let resp = self
            .send_with_retry(reqwest::Method::GET, url, None, account)
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn post(&self, url: &str, body: Value, account: Option<&str>) -> Result<Value> {
        let resp = self
            .send_with_retry(reqwest::Method::POST, url, Some(&body), account)
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn patch(&self, url: &str, body: Value, account: Option<&str>) -> Result<Value> {
        let resp = self
            .send_with_retry(reqwest::Method::PATCH, url, Some(&body), account)
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn put(&self, url: &str, body: Value, account: Option<&str>) -> Result<Value> {
        let resp = self
            .send_with_retry(reqwest::Method::PUT, url, Some(&body), account)
            .await?;
        Self::json_or_error(resp).await
    }

    pub async fn delete(&self, url: &str, account: Option<&str>) -> Result<Value> {
        let resp = self
            .send_with_retry(reqwest::Method::DELETE, url, None, account)
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(serde_json::json!({ "deleted": true, "status": status.as_u16() }));
        }
        if Self::is_operational_error(status) {
            return Ok(serde_json::json!({ "error": text, "status": status.as_u16() }));
        }
        Err(anyhow!("google api delete error {status}: {text}"))
    }
}
