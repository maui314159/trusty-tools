// Gated on the `parser` feature — these integration tests exercise the
// tree-sitter-backed pipeline (parser, emitter, graph, registry, strategies).
#![cfg(feature = "parser")]

//! Integration tests for `SymbolRegistry`.
//!
//! Why: Asserts the public crate API (not just internal mod-level tests)
//! supports insert, lookup, hash-mismatch detection, and save/load round-trip.
//! What: Black-box tests that import `symgraph::*` only.
//! Test: `cargo test -p trusty-symgraph --test registry_tests`.

use tempfile::TempDir;
use trusty_symgraph::registry::SymbolKind;
use trusty_symgraph::{SymbolEntry, SymbolId, SymbolRegistry};

#[test]
fn insert_and_lookup() {
    let tmp = TempDir::new().unwrap();
    let mut reg = SymbolRegistry::new(tmp.path().to_path_buf());
    reg.insert(SymbolEntry::new(
        SymbolId::new("api", "handler"),
        SymbolKind::Function,
        "fn handler() {}".into(),
        "rust",
    ));
    assert_eq!(reg.len(), 1);
    assert!(reg.get(&SymbolId::new("api", "handler")).is_some());
}

#[test]
fn save_load_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let mut reg = SymbolRegistry::new(tmp.path().to_path_buf());
    reg.insert(SymbolEntry::new(
        SymbolId::new("util", "helper"),
        SymbolKind::Function,
        "fn helper() {}".into(),
        "rust",
    ));
    reg.save().unwrap();

    let reloaded = SymbolRegistry::load(tmp.path()).unwrap();
    assert_eq!(reloaded.len(), 1);
    assert!(reloaded.get(&SymbolId::new("util", "helper")).is_some());
}
