//! `WolframInventory` — read-only summary view over the registry.
//!
//! Why: external consumers (UI, monitoring) need a stable read-only view
//! that does not let them mutate the registry; the inventory wraps a
//! borrow with summary statistics.
//! What: holds a reference to a registry slice and exposes count + sum.
//! Test: `test_summary_round_trips`.

use crate::wolfram::registry::WolframRegistry;

/// Read-only summary view over a `WolframRegistry`.
///
/// Why: prevents read-paths from accidentally taking a `&mut` and mutating.
/// What: stores a borrow to the registry.
/// Test: tests below.
pub struct WolframInventory<'a> {
    registry: &'a WolframRegistry,
}

impl<'a> WolframInventory<'a> {
    /// Build an inventory view over a registry.
    pub fn over(registry: &'a WolframRegistry) -> Self {
        Self { registry }
    }

    /// Number of entries in the underlying registry.
    pub fn count(&self) -> usize {
        self.registry.len()
    }

    /// Number of compactions seen by the underlying registry.
    pub fn compaction_count(&self) -> usize {
        self.registry.compactions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summary_round_trips() {
        let mut r = WolframRegistry::new();
        r.insert("a", 1.0);
        r.insert("b", 2.0);
        let inv = WolframInventory::over(&r);
        assert_eq!(inv.count(), 2);
        assert_eq!(inv.compaction_count(), 0);
    }
}
