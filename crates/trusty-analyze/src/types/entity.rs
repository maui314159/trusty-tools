//! Entity taxonomy and edge kinds shared between search and analyzer.
//!
//! Why: trusty-search-core, trusty-analyzer-core, and ingest pipelines all
//! consume the same `EntityType` / `EdgeKind` / `RawEntity` shapes. The shared
//! `trusty-symgraph` crate owns the canonical definitions (no tree-sitter
//! dep) and this module simply re-exports them so existing callers continue to
//! work via `crate::types::{EntityType, EdgeKind, RawEntity}`.
//!
//! What: pure re-exports from `trusty_symgraph`. No local definitions.
//!
//! Test: `entity_type_round_trips` exercises serde round-tripping through the
//! re-exported types.

pub use trusty_symgraph::contracts::EdgeKind;
pub use trusty_symgraph::{fact_hash_str, EntityType, RawEntity};

/// redb table name constants for entity storage, re-exported from
/// `trusty_symgraph::contracts::tables`.
pub mod tables {
    pub use trusty_symgraph::contracts::tables::*;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_type_round_trips() {
        let kinds = [
            EntityType::NamedType,
            EntityType::ModulePath,
            EntityType::TestRelation,
            EntityType::ConceptCluster,
        ];
        for k in kinds {
            let s = serde_json::to_string(&k).unwrap();
            let back: EntityType = serde_json::from_str(&s).unwrap();
            assert_eq!(k, back);
        }
    }
}
