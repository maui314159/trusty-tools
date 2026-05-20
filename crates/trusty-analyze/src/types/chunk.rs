//! `CodeChunk`: the unit of work passed between trusty-search and trusty-analyzer.
//!
//! Why: the analyzer fetches chunks from the search daemon's
//! `GET /indexes/:id/chunks` endpoint and runs analysis on them. Keeping the
//! struct here (separate from trusty-search-core) means trusty-analyzer can
//! deserialize without depending on trusty-search at all.
//!
//! Compatibility: serde tolerates unknown fields by default, so the search
//! daemon can add structural metadata (chunk_type, calls, inherits_from, …)
//! without breaking analyzer deserialization. We only declare the fields the
//! analyzer needs to do its job.
//!
//! Note: complexity and blame are NOT carried on `CodeChunk`. trusty-analyzer
//! computes those independently via `compute_complexity_for()` and the blame
//! module — trusty-search never populated them in practice, so removing them
//! drops dead carrier fields.

use serde::{Deserialize, Serialize};

/// One chunk of code, anchored to a file + line range.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeChunk {
    /// Collision-safe id: `{path}:{start}:{end}`.
    pub id: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    #[serde(default)]
    pub function_name: Option<String>,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub compact_snippet: Option<String>,
    #[serde(default)]
    pub match_reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_minimal_json_round_trips() {
        // The smallest legal payload: only the required fields.
        let s = r#"{
            "id": "f:1:5",
            "file": "f.rs",
            "start_line": 1,
            "end_line": 5,
            "content": "fn f() {}"
        }"#;
        let c: CodeChunk = serde_json::from_str(s).unwrap();
        assert_eq!(c.id, "f:1:5");
        assert_eq!(c.content, "fn f() {}");
    }

    #[test]
    fn chunk_tolerates_extra_fields_from_search_daemon() {
        // trusty-search emits extra structural metadata (chunk_type, calls,
        // …); the analyzer must accept and ignore them gracefully.
        let s = r#"{
            "id": "f:1:5",
            "file": "f.rs",
            "start_line": 1,
            "end_line": 5,
            "content": "fn f() {}",
            "chunk_type": "Function",
            "calls": ["other"],
            "inherits_from": [],
            "complexity_score": 3,
            "chunk_depth": 0
        }"#;
        let c: CodeChunk = serde_json::from_str(s).unwrap();
        assert_eq!(c.id, "f:1:5");
    }
}
