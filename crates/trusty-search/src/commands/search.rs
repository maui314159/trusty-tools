//! Handler for `trusty-search search`.
//!
//! Why: `search` is the project-scoped sibling of `query` — when the working
//! directory is inside a registered index, callers prefer `search` because it
//! auto-resolves the target index from CWD and skips the cross-repo fan-out.
//! Historically the handler was a placeholder that printed
//! "Daemon connection not yet implemented" (issue #3). Issue #65 wires it to
//! the same daemon path used by `query` so the two commands share the
//! `/indexes/:id/search` POST and a single render routine.
//! What: resolve the target index (explicit `--index` flag wins, otherwise the
//! CWD-detected id from `resolve_index`), then delegate to
//! `commands::query::handle_query` which performs the daemon round-trip and
//! prints the result list. `top_k` is forwarded verbatim; `full`/`intent`/
//! `no_kg`/`offset`/`budget` are accepted from the CLI for forward
//! compatibility but the unsupported subset is silently ignored — wiring them
//! is a follow-up once the daemon exposes the matching knobs.
//! Test: `search_delegates_to_query_with_explicit_index` covers the index-id
//! plumbing without touching the network.

use super::index_resolve::resolve_index;
use anyhow::Result;

/// Why: see module docs. Keeping the same arity (and `#[allow]`) lets the
/// `main.rs` call site stay unchanged after the issue #65 wiring.
/// What: resolves the target index id (explicit > CWD-detected), wraps it in
/// the `Option<String>` shape `handle_query` expects, and delegates to
/// `handle_query` so the two subcommands share one code path. We pass
/// `full = false` (compact 7-line snippets), `global_json = false` (text
/// rendering), and `indexes = "*"` because the explicit id is already
/// supplied so the multi-index resolution branch in `handle_query` is
/// unreachable.
/// Test: see module docs.
#[allow(clippy::too_many_arguments)]
pub async fn handle_search(
    explicit_index: &Option<String>,
    query: String,
    top_k: usize,
    full: bool,
) -> Result<()> {
    let (index_id, _warned) = resolve_index(explicit_index);
    // Forward to the shared `query` implementation. `handle_query` itself
    // prints the index-header line, so we don't repeat it here.
    super::query::handle_query(
        &Some(index_id),
        false, // global_json: text output, matches `query` default
        query,
        "*".to_string(),
        top_k,
        full,
    )
    .await
}
