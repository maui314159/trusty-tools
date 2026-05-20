//! `TestColocationStrategy` — places test symbols adjacent to their targets.
//!
//! Uses `SymbolEntry::test_covers` to find the production symbol each test
//! covers, and assigns the test to the same file as its target. Within each
//! file, non-test symbols sort before test symbols (secondary stable sort
//! after topological sort).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::symgraph::emitter::{EmitError, LayoutRules, assign_file, topological_sort};
use crate::symgraph::registry::{SymbolId, SymbolKind, SymbolRegistry};
use crate::symgraph::strategy::EmitStrategy;

/// Strategy that co-locates test symbols with their production targets.
pub struct TestColocationStrategy {
    pub src_root: String,
}

impl Default for TestColocationStrategy {
    // INTENT: Provide a sensible default matching LayoutRules::default().
    fn default() -> Self {
        Self {
            src_root: "src".to_string(),
        }
    }
}

// INTENT: Return true if the symbol kind represents a test.
fn is_test_kind(kind: &SymbolKind) -> bool {
    matches!(kind, SymbolKind::Test | SymbolKind::TestSuite)
}

impl EmitStrategy for TestColocationStrategy {
    // INTENT: Assign tests to the same file as their test_covers target.
    fn partition(
        &self,
        registry: &SymbolRegistry,
        rules: &LayoutRules,
    ) -> std::result::Result<HashMap<PathBuf, Vec<SymbolId>>, EmitError> {
        // Pass 1: resolve file for all non-test symbols.
        let mut file_for: HashMap<SymbolId, PathBuf> = HashMap::new();
        for (id, entry) in registry.iter() {
            if is_test_kind(&entry.kind) {
                continue;
            }
            let file = entry
                .assigned_file
                .clone()
                .unwrap_or_else(|| assign_file(id, &rules.src_root));
            file_for.insert(id.clone(), file);
        }

        // Pass 2: resolve file for test symbols.
        let mut file_symbols: HashMap<PathBuf, Vec<SymbolId>> = HashMap::new();
        for (id, entry) in registry.iter() {
            let file = if is_test_kind(&entry.kind) {
                if let Some(ref assigned) = entry.assigned_file {
                    assigned.clone()
                } else if let Some(ref target_id) = entry.test_covers {
                    // Follow the target's resolved file.
                    file_for
                        .get(target_id)
                        .cloned()
                        .unwrap_or_else(|| assign_file(id, &rules.src_root))
                } else {
                    // Test without test_covers: fallback.
                    assign_file(id, &rules.src_root)
                }
            } else {
                file_for.get(id).cloned().unwrap()
            };
            file_symbols.entry(file).or_default().push(id.clone());
        }

        Ok(file_symbols)
    }

    // INTENT: Order symbols with production first, tests after, preserving topological order.
    fn order_within_file(
        &self,
        ids: &[SymbolId],
        registry: &SymbolRegistry,
    ) -> std::result::Result<Vec<SymbolId>, EmitError> {
        let content_ids: Vec<SymbolId> = ids
            .iter()
            .filter(|id| {
                registry
                    .get(id)
                    .map(|e| e.kind != SymbolKind::Import)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        let sorted = topological_sort(&content_ids, registry)?;

        // Stable partition: non-test symbols first, then test symbols.
        let mut production = Vec::new();
        let mut tests = Vec::new();
        for id in sorted {
            let is_test = registry
                .get(&id)
                .map(|e| is_test_kind(&e.kind))
                .unwrap_or(false);
            if is_test {
                tests.push(id);
            } else {
                production.push(id);
            }
        }
        production.extend(tests);
        Ok(production)
    }

    // INTENT: Identify this strategy in logs.
    fn name(&self) -> &'static str {
        "test-colocation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symgraph::registry::{SymbolEntry, SymbolId, SymbolKind, SymbolRegistry};

    fn make_registry_with_test() -> SymbolRegistry {
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        reg.insert(SymbolEntry::new(
            SymbolId::new("utils", "helper"),
            SymbolKind::Function,
            "fn helper() {}".into(),
            "rust",
        ));
        let mut test_entry = SymbolEntry::new(
            SymbolId::new("utils", "test_helper"),
            SymbolKind::Test,
            "#[test] fn test_helper() {}".into(),
            "rust",
        );
        test_entry.test_covers = Some(SymbolId::new("utils", "helper"));
        reg.insert(test_entry);
        reg
    }

    #[test]
    fn test_colocates_with_target() {
        let s = TestColocationStrategy::default();
        let reg = make_registry_with_test();
        let rules = LayoutRules::default();
        let result = s.partition(&reg, &rules).unwrap();

        // Both should be in the same file.
        assert_eq!(result.len(), 1);
        let ids = result.values().next().unwrap();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn test_without_covers_uses_fallback() {
        let s = TestColocationStrategy::default();
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        reg.insert(SymbolEntry::new(
            SymbolId::new("other", "func"),
            SymbolKind::Function,
            "fn func() {}".into(),
            "rust",
        ));
        reg.insert(SymbolEntry::new(
            SymbolId::new("tests", "orphan_test"),
            SymbolKind::Test,
            "#[test] fn orphan_test() {}".into(),
            "rust",
        ));
        let result = s.partition(&reg, &LayoutRules::default()).unwrap();
        // Orphan test falls back to its own module path.
        assert!(result.contains_key(&PathBuf::from("src/tests.rs")));
    }

    #[test]
    fn respects_assigned_file_on_target() {
        let s = TestColocationStrategy::default();
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        let mut prod = SymbolEntry::new(
            SymbolId::new("core", "engine"),
            SymbolKind::Function,
            "fn engine() {}".into(),
            "rust",
        );
        prod.assigned_file = Some(PathBuf::from("custom/engine.rs"));
        reg.insert(prod);

        let mut test_entry = SymbolEntry::new(
            SymbolId::new("core", "test_engine"),
            SymbolKind::Test,
            "#[test] fn test_engine() {}".into(),
            "rust",
        );
        test_entry.test_covers = Some(SymbolId::new("core", "engine"));
        reg.insert(test_entry);

        let result = s.partition(&reg, &LayoutRules::default()).unwrap();
        // Test follows target's assigned_file override.
        assert!(result.contains_key(&PathBuf::from("custom/engine.rs")));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn order_puts_production_before_tests() {
        let s = TestColocationStrategy::default();
        let reg = make_registry_with_test();
        let ids: Vec<SymbolId> = reg.iter().map(|(id, _)| id.clone()).collect();
        let ordered = s.order_within_file(&ids, &reg).unwrap();
        assert_eq!(ordered.len(), 2);
        // Production symbol first.
        assert_eq!(ordered[0].as_str(), "utils::helper");
        assert_eq!(ordered[1].as_str(), "utils::test_helper");
    }

    #[test]
    fn name_returns_test_colocation() {
        assert_eq!(TestColocationStrategy::default().name(), "test-colocation");
    }
}
