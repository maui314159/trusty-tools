// Gated on the `parser` feature — these integration tests exercise the
// tree-sitter-backed pipeline (parser, emitter, graph, registry, strategies).
#![cfg(feature = "parser")]

//! Integration tests for all three EmitStrategy implementations.
//!
//! Covers partition correctness, ordering, edge cases, backward compatibility,
//! and cross-strategy validation. Does NOT test rendering/imports — those are
//! emit()'s job, not the strategy's.

use std::path::PathBuf;

use trusty_symgraph::emitter::{LayoutRules, emit};
use trusty_symgraph::locality::LocalityStrategy;
use trusty_symgraph::registry::{SymbolEntry, SymbolId, SymbolKind, SymbolRegistry};
use trusty_symgraph::strategy::{EmitStrategy, ModulePathStrategy};
use trusty_symgraph::test_colocation::TestColocationStrategy;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn empty_registry() -> SymbolRegistry {
    SymbolRegistry::new(PathBuf::from("/tmp"))
}

fn default_rules() -> LayoutRules {
    LayoutRules::default()
}

// ---------------------------------------------------------------------------
// ModulePathStrategy tests
// ---------------------------------------------------------------------------

// INTENT: Verify module-path partition produces the same file→symbol mapping as legacy assign_file.
#[test]
fn module_path_partition_matches_legacy() {
    let strategy = ModulePathStrategy::default();
    let mut reg = empty_registry();

    reg.insert(SymbolEntry::new(
        SymbolId::new("api::handlers", "process"),
        SymbolKind::Function,
        "fn process() {}".into(),
        "rust",
    ));
    reg.insert(SymbolEntry::new(
        SymbolId::new("utils", "helper"),
        SymbolKind::Function,
        "fn helper() {}".into(),
        "rust",
    ));
    reg.insert(SymbolEntry::new(
        SymbolId::new("", "main"),
        SymbolKind::Function,
        "fn main() {}".into(),
        "rust",
    ));

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    assert!(result.contains_key(&PathBuf::from("src/api/handlers.rs")));
    assert!(result.contains_key(&PathBuf::from("src/utils.rs")));
    assert!(result.contains_key(&PathBuf::from("src/main.rs")));

    // Each file should contain exactly one symbol.
    for ids in result.values() {
        assert_eq!(ids.len(), 1);
    }
}

// INTENT: Verify assigned_file override takes precedence over assign_file().
#[test]
fn module_path_respects_assigned_file_override() {
    let strategy = ModulePathStrategy::default();
    let mut reg = empty_registry();

    let mut entry = SymbolEntry::new(
        SymbolId::new("utils", "helper"),
        SymbolKind::Function,
        "fn helper() {}".into(),
        "rust",
    );
    entry.assigned_file = Some(PathBuf::from("custom/path.rs"));
    reg.insert(entry);

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    assert!(result.contains_key(&PathBuf::from("custom/path.rs")));
    assert!(!result.contains_key(&PathBuf::from("src/utils.rs")));
}

// INTENT: Verify topological ordering places dependencies before dependents.
#[test]
fn module_path_order_is_topological() {
    let strategy = ModulePathStrategy::default();
    let mut reg = empty_registry();

    reg.insert(SymbolEntry::new(
        SymbolId::new("core", "callee"),
        SymbolKind::Function,
        "fn callee() {}".into(),
        "rust",
    ));

    let mut caller = SymbolEntry::new(
        SymbolId::new("core", "caller"),
        SymbolKind::Function,
        "fn caller() { callee(); }".into(),
        "rust",
    );
    caller.dependencies.insert(SymbolId::new("core", "callee"));
    reg.insert(caller);

    let ids: Vec<SymbolId> = reg.iter().map(|(id, _)| id.clone()).collect();
    let ordered = strategy.order_within_file(&ids, &reg).unwrap();

    assert_eq!(ordered.len(), 2);
    let callee_pos = ordered
        .iter()
        .position(|id| id.as_str() == "core::callee")
        .unwrap();
    let caller_pos = ordered
        .iter()
        .position(|id| id.as_str() == "core::caller")
        .unwrap();
    assert!(callee_pos < caller_pos, "callee must appear before caller");
}

// INTENT: Verify emit() with ModulePathStrategy produces byte-identical output across runs.
#[test]
fn module_path_emit_backward_compat() {
    let mut reg = empty_registry();
    reg.insert(SymbolEntry::new(
        SymbolId::new("utils", "helper"),
        SymbolKind::Function,
        "fn helper() {}".into(),
        "rust",
    ));
    reg.insert(SymbolEntry::new(
        SymbolId::new("api", "serve"),
        SymbolKind::Function,
        "fn serve() {}".into(),
        "rust",
    ));

    let rules = default_rules();
    let strategy = ModulePathStrategy::default();

    let out1 = emit(&reg, &rules, &strategy).unwrap();
    let out2 = emit(&reg, &rules, &strategy).unwrap();

    assert_eq!(out1, out2, "emit must be deterministic across invocations");
    assert!(!out1.is_empty());
}

