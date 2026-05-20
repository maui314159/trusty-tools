//! Object-safe `EmitStrategy` trait + `ModulePathStrategy` default implementation.
//!
//! Why: Decoupling partition/ordering logic from the emitter lets callers swap
//! strategies (locality-based, test-colocation) without touching render code.
//! What: `EmitStrategy` defines two fallible methods (`partition`, `order_within_file`)
//! and one infallible name accessor. `ModulePathStrategy` wraps the existing
//! `assign_file()` + `topological_sort()` logic for full backward compatibility.

use crate::symgraph::emitter::{EmitError, LayoutRules, assign_file, topological_sort};
use crate::symgraph::registry::{SymbolId, SymbolKind, SymbolRegistry};
use std::collections::HashMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// EmitStrategy trait
// ---------------------------------------------------------------------------

/// Object-safe strategy for partitioning symbols into files and ordering them.
pub trait EmitStrategy {
    // INTENT: Assign every symbol in the registry to a target file path.
    fn partition(
        &self,
        registry: &SymbolRegistry,
        rules: &LayoutRules,
    ) -> std::result::Result<HashMap<PathBuf, Vec<SymbolId>>, EmitError>;

    // INTENT: Order symbols within a single file for emission (dependencies first).
    fn order_within_file(
        &self,
        ids: &[SymbolId],
        registry: &SymbolRegistry,
    ) -> std::result::Result<Vec<SymbolId>, EmitError>;

    // INTENT: Return a human-readable name for logging and debugging.
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// ModulePathStrategy — backward-compatible default
// ---------------------------------------------------------------------------

/// Default strategy that mirrors the pre-refactor `emit()` behaviour:
/// symbols are placed via `assign_file()` (respecting `assigned_file` overrides)
/// and ordered via `topological_sort()` after filtering out `Import`-kind symbols.
pub struct ModulePathStrategy {
    /// Source root directory (e.g. `"src"`). Stored so `partition()` can fall
    /// back to it when `LayoutRules` is not needed beyond `src_root`.
    pub src_root: String,
}

impl Default for ModulePathStrategy {
    // INTENT: Provide a sensible default matching LayoutRules::default().
    fn default() -> Self {
        Self {
            src_root: "src".to_string(),
        }
    }
}

impl EmitStrategy for ModulePathStrategy {
    // INTENT: Group symbols by file using assigned_file overrides or assign_file().
    fn partition(
        &self,
        registry: &SymbolRegistry,
        rules: &LayoutRules,
    ) -> std::result::Result<HashMap<PathBuf, Vec<SymbolId>>, EmitError> {
        let mut file_symbols: HashMap<PathBuf, Vec<SymbolId>> = HashMap::new();
        for (id, entry) in registry.iter() {
            let file = entry
                .assigned_file
                .clone()
                .unwrap_or_else(|| assign_file(id, &rules.src_root));
            file_symbols.entry(file).or_default().push(id.clone());
        }
        Ok(file_symbols)
    }

    // INTENT: Topologically sort non-import symbols so callees precede callers.
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
        topological_sort(&content_ids, registry)
    }

    // INTENT: Identify this strategy in logs.
    fn name(&self) -> &'static str {
        "module-path"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symgraph::registry::{SymbolEntry, SymbolId, SymbolKind, SymbolRegistry};
    use std::path::PathBuf;

    fn make_registry() -> SymbolRegistry {
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        reg.insert(SymbolEntry::new(
            SymbolId::new("utils", "helper"),
            SymbolKind::Function,
            "fn helper() {}".into(),
            "rust",
        ));
        reg.insert(SymbolEntry::new(
            SymbolId::new("api::handlers", "process"),
            SymbolKind::Function,
            "fn process() {}".into(),
            "rust",
        ));
        reg
    }

    #[test]
    fn partition_uses_assign_file() {
        let strategy = ModulePathStrategy::default();
        let reg = make_registry();
        let rules = LayoutRules::default();
        let result = strategy.partition(&reg, &rules).unwrap();

        assert!(result.contains_key(&PathBuf::from("src/utils.rs")));
        assert!(result.contains_key(&PathBuf::from("src/api/handlers.rs")));
    }

    #[test]
    fn partition_respects_assigned_file_override() {
        let strategy = ModulePathStrategy::default();
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        let mut entry = SymbolEntry::new(
            SymbolId::new("utils", "helper"),
            SymbolKind::Function,
            "fn helper() {}".into(),
            "rust",
        );
        entry.assigned_file = Some(PathBuf::from("custom/path.rs"));
        reg.insert(entry);

        let rules = LayoutRules::default();
        let result = strategy.partition(&reg, &rules).unwrap();

        assert!(result.contains_key(&PathBuf::from("custom/path.rs")));
        assert!(!result.contains_key(&PathBuf::from("src/utils.rs")));
    }

    #[test]
    fn order_filters_imports() {
        let strategy = ModulePathStrategy::default();
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        reg.insert(SymbolEntry::new(
            SymbolId::new("utils", "helper"),
            SymbolKind::Function,
            "fn helper() {}".into(),
            "rust",
        ));
        reg.insert(SymbolEntry::new(
            SymbolId::new("utils", "imp"),
            SymbolKind::Import,
            "use std::io;".into(),
            "rust",
        ));

        let ids: Vec<SymbolId> = reg.iter().map(|(id, _)| id.clone()).collect();
        let ordered = strategy.order_within_file(&ids, &reg).unwrap();

        // Import should be filtered out
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].as_str(), "utils::helper");
    }

    #[test]
    fn name_returns_module_path() {
        let strategy = ModulePathStrategy::default();
        assert_eq!(strategy.name(), "module-path");
    }
}
