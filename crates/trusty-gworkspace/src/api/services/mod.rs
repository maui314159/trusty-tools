//! Per-domain Google Workspace service modules.
//!
//! Why: Each module mirrors a Google product surface (Gmail, Drive, …) so
//! tool definitions and implementations stay co-located.
//! What: Every service function has signature
//! `async fn(&BaseClient, serde_json::Value) -> anyhow::Result<Value>` so
//! the MCP dispatcher in `crate::server` can route uniformly.
//! Test: Module-level smoke tests verify argument extraction; live API
//! tests are out-of-scope.

pub mod accounts;
pub mod calendar;
pub mod docs;
pub mod drive;
pub mod gmail;
pub mod sheets;
pub mod slides;
pub mod tasks;

use serde_json::Value;

/// Extract the optional `account` profile name from MCP arguments.
///
/// Why: Every tool accepts an optional `account` field; centralising the
/// extraction avoids subtle off-by-one bugs.
/// What: Returns `Some(name)` only when the field is a non-empty string.
/// Test: Implicitly covered by every service function.
pub(crate) fn account_of(args: &Value) -> Option<&str> {
    args.get("account")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Extract a required string field, returning an error if missing/empty.
pub(crate) fn require_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: {key}"))
}

/// Extract an optional string field.
pub(crate) fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}
