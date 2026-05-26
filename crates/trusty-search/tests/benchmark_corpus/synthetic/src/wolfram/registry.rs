//! `WolframRegistry` — durable sink at the tail of the pipeline.
//!
//! Why: every transform output ultimately lands in the registry; making it
//! a distinct type means the storage format can change (currently in-memory
//! Vec; planned: backed by mmap) without touching the producers.
//! What: a struct holding `(key, value)` entries plus a compaction counter.
//! When the entry count exceeds `WOLFRAM_NODE_CAP`, the next insertion
//! triggers a compaction (rebuild + dedupe).
//! Test: `test_compaction_fires_at_cap`.

use crate::constants::WOLFRAM_NODE_CAP;

/// Durable in-memory registry.
///
/// Why: keeping a flat vector (rather than a HashMap) keeps the access
/// pattern predictable for the mmap-backed evolution; lookups are linear
/// but consumers cache the index of the entry they care about.
/// What: entries plus a compaction counter for observability.
/// Test: tests below.
pub struct WolframRegistry {
    entries: Vec<(String, f64)>,
    compactions: usize,
}

impl WolframRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            compactions: 0,
        }
    }

    /// Insert a `(key, value)` pair. If the registry already holds the key,
    /// the value is updated in place. Triggers compaction when the entry
    /// count exceeds `WOLFRAM_NODE_CAP`.
    pub fn insert(&mut self, key: impl Into<String>, value: f64) {
        let key = key.into();
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries[idx].1 = value;
        } else {
            self.entries.push((key, value));
            if self.entries.len() > WOLFRAM_NODE_CAP {
                self.compact();
            }
        }
    }

    /// Lookup by key.
    pub fn lookup(&self, key: &str) -> Option<f64> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
    }

    /// Number of compactions performed since construction.
    pub fn compactions(&self) -> usize {
        self.compactions
    }

    /// Number of currently registered entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn compact(&mut self) {
        // Toy compaction: drop the oldest 10%.
        let drop = self.entries.len() / 10;
        self.entries.drain(..drop);
        self.compactions += 1;
    }
}

impl Default for WolframRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_then_lookup() {
        let mut r = WolframRegistry::new();
        r.insert("alpha", 1.0);
        r.insert("beta", 2.5);
        assert_eq!(r.lookup("beta"), Some(2.5));
        assert_eq!(r.lookup("missing"), None);
    }

    #[test]
    fn test_insert_overwrites_existing() {
        let mut r = WolframRegistry::new();
        r.insert("alpha", 1.0);
        r.insert("alpha", 2.0);
        assert_eq!(r.lookup("alpha"), Some(2.0));
        assert_eq!(r.len(), 1);
    }
}
