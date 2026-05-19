// Gated on the `parser` feature — these integration tests exercise the
// tree-sitter-backed pipeline (parser, emitter, graph, registry, strategies).
#![cfg(feature = "parser")]

//! Integration tests for `emit` / `apply_emit`.
//!
//! Why: Validates the registry → file projection round-trip.
//! What: Builds a tiny registry, emits to a temp dir, asserts the emitted
//! file contains the symbol's source.
//! Test: `cargo test -p trusty-symgraph --test emitter_tests`.

use tempfile::TempDir;
use trusty_symgraph::registry::SymbolKind;
use trusty_symgraph::{
    LayoutRules, ModulePathStrategy, SymbolEntry, SymbolId, SymbolRegistry, apply_emit, emit,
};

// INTENT: Verify that emit + apply_emit writes files containing the expected symbol source.
#[test]
fn emit_writes_files() {
    let tmp = TempDir::new().unwrap();
    let mut reg = SymbolRegistry::new(tmp.path().to_path_buf());
    reg.insert(SymbolEntry::new(
        SymbolId::new("util", "helper"),
        SymbolKind::Function,
        "fn helper() -> i32 { 1 }".into(),
        "rust",
    ));
    let outputs = emit(
        &reg,
        &LayoutRules::default(),
        &ModulePathStrategy::default(),
    )
    .unwrap();
    let written = apply_emit(&outputs, tmp.path()).unwrap();
    assert!(!written.is_empty());

    let contents = std::fs::read_to_string(&written[0]).unwrap();
    assert!(contents.contains("fn helper"));
}

// INTENT: Verify that two consecutive emit calls produce byte-identical output.
#[test]
fn emit_is_deterministic() {
    let mut reg = SymbolRegistry::new(std::path::PathBuf::from("/tmp"));
    reg.insert(SymbolEntry::new(
        SymbolId::new("a", "x"),
        SymbolKind::Function,
        "fn x() {}".into(),
        "rust",
    ));
    reg.insert(SymbolEntry::new(
        SymbolId::new("b", "y"),
        SymbolKind::Function,
        "fn y() {}".into(),
        "rust",
    ));

    let r = LayoutRules::default();
    let s = ModulePathStrategy::default();
    let a = emit(&reg, &r, &s).unwrap();
    let b = emit(&reg, &r, &s).unwrap();
    assert_eq!(a, b);
}
