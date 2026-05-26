//! Cross-cutting diagnostics counters.
//!
//! Why: every subsystem owns local counters but operators want a one-stop
//! view; this module collects a `DiagnosticsReport` summarising all of
//! them in one struct.
//! What: a small struct + a `summarise` free function that walks the
//! supplied subsystems and produces a report.
//! Test: `test_summarise_counts_compactions`.

use crate::wolfram::{WolframInventory, WolframRegistry};

/// Diagnostic snapshot across the pipeline.
///
/// Why: a single struct lets the operator UI render one row per metric
/// rather than chase each subsystem individually.
/// What: counts entry count and compactions for the wolfram registry.
/// Test: tests below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticsReport {
    pub wolfram_entries: usize,
    pub wolfram_compactions: usize,
}

/// Take a snapshot of the supplied registry.
pub fn summarise_diagnostics(registry: &WolframRegistry) -> DiagnosticsReport {
    let inv = WolframInventory::over(registry);
    DiagnosticsReport {
        wolfram_entries: inv.count(),
        wolfram_compactions: inv.compaction_count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarise_counts_compactions() {
        let mut r = WolframRegistry::new();
        r.insert("k", 1.0);
        let report = summarise_diagnostics(&r);
        assert_eq!(report.wolfram_entries, 1);
        assert_eq!(report.wolfram_compactions, 0);
    }
}