// ---------------------------------------------------------------------------
// LocalityStrategy tests
// ---------------------------------------------------------------------------

// INTENT: Verify mutually-dependent symbols are clustered into the same file.
#[test]
fn locality_clusters_mutual_callers() {
    let strategy = LocalityStrategy::default();
    let mut reg = empty_registry();

    let mut a = SymbolEntry::new(
        SymbolId::new("mod_a", "alpha"),
        SymbolKind::Function,
        "fn alpha() { beta(); }".into(),
        "rust",
    );
    a.dependencies.insert(SymbolId::new("mod_b", "beta"));

    let mut b = SymbolEntry::new(
        SymbolId::new("mod_b", "beta"),
        SymbolKind::Function,
        "fn beta() { alpha(); }".into(),
        "rust",
    );
    b.dependencies.insert(SymbolId::new("mod_a", "alpha"));

    let c = SymbolEntry::new(
        SymbolId::new("mod_c", "gamma"),
        SymbolKind::Function,
        "fn gamma() {}".into(),
        "rust",
    );

    reg.insert(a);
    reg.insert(b);
    reg.insert(c);

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    // gamma should be separate from the alpha/beta cluster.
    assert!(result.contains_key(&PathBuf::from("src/mod_c.rs")));

    // alpha and beta should share a file (SCC cluster).
    let gamma_file = PathBuf::from("src/mod_c.rs");
    let mut non_gamma_files: Vec<&PathBuf> = result.keys().filter(|k| *k != &gamma_file).collect();
    non_gamma_files.sort();

    // The cluster should produce exactly one file for alpha+beta.
    let cluster_ids: Vec<&SymbolId> = non_gamma_files
        .iter()
        .flat_map(|f| result[*f].iter())
        .collect();
    assert_eq!(
        cluster_ids.len(),
        2,
        "alpha and beta should be in one cluster"
    );
}

// INTENT: Verify a symbol with no call edges falls back to assign_file().
#[test]
fn locality_singleton_fallback() {
    let strategy = LocalityStrategy::default();
    let mut reg = empty_registry();

    reg.insert(SymbolEntry::new(
        SymbolId::new("utils", "helper"),
        SymbolKind::Function,
        "fn helper() {}".into(),
        "rust",
    ));

    let result = strategy.partition(&reg, &default_rules()).unwrap();
    assert!(result.contains_key(&PathBuf::from("src/utils.rs")));
}

// INTENT: Verify pinned (assigned_file) symbols are not relocated by locality clustering.
#[test]
fn locality_respects_assigned_file_override() {
    let strategy = LocalityStrategy::default();
    let mut reg = empty_registry();

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

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    // Pinned symbol must remain at its override path.
    assert!(result.contains_key(&PathBuf::from("pinned/alpha.rs")));
}

// INTENT: Verify cluster file is lexicographically earliest among member module paths.
#[test]
fn locality_deterministic_tiebreak() {
    let strategy = LocalityStrategy::default();
    let mut reg = empty_registry();

    let mut a = SymbolEntry::new(
        SymbolId::new("b::foo", "x"),
        SymbolKind::Function,
        "fn x() { y(); }".into(),
        "rust",
    );
    a.dependencies.insert(SymbolId::new("a::bar", "y"));

    let mut b = SymbolEntry::new(
        SymbolId::new("a::bar", "y"),
        SymbolKind::Function,
        "fn y() { x(); }".into(),
        "rust",
    );
    b.dependencies.insert(SymbolId::new("b::foo", "x"));

    reg.insert(a);
    reg.insert(b);

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    // Both should land in the lex-first path: src/a/bar.rs < src/b/foo.rs.
    assert_eq!(result.len(), 1, "cycle should produce one cluster file");
    assert!(
        result.contains_key(&PathBuf::from("src/a/bar.rs")),
        "cluster file should be the lexicographically first path"
    );
}

// ---------------------------------------------------------------------------
// TestColocationStrategy tests
// ---------------------------------------------------------------------------

// INTENT: Verify a test with test_covers lands in the same file as its target.
#[test]
fn test_colocation_places_test_with_target() {
    let strategy = TestColocationStrategy::default();
    let mut reg = empty_registry();

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

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    assert_eq!(result.len(), 1, "test and target should share one file");
    let ids = result.values().next().unwrap();
    assert_eq!(ids.len(), 2);
}

