//! Prompt-facts surface: hot KG predicates exposed as an MCP prompt.
//!
//! Why: Certain KG triples — aliases, project conventions, ambient facts —
//! belong at the *top* of every Claude session so the model doesn't have to
//! discover them via tool calls. MCP hosts (Claude Code) read `prompts/list`
//! + `prompts/get` once at connect time, so a single cached string surfaced
//! as a prompt gives us zero per-message overhead.
//! What: Defines the `HOT_PREDICATES` allow-list, the grouping/formatting
//! logic that turns `(subject, predicate, object)` triples into a Markdown
//! context block, and the helpers used by the MCP layer to fetch and
//! refresh that block.
//! Test: see the `tests` module — covers `is_hot_predicate`, the formatter
//! grouping/sections, and the empty-input shortcut.

use crate::AppState;
use anyhow::Result;

/// Predicates whose currently-active triples are always included in the
/// session-init prompt context.
///
/// Why: Aliases, conventions, and standalone facts are the categories users
/// reach for when they want a model to "just know" something at the start of
/// every conversation. Other predicates (`works_at`, `lives_in`, …) are
/// retrieval-driven and don't belong in the always-on prompt.
/// What: A static slice of predicate strings; order here drives section
/// order in `build_prompt_context`.
/// Test: `is_hot_predicate_matches_listed`.
pub const HOT_PREDICATES: &[&str] = &[
    "is_alias_for",
    "has_convention",
    "is_fact",
    "is_shorthand_for",
];

/// Check whether `p` is one of the hot predicates surfaced via the prompt.
///
/// Why: Callers (the `kg_assert` dispatch, the `add_alias` tool) need to
/// decide whether a write should invalidate the prompt cache. A free
/// function avoids `HashSet` allocation for a four-element constant list.
/// What: Linear scan over `HOT_PREDICATES` — at four entries this is faster
/// than any hashed alternative and keeps the API copy-free.
/// Test: `is_hot_predicate_matches_listed`.
pub fn is_hot_predicate(p: &str) -> bool {
    HOT_PREDICATES.contains(&p)
}

/// Friendly section heading for each hot predicate.
///
/// Why: Predicate identifiers (`is_alias_for`) are machine-friendly but read
/// poorly in a prompt; "Aliases" reads naturally to a model and a human
/// auditing the prompt content.
/// What: Maps each known predicate to its display heading. Unknown
/// predicates fall back to the predicate name itself so an accidentally
/// added hot predicate still renders coherently.
/// Test: indirectly via `build_prompt_context_groups_and_formats`.
fn section_heading(predicate: &str) -> &str {
    match predicate {
        "is_alias_for" => "Aliases",
        "has_convention" => "Conventions",
        "is_fact" => "Facts",
        "is_shorthand_for" => "Shorthands",
        other => other,
    }
}

