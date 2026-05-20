//! Self-analysis integration tests.
//!
//! Why: The strongest validation of the static-analysis pipeline is that it
//! produces meaningful, non-trivial output when run against this project's
//! own Rust source. If we cannot analyze ourselves, the pipeline is broken.
//!
//! What: Four tests that build `CodeChunk` corpora from this crate's `src/`
//! (and, for the workspace test, the whole `crates/` tree), feed them
//! through `RustAnalyzer` / `AnalyzerRegistry` / `compute_complexity_for` /
//! `aggregate_quality`, and assert non-trivial structure (function counts,
//! Contains edges, A/B average grade, etc.).
//!
//! Test: All four tests print a one-line summary via `println!` and run as
//! part of the regular `cargo test` suite (no `#[ignore]`).

#![cfg(test)]

use std::path::Path;

use crate::lang::{adapters::rust::RustAnalyzer, LanguageAnalyzer};
use crate::types::complexity::ComplexityGrade;
use crate::types::{KgEdgeKind, KgNodeKind};

use crate::core::complexity::compute_complexity_for;
use crate::core::quality::aggregate_quality;
use crate::core::registry::AnalyzerRegistry;
use crate::core::test_utils::chunks_from_dir;

#[test]
fn self_analysis_finds_functions() {
    // Analyze this project's own trusty-analyzer-core/src/ directory.
    let core_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let chunks = chunks_from_dir(&core_src, ".rs").expect("read own source");

    assert!(!chunks.is_empty(), "should have found .rs chunks");

    let analyzer = RustAnalyzer::new();
    let result = analyzer.analyze_chunks(&chunks);

    let fn_count = result
        .graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, KgNodeKind::Function | KgNodeKind::Method))
        .count();
    assert!(fn_count >= 10, "expected >=10 functions, got {fn_count}");

    let contains = result
        .graph
        .edges
        .iter()
        .filter(|e| e.kind == KgEdgeKind::Contains)
        .count();
    assert!(contains >= 5, "expected >=5 Contains edges, got {contains}");

    assert!(
        result.errors.is_empty(),
        "parse errors: {:?}",
        result.errors
    );

    println!(
        "self_analysis: {} nodes, {} edges, {} chunks analyzed",
        result.graph.node_count(),
        result.graph.edge_count(),
        result.analyzed_chunks
    );
}

#[test]
fn self_analysis_complexity_is_accurate() {
    let core_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let chunks = chunks_from_dir(&core_src, ".rs").expect("read own source");

    let metrics: Vec<_> = chunks
        .iter()
        .map(|c| compute_complexity_for(&c.content, "rust"))
        .collect();

    assert!(
        metrics.iter().all(|m| m.cyclomatic >= 1),
        "cyclomatic must be >= 1 for all chunks"
    );

    let branchy = metrics.iter().filter(|m| m.cyclomatic > 1).count();
    assert!(
        branchy >= 3,
        "expected >=3 chunks with branching logic, got {branchy}"
    );

    let avg_cyclo = metrics.iter().map(|m| m.cyclomatic as f64).sum::<f64>() / metrics.len() as f64;
    assert!(
        avg_cyclo < 11.0,
        "average cyclomatic {avg_cyclo:.1} is too high (expected < 11)"
    );

    println!(
        "complexity: {:.1} avg cyclomatic over {} chunks, {} branchy",
        avg_cyclo,
        metrics.len(),
        branchy
    );
}

#[test]
fn self_analysis_quality_aggregate() {
    let core_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let chunks = chunks_from_dir(&core_src, ".rs").expect("read own source");

    let quality = aggregate_quality(&chunks);

    // QualityReport does not carry an aggregate letter grade; derive one from
    // the rounded average cyclomatic number using the same banding the
    // per-chunk grader uses.
    let overall_grade = ComplexityGrade::from_cyclomatic(quality.avg_cyclomatic.round() as u32);

    assert!(
        matches!(overall_grade, ComplexityGrade::A | ComplexityGrade::B),
        "expected A/B grade for own source, got {:?}",
        overall_grade
    );
    assert!(
        quality.avg_cyclomatic < 11.0,
        "avg cyclomatic {:.1} too high",
        quality.avg_cyclomatic
    );

    println!(
        "quality: grade={:?}, avg_cyclomatic={:.1}, pct_A={:.0}%, smells={}",
        overall_grade,
        quality.avg_cyclomatic,
        quality.pct_grade_a * 100.0,
        quality.smell_count
    );
}

#[test]
fn self_analysis_full_workspace() {
    // Single-crate layout: $CARGO_MANIFEST_DIR is the repo root and `src/`
    // contains every former crate's modules.
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let chunks = chunks_from_dir(&src_dir, ".rs").expect("read workspace");
    assert!(
        chunks.len() >= 20,
        "expected >=20 chunks from collapsed src tree, got {}",
        chunks.len()
    );

    let registry = AnalyzerRegistry::default_registry();
    let result = registry.analyze(&chunks);

    assert!(
        result.graph.node_count() >= 30,
        "expected >=30 nodes from workspace, got {}",
        result.graph.node_count()
    );
    assert!(
        result.graph.edge_count() >= 20,
        "expected >=20 edges from workspace, got {}",
        result.graph.edge_count()
    );
    assert!(
        result.analyzed_files >= 5,
        "expected >=5 files analyzed, got {}",
        result.analyzed_files
    );

    let fn_nodes = result
        .graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, KgNodeKind::Function | KgNodeKind::Method))
        .count();
    let type_nodes = result
        .graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, KgNodeKind::Class | KgNodeKind::Interface))
        .count();

    println!(
        "workspace: {} nodes ({fn_nodes} fns, {type_nodes} types), {} edges, {} files, {} chunks",
        result.graph.node_count(),
        result.graph.edge_count(),
        result.analyzed_files,
        result.analyzed_chunks
    );

    assert!(
        fn_nodes >= 10,
        "expected >=10 function nodes, got {fn_nodes}"
    );

    // After linking, duplicate fn nodes introduced by overlapping chunk
    // windows are collapsed to canonical representatives, so the number of
    // surviving fn nodes must be strictly less than the number of chunks
    // analyzed (which contained many redundant fn definitions).
    assert!(
        fn_nodes < result.analyzed_chunks,
        "after linking, fn_nodes ({fn_nodes}) should be < analyzed_chunks ({})",
        result.analyzed_chunks
    );
}