// INTENT: Verify a test without test_covers falls back to assign_file().
#[test]
fn test_colocation_test_without_covers_uses_fallback() {
    let strategy = TestColocationStrategy::default();
    let mut reg = empty_registry();

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

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    // Orphan test falls back to its own module path.
    assert!(result.contains_key(&PathBuf::from("src/tests.rs")));
    assert!(result.contains_key(&PathBuf::from("src/other.rs")));
}

// INTENT: Verify non-test symbols are ordered before test symbols within a file.
#[test]
fn test_colocation_order_non_test_before_test() {
    let strategy = TestColocationStrategy::default();
    let mut reg = empty_registry();

    reg.insert(SymbolEntry::new(
        SymbolId::new("utils", "fn_b"),
        SymbolKind::Function,
        "fn fn_b() {}".into(),
        "rust",
    ));

    let mut test_a = SymbolEntry::new(
        SymbolId::new("utils", "test_a"),
        SymbolKind::Test,
        "#[test] fn test_a() {}".into(),
        "rust",
    );
    test_a.test_covers = Some(SymbolId::new("utils", "fn_b"));
    reg.insert(test_a);

    let ids: Vec<SymbolId> = reg.iter().map(|(id, _)| id.clone()).collect();
    let ordered = strategy.order_within_file(&ids, &reg).unwrap();

    assert_eq!(ordered.len(), 2);
    assert_eq!(
        ordered[0].as_str(),
        "utils::fn_b",
        "production symbol must come first"
    );
    assert_eq!(
        ordered[1].as_str(),
        "utils::test_a",
        "test symbol must come second"
    );
}

// INTENT: Verify test follows target's assigned_file override.
#[test]
fn test_colocation_follows_target_override() {
    let strategy = TestColocationStrategy::default();
    let mut reg = empty_registry();

    let mut prod = SymbolEntry::new(
        SymbolId::new("core", "engine"),
        SymbolKind::Function,
        "fn engine() {}".into(),
        "rust",
    );
    prod.assigned_file = Some(PathBuf::from("custom.rs"));
    reg.insert(prod);

    let mut test_entry = SymbolEntry::new(
        SymbolId::new("core", "test_engine"),
        SymbolKind::Test,
        "#[test] fn test_engine() {}".into(),
        "rust",
    );
    test_entry.test_covers = Some(SymbolId::new("core", "engine"));
    reg.insert(test_entry);

    let result = strategy.partition(&reg, &default_rules()).unwrap();

    assert!(result.contains_key(&PathBuf::from("custom.rs")));
    assert_eq!(result.len(), 1, "test must follow target to custom.rs");
}

// ---------------------------------------------------------------------------
// Cross-strategy tests
// ---------------------------------------------------------------------------

// INTENT: Verify all three strategies produce valid, non-empty emit output with the header comment.
#[test]
fn all_strategies_produce_valid_emit_output() {
    let mut reg = empty_registry();
    reg.insert(SymbolEntry::new(
        SymbolId::new("utils", "helper"),
        SymbolKind::Function,
        "fn helper() {}".into(),
        "rust",
    ));

    let mut caller = SymbolEntry::new(
        SymbolId::new("api", "serve"),
        SymbolKind::Function,
        "fn serve() { helper(); }".into(),
        "rust",
    );
    caller.dependencies.insert(SymbolId::new("utils", "helper"));
    reg.insert(caller);

    let rules = default_rules();

    let strategies: Vec<Box<dyn EmitStrategy>> = vec![
        Box::new(ModulePathStrategy::default()),
        Box::new(LocalityStrategy::default()),
        Box::new(TestColocationStrategy::default()),
    ];

    for strategy in &strategies {
        let output = emit(&reg, &rules, strategy.as_ref()).unwrap();
        assert!(
            !output.is_empty(),
            "{} strategy produced empty output",
            strategy.name()
        );
        for (path, content) in &output {
            assert!(
                !content.is_empty(),
                "{} strategy produced empty content for {:?}",
                strategy.name(),
                path
            );
            assert!(
                content.contains("Generated by open-mpm"),
                "{} strategy missing header comment in {:?}",
                strategy.name(),
                path
            );
        }
    }
}

// INTENT: Verify all three strategies handle an empty registry without errors.
#[test]
fn emit_with_empty_registry() {
    let reg = empty_registry();
    let rules = default_rules();

    let strategies: Vec<Box<dyn EmitStrategy>> = vec![
        Box::new(ModulePathStrategy::default()),
        Box::new(LocalityStrategy::default()),
        Box::new(TestColocationStrategy::default()),
    ];

    for strategy in &strategies {
        let output = emit(&reg, &rules, strategy.as_ref()).unwrap();
        assert!(
            output.is_empty(),
            "{} strategy should produce empty output for empty registry",
            strategy.name()
        );
    }
}