/// Build the prompt-context Markdown block from a flat list of triples.
///
/// Why: The MCP `prompts/get` handler returns a single text block; keeping
/// the formatter pure (in: `(subject, predicate, object)` tuples; out:
/// `String`) makes the cache rebuild trivial and the unit tests cheap.
/// What: Filters to hot predicates, groups by predicate in `HOT_PREDICATES`
/// order, emits a top-level header followed by a `###` section per
/// non-empty group with `- subject → object` bullets (for aliases /
/// shorthands) or `- object` bullets (for conventions / facts). Returns an
/// empty `String` when no hot triples are present, so the caller can fall
/// back to a "no context stored yet" message without inspecting the
/// internals.
/// Test: `build_prompt_context_groups_and_formats`,
/// `build_prompt_context_empty_when_no_hot_triples`.
pub fn build_prompt_context(triples: &[(String, String, String)]) -> String {
    // Filter and group preserving HOT_PREDICATES ordering.
    let mut sections: Vec<(&str, Vec<&(String, String, String)>)> =
        HOT_PREDICATES.iter().map(|p| (*p, Vec::new())).collect();
    for triple in triples {
        if let Some(slot) = sections.iter_mut().find(|(p, _)| *p == triple.1.as_str()) {
            slot.1.push(triple);
        }
    }

    // Bail early when nothing matched — callers render a placeholder.
    if sections.iter().all(|(_, v)| v.is_empty()) {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("## Project Context (from memory palace)\n");
    for (predicate, items) in sections {
        if items.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str("### ");
        out.push_str(section_heading(predicate));
        out.push('\n');
        for (subject, _predicate, object) in items {
            // Aliases / shorthands read best as "short → full"; conventions
            // and facts are self-contained so we drop the subject (which is
            // typically a synthetic "convention-1" id with no value to the
            // model).
            match predicate {
                "is_alias_for" | "is_shorthand_for" => {
                    out.push_str("- ");
                    out.push_str(subject);
                    out.push_str(" → ");
                    out.push_str(object);
                    out.push('\n');
                }
                _ => {
                    out.push_str("- ");
                    out.push_str(object);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Fetch every currently-active hot-predicate triple across every palace in
/// the registry.
///
/// Why: The prompt cache surfaces context regardless of which palace stored
/// the fact, so a single MCP connection sees aliases / conventions from
/// every project namespace. Reading once into a `Vec<(String, String,
/// String)>` keeps the formatter side-effect-free and lets tests build
/// fixtures without touching SQLite.
/// What: Iterates every palace handle currently registered, calls
/// `list_active` with a generous limit, and filters each batch through
/// `is_hot_predicate`. A palace whose KG fails to read is logged at `warn`
/// and skipped — one bad palace must not blank the prompt context.
/// Test: `gather_hot_triples_skips_non_hot` (integration in `tools::tests`).
pub async fn gather_hot_triples(state: &AppState) -> Result<Vec<(String, String, String)>> {
    // Why: `list_active` requires a finite limit; HOT_PREDICATES facts are
    // small in count by design (aliases / conventions, not free-form
    // memory), so 1024 is generous without risking unbounded reads on a
    // misuse where someone stores thousands of "facts".
    const PER_PALACE_LIMIT: usize = 1024;

    let mut out = Vec::new();
    for palace_id in state.registry.list() {
        let handle = match state.registry.get(&palace_id) {
            Some(h) => h,
            None => continue, // raced with removal; nothing to read
        };
        match handle.kg.list_active(PER_PALACE_LIMIT, 0).await {
            Ok(triples) => {
                for t in triples {
                    if is_hot_predicate(&t.predicate) {
                        out.push((t.subject, t.predicate, t.object));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    palace = %palace_id.as_str(),
                    "skipping palace during prompt-fact gather: {e:#}",
                );
            }
        }
    }
    Ok(out)
}

/// Refresh `AppState.prompt_context_cache` from the live palace registry.
///
/// Why: Every write that touches a hot predicate (`kg_assert`, `add_alias`,
/// `remove_prompt_fact`) must update the cache so the next `prompts/get`
/// returns the fresh content. Centralising the refresh here means the
/// dispatch sites only have to call one function.
/// What: Calls `gather_hot_triples`, formats via `build_prompt_context`,
/// then takes the cache's write lock and replaces the stored string. The
/// write is non-blocking from the caller's perspective: the lock is held
/// only for the assignment, not the gather/format work.
/// Test: `rebuild_prompt_cache_reflects_writes` (in `tools::tests`).
pub async fn rebuild_prompt_cache(state: &AppState) -> Result<()> {
    let triples = gather_hot_triples(state).await?;
    let formatted = build_prompt_context(&triples);
    let cache = state.prompt_context_cache.clone();
    let mut guard = cache.write().map_err(|e| {
        // RwLock poisoning is recoverable here — the worst case is a stale
        // cache, which is strictly better than panicking the MCP loop.
        anyhow::anyhow!("prompt cache lock poisoned: {e}")
    })?;
    *guard = formatted;
    Ok(())
}

/// Build the MCP `prompts/list` response body.
///
/// Why: Hosts call this once per connection to enumerate available prompts;
/// the response shape is fixed by the MCP spec.
/// What: Returns the single `project_context` entry. Kept as a free
/// function so tests don't need to spin up a server.
/// Test: `prompts_list_returns_project_context` (in `lib::tests`).
pub fn prompts_list_response() -> serde_json::Value {
    serde_json::json!({
        "prompts": [
            {
                "name": "project_context",
                "description": "Project aliases, conventions, and facts from the memory palace. Include at session start.",
            }
        ]
    })
}

/// Build the MCP `prompts/get` response body for a given prompt name.
///
/// Why: Splitting the formatting from the dispatch keeps the JSON-RPC
/// envelope construction in `handle_message` and lets us unit-test the
/// payload shape directly.
/// What: For `project_context`, reads the cached string and wraps it in a
/// single-user-message envelope. Falls back to a "no context stored yet"
/// hint when the cache is empty. For any other name, returns a JSON-RPC
/// error shape (caller wraps as a `Response::err`).
/// Test: `prompts_get_returns_cached_context_or_hint`.
pub fn prompts_get_response(state: &AppState, name: &str) -> Result<serde_json::Value> {
    if name != "project_context" {
        anyhow::bail!("unknown prompt: {name}");
    }
    let cache = state.prompt_context_cache.clone();
    let text = {
        let guard = cache
            .read()
            .map_err(|e| anyhow::anyhow!("prompt cache lock poisoned: {e}"))?;
        guard.clone()
    };
    let body = if text.is_empty() {
        "No project context stored yet. Use add_alias or assert_fact to add context.".to_string()
    } else {
        text
    };
    Ok(serde_json::json!({
        "messages": [
            {
                "role": "user",
                "content": { "type": "text", "text": body }
            }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_hot_predicate_matches_listed() {
        for p in HOT_PREDICATES {
            assert!(is_hot_predicate(p), "expected hot: {p}");
        }
        assert!(!is_hot_predicate("works_at"));
        assert!(!is_hot_predicate(""));
    }

    #[test]
    fn build_prompt_context_empty_when_no_hot_triples() {
        let triples: Vec<(String, String, String)> = vec![
            ("alice".into(), "works_at".into(), "Acme".into()),
            ("bob".into(), "lives_in".into(), "Paris".into()),
        ];
        assert_eq!(build_prompt_context(&triples), "");
    }

    #[test]
    fn build_prompt_context_groups_and_formats() {
        let triples: Vec<(String, String, String)> = vec![
            (
                "tga".into(),
                "is_alias_for".into(),
                "trusty-git-analytics".into(),
            ),
            ("tm".into(), "is_alias_for".into(), "trusty-memory".into()),
            (
                "conv-1".into(),
                "has_convention".into(),
                "No unwrap() in library code".into(),
            ),
            ("fact-1".into(), "is_fact".into(), "MSRV is 1.88".into()),
            // Non-hot — must be ignored entirely.
            ("alice".into(), "works_at".into(), "Acme".into()),
        ];
        let out = build_prompt_context(&triples);
        assert!(out.starts_with("## Project Context (from memory palace)"));
        assert!(out.contains("### Aliases"));
        assert!(out.contains("- tga → trusty-git-analytics"));
        assert!(out.contains("- tm → trusty-memory"));
        assert!(out.contains("### Conventions"));
        assert!(out.contains("- No unwrap() in library code"));
        assert!(out.contains("### Facts"));
        assert!(out.contains("- MSRV is 1.88"));
        // Non-hot triple omitted.
        assert!(!out.contains("Acme"));
        // Aliases section must come before Conventions (HOT_PREDICATES order).
        let aliases_idx = out.find("### Aliases").unwrap();
        let conventions_idx = out.find("### Conventions").unwrap();
        let facts_idx = out.find("### Facts").unwrap();
        assert!(aliases_idx < conventions_idx);
        assert!(conventions_idx < facts_idx);
    }
}
