//! `LocalityStrategy` — groups symbols by call-edge density using SCC clustering.
//!
//! Uses `petgraph::algo::tarjan_scc` on the call graph from
//! `SymbolGraph::build_from_registry()` to co-locate tightly-coupled symbols.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::symgraph::emitter::{EmitError, LayoutRules, assign_file, topological_sort};
use crate::symgraph::graph::SymbolGraph;
use crate::symgraph::registry::{SymbolId, SymbolKind, SymbolRegistry};
use crate::symgraph::strategy::EmitStrategy;

/// Strategy that clusters symbols with high mutual call-edge density into the
/// same file using Tarjan's strongly-connected-components algorithm.
pub struct LocalityStrategy {
    pub src_root: String,
}

impl Default for LocalityStrategy {
    // INTENT: Provide a sensible default matching LayoutRules::default().
    fn default() -> Self {
        Self {
            src_root: "src".to_string(),
        }
    }
}

impl EmitStrategy for LocalityStrategy {
    // INTENT: Partition symbols into files using SCC-based locality clustering.
    fn partition(
        &self,
        registry: &SymbolRegistry,
        rules: &LayoutRules,
    ) -> std::result::Result<HashMap<PathBuf, Vec<SymbolId>>, EmitError> {
        let mut pinned: HashMap<SymbolId, PathBuf> = HashMap::new();
        for (id, entry) in registry.iter() {
            if let Some(ref path) = entry.assigned_file {
                pinned.insert(id.clone(), path.clone());
            }
        }

        let graph = SymbolGraph::build_from_registry(registry);
        let inner = graph.inner();
        let sccs = petgraph::algo::tarjan_scc(inner);
        let bare_to_id = build_bare_name_index(registry);

        let mut clustered: HashMap<SymbolId, PathBuf> = HashMap::new();
        for scc in &sccs {
            if scc.len() < 2 {
                continue;
            }
            let member_ids: Vec<SymbolId> = scc
                .iter()
                .filter_map(|&ni| bare_to_id.get(inner[ni].name.as_str()).cloned())
                .filter(|id| !pinned.contains_key(id))
                .collect();
            if member_ids.is_empty() {
                continue;
            }
            let target = pick_cluster_file(&member_ids, &rules.src_root);
            for id in member_ids {
                clustered.insert(id, target.clone());
            }
        }

        let mut file_symbols: HashMap<PathBuf, Vec<SymbolId>> = HashMap::new();
        for (id, _entry) in registry.iter() {
            let file = pinned
                .get(id)
                .or_else(|| clustered.get(id))
                .cloned()
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
        "locality"
    }
}

// INTENT: Build a reverse lookup from bare symbol name to full SymbolId.
fn build_bare_name_index(registry: &SymbolRegistry) -> HashMap<String, SymbolId> {
    let mut map = HashMap::new();
    for (id, _) in registry.iter() {
        let bare = id
            .as_str()
            .rsplit("::")
            .next()
            .unwrap_or(id.as_str())
            .to_string();
        map.entry(bare).or_insert_with(|| id.clone());
    }
    map
}

// INTENT: Pick the target file for an SCC cluster via lexicographically first path.
fn pick_cluster_file(ids: &[SymbolId], src_root: &str) -> PathBuf {
    ids.iter()
        .map(|id| assign_file(id, src_root))
        .min()
        .unwrap_or_else(|| PathBuf::from(format!("{src_root}/main.rs")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symgraph::registry::{SymbolEntry, SymbolId, SymbolKind, SymbolRegistry};

    fn make_cycle_registry() -> SymbolRegistry {
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        let mut a = SymbolEntry::new(
            SymbolId::new("core", "alpha"),
            SymbolKind::Function,
            "fn alpha() { beta(); }".into(),
            "rust",
        );
        a.dependencies.insert(SymbolId::new("core", "beta"));
        let mut b = SymbolEntry::new(
            SymbolId::new("core", "beta"),
            SymbolKind::Function,
            "fn beta() { alpha(); }".into(),
            "rust",
        );
        b.dependencies.insert(SymbolId::new("core", "alpha"));
        reg.insert(a);
        reg.insert(b);
        reg
    }

    #[test]
    fn scc_cluster_colocates_cycle() {
        let s = LocalityStrategy::default();
        let result = s
            .partition(&make_cycle_registry(), &LayoutRules::default())
            .unwrap();
        assert_eq!(result.values().flatten().count(), 2);
        assert_eq!(result.len(), 1, "cycle members should share one file");
    }

    #[test]
    fn singleton_falls_back_to_assign_file() {
        let s = LocalityStrategy::default();
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        reg.insert(SymbolEntry::new(
            SymbolId::new("utils", "helper"),
            SymbolKind::Function,
            "fn helper() {}".into(),
            "rust",
        ));
        let result = s.partition(&reg, &LayoutRules::default()).unwrap();
        assert!(result.contains_key(&PathBuf::from("src/utils.rs")));
    }

    #[test]
    fn pinned_symbols_not_moved() {
        let s = LocalityStrategy::default();
        let mut reg = SymbolRegistry::new(PathBuf::from("/tmp"));
        let mut a = SymbolEntry::new(
            SymbolId::new("core", "alpha"),
            SymbolKind::Function,
            "fn alpha() { beta(); }".into(),
            "rust",
        );
        a.dependencies.insert(SymbolId::new("core", "beta"));
        a.assigned_file = Some(PathBuf::from("pinned/alpha.rs"));
        let mut b = SymbolEntry::new(
            SymbolId::new("core", "beta"),
            SymbolKind::Function,
            "fn beta() { alpha(); }".into(),
            "rust",
        );
        b.dependencies.insert(SymbolId::new("core", "alpha"));
        reg.insert(a);
        reg.insert(b);
        let result = s.partition(&reg, &LayoutRules::default()).unwrap();
        assert!(result.contains_key(&PathBuf::from("pinned/alpha.rs")));
    }

    #[test]
    fn name_returns_locality() {
        assert_eq!(LocalityStrategy::default().name(), "locality");
    }
}
