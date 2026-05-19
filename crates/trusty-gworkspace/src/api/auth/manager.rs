//! OAuth refresh manager — exchanges a refresh token for a new access token.
//!
//! Why: Access tokens expire ~1 hour. We don't run the interactive flow
//! here (that's the Python CLI's job for now) but we *do* need to refresh
//! before requests go stale.
//! What: POSTs `grant_type=refresh_token` to Google's OAuth token endpoint
//! and updates the on-disk record.
//! Test: Manual — requires real Google credentials. Logic-only branches
//! (env-var presence, JSON shape) are exercised indirectly by `BaseClient`.

use anyhow::{Context, Result, anyhow};
use chrono::{Duration, Utc};
use serde::Deserialize;

use super::models::{OAuthToken, StoredToken};
use super::storage::TokenStorage;
use crate::api::constants::OAUTH_TOKEN_URL;

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// OAuth refresh manager.
///
/// Why: Encapsulates Google's OAuth client credentials so `BaseClient` only
/// needs to call one method to get a fresh token.
/// What: Reads `GOOGLE_OAUTH_CLIENT_ID` / `GOOGLE_OAUTH_CLIENT_SECRET` env
/// vars on construction. `refresh` performs the HTTP exchange.
/// Test: Construction is covered by `from_env_returns_none_when_missing`.
pub struct OAuthManager {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

impl OAuthManager {
    /// Construct from env vars, returning `Ok(None)` when both are absent
    /// (read-only token mode — refresh disabled).
    pub fn from_env() -> Result<Option<Self>> {
        let id = std::env::var("GOOGLE_OAUTH_CLIENT_ID").ok();
        let secret = std::env::var("GOOGLE_OAUTH_CLIENT_SECRET").ok();
        match (id, secret) {
            (Some(client_id), Some(client_secret)) => Ok(Some(Self {
                http: reqwest::Client::new(),
                client_id,
                client_secret,
            })),
            _ => Ok(None),
        }
    }

    /// Refresh the access token for the given profile and persist the result.
    ///
    /// Why: The stored token is near or past expiry; we need a fresh one
    /// before the next API call.
    /// What: POSTs to Google's OAuth endpoint with `grant_type=refresh_token`,
    /// parses the response, updates `expires_at` to `now + expires_in`, and
    /// writes the updated `StoredToken` back to disk.
    /// Test: integration with real Google creds only.
    pub async fn refresh(&self, storage: &TokenStorage, profile: &str) -> Result<OAuthToken> {
        let mut stored: StoredToken = storage
            .get_profile(profile)?
            .ok_or_else(|| anyhow!("no stored token for profile '{profile}'"))?;
        let refresh_token = stored
            .token
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("no refresh_token available for profile '{profile}'"))?;

        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];
        let resp = self
            .http
            .post(OAUTH_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .context("POST oauth2 token endpoint")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("token refresh failed ({status}): {body}"));
        }
        let parsed: GoogleTokenResponse =
            serde_json::from_str(&body).with_context(|| format!("parse token response: {body}"))?;

        let expires_in = parsed.expires_in.unwrap_or(3600);
        let new_token = OAuthToken {
            access_token: parsed.access_token,
            refresh_token: parsed.refresh_token.or(Some(refresh_token)),
            expires_at: Utc::now() + Duration::seconds(expires_in),
            scopes: parsed
                .scope
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or(stored.token.scopes.clone()),
            token_type: parsed.token_type.unwrap_or_else(|| "Bearer".into()),
        };

        stored.token = new_token.clone();
        stored.metadata.last_refreshed = Some(Utc::now());

        let mut all = storage.load()?;
        all.insert(profile.to_string(), stored);
        storage.save(&all)?;

        Ok(new_token)
    }
}
