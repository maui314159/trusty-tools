//! Thin wrapper — the CTO DB service implementation lives in `tc-services` (#484).
//!
//! Why: Issue #484 Phase 1 consolidated the CTO DB *service adapter* (schema
//! emission + dispatch over `trusty-cto-db`) into the shared
//! `tc_services::cto_db` module so open-mpm, trusty-izzie, and the Python CTO
//! bot stop re-deriving it. This module is the open-mpm-specific
//! Anti-Corruption Layer: it adapts the host-agnostic `CtoDbService` to
//! open-mpm's `ToolExecutor` trait. `tc-services` cannot depend on that
//! trait (it lives in `trusty-common`, which open-mpm depends on), so the
//! adaptation has to live here.
//! What: `CtoDbToolExecutor` newtype wraps a `tc_services::cto_db::CtoDbService`
//! and implements `ToolExecutor` by delegating `name`/`schema`/`execute` to
//! it, translating `CtoDbOutcome` into `ToolResult`. `cto_db_tools()` keeps
//! the same signature so the `ctrl::mod.rs` persona-registry wiring is
//! untouched.
//! SECURITY: This wrapper is sensitive (HR/budget data). Only the
//! `cto-assistant` persona should ever have these tools registered. The gate
//! lives in `ctrl::mod.rs`; this module does NOT self-restrict.
//! Test: `cto_db_tools_lists_four`, `execute_outcome_maps_to_tool_result`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tc_services::cto_db::{CtoDbOutcome, CtoDbService};

use crate::tools::traits::{ToolExecutor, ToolResult};

/// open-mpm `ToolExecutor` adapter over a `tc_services::cto_db::CtoDbService`.
///
/// Why: open-mpm dispatches by tool name through `dyn ToolExecutor`. The
/// shared `CtoDbService` returns a host-agnostic `CtoDbOutcome`; this newtype
/// is the seam that translates it into open-mpm's `ToolResult`.
/// What: Holds one `CtoDbService`. `execute()` calls
/// `CtoDbService::execute` and maps `CtoDbOutcome::Ok` → `ToolResult::ok`,
/// `CtoDbOutcome::Err` → recoverable `ToolResult::err` so a missing DB or a
/// schema-drift error doesn't tear down the LLM loop.
/// Test: See module-level tests below.
pub struct CtoDbToolExecutor {
    service: CtoDbService,
}

#[async_trait]
impl ToolExecutor for CtoDbToolExecutor {
    fn name(&self) -> &str {
        self.service.name()
    }

    fn schema(&self) -> Value {
        self.service.schema()
    }

    async fn execute(&self, args: Value) -> ToolResult {
        match self.service.execute(args).await {
            CtoDbOutcome::Ok(s) => ToolResult::ok(s),
            CtoDbOutcome::Err(msg) => ToolResult::err(msg),
        }
    }
}

/// Build the full list of CTO DB tool executors (one per query function).
///
/// Why: Centralised constructor that the persona-registry wiring in
/// `ctrl::mod.rs` calls when building the cto-assistant tool surface. Kept at
/// the same signature as before the #484 migration so callers are untouched.
/// What: Delegates to `tc_services::cto_db::cto_db_services()` and wraps each
/// `CtoDbService` in a `CtoDbToolExecutor` boxed as `Arc<dyn ToolExecutor>`.
/// Test: `cto_db_tools_lists_four`.
pub fn cto_db_tools() -> Vec<Arc<dyn ToolExecutor>> {
    tc_services::cto_db::cto_db_services()
        .into_iter()
        .map(|service| Arc::new(CtoDbToolExecutor { service }) as Arc<dyn ToolExecutor>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wrapper must surface one executor per published CTO DB tool.
    ///
    /// Why: Verifies the `CtoDbService` → `CtoDbToolExecutor` adaptation
    /// preserves the four-tool surface after the #484 migration.
    /// What: Asserts `cto_db_tools()` returns four executors whose names match
    /// `tc_services::cto_db::CTO_DB_TOOL_NAMES`.
    /// Test: `cargo test -p open-mpm cto_db_tools_lists_four`.
    #[test]
    fn cto_db_tools_lists_four() {
        let tools = cto_db_tools();
        assert_eq!(tools.len(), 4, "expected one executor per CTO DB tool");
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        for expected in tc_services::cto_db::CTO_DB_TOOL_NAMES {
            assert!(
                names.contains(expected),
                "missing tool {expected} in {names:?}"
            );
        }
    }

    /// A `CtoDbOutcome::Err` from the service must become a recoverable
    /// `ToolResult::Error` — never fatal, never a panic.
    ///
    /// Why: The harness must keep running when `cto.db` is unavailable; the
    /// adapter is the layer that guarantees the error stays recoverable.
    /// What: Points `CTO_DB_PATH` at a non-existent file, runs a tool, and
    /// asserts the `ToolResult` is a non-fatal error. Restores the env var.
    /// Test: `cargo test -p open-mpm execute_outcome_maps_to_tool_result`.
    #[tokio::test]
    async fn execute_outcome_maps_to_tool_result() {
        let prev = std::env::var(trusty_cto_db::ENV_CTO_DB_PATH).ok();
        // SAFETY: single-threaded test scope; env var restored below.
        unsafe {
            std::env::set_var(
                trusty_cto_db::ENV_CTO_DB_PATH,
                "/tmp/definitely-not-a-real-cto-db-path-484-wrapper.sqlite",
            );
        }

        let tools = cto_db_tools();
        let tool = tools
            .iter()
            .find(|t| t.name() == "query_headcount")
            .expect("query_headcount executor present");
        let result = tool.execute(json!({})).await;
        assert!(result.is_error(), "missing DB must yield an error result");
        assert!(!result.is_fatal(), "DB errors must be recoverable");

        // SAFETY: same single-threaded test scope.
        unsafe {
            match prev {
                Some(v) => std::env::set_var(trusty_cto_db::ENV_CTO_DB_PATH, v),
                None => std::env::remove_var(trusty_cto_db::ENV_CTO_DB_PATH),
            }
        }
    }
}
