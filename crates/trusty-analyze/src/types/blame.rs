//! Git blame metadata for a chunk's line range.
//!
//! Wire-compatible with `trusty_search_core::blame::ChunkBlame`. Field
//! names match exactly. Optional `decay_score` is computed downstream.

use serde::{Deserialize, Serialize};

/// Git blame metadata for a single chunk's line range.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ChunkBlame {
    /// Short commit hash (12 chars) or empty if not tracked.
    #[serde(default)]
    pub commit_hash: String,
    #[serde(default)]
    pub author_email: String,
    /// ISO 8601 date string, e.g. "2026-05-09".
    #[serde(default)]
    pub last_modified: String,
    /// Days between `last_modified` and the time the search daemon ran blame.
    #[serde(default)]
    pub days_since_modified: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blame_round_trips() {
        let b = ChunkBlame {
            commit_hash: "abcdef012345".into(),
            author_email: "a@b.c".into(),
            last_modified: "2026-01-01".into(),
            days_since_modified: 30,
        };
        let s = serde_json::to_string(&b).unwrap();
        let back: ChunkBlame = serde_json::from_str(&s).unwrap();
        assert_eq!(b, back);
    }
}
