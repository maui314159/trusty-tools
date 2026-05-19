//! Account / profile listing.
//!
//! Why: Users with multiple Google accounts need to discover which profile
//! names they can pass as the `account` MCP argument.
//! What: Reads from `TokenStorage` (no network); returns a JSON array of
//! `{name, email, is_default}` objects.
//! Test: Indirect — covered by storage tests.

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::client::BaseClient;

/// Why: Enumerate authenticated Google profiles stored locally so the model can pick one.
/// What: Returns `{accounts: [{name, email, is_default}]}` read from `TokenStorage` — no network.
/// Test: Covered indirectly via `TokenStorage` storage round-trip test.
pub async fn list_accounts(client: &BaseClient, _args: Value) -> Result<Value> {
    let rows = client.storage().list_accounts()?;
    let accounts: Vec<Value> = rows
        .into_iter()
        .map(|(name, email, is_default)| {
            json!({
                "name": name,
                "email": email,
                "is_default": is_default,
            })
        })
        .collect();
    Ok(json!({ "accounts": accounts }))
}
