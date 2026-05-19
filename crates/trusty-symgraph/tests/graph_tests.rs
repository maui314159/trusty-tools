// Gated on the `parser` feature — these integration tests exercise the
// tree-sitter-backed pipeline (parser, emitter, graph, registry, strategies).
#![cfg(feature = "parser")]

//! Integration tests for the petgraph-backed `SymbolGraph` (#356).
//!
//! Why: Confirms the internal `StableGraph` exposes the same node / edge
//! counts that the public `nodes()` / `edges()` accessors report — i.e.
//! the inner petgraph and the public surface stay in sync.
//! What: Builds a graph from a tiny in-memory Rust source, then asserts
//! `inner().node_count() == nodes().len()` and likewise for edges.
//! Test: `cargo test -p trusty-symgraph --test graph_tests`.

use std::io::Write;
use trusty_symgraph::SymbolGraph;

#[test]
fn petgraph_view_basic() {
    let src = "fn caller() { callee(); }\n\nfn callee() {}\n";
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(src.as_bytes()).unwrap();
    let p = tmp.path().with_extension("rs");
    std::fs::copy(tmp.path(), &p).unwrap();

    let g = SymbolGraph::build_from_file(&p).unwrap();
    let _ = std::fs::remove_file(&p);

    let pg = g.inner();
    assert_eq!(pg.node_count(), g.nodes().len());
    assert_eq!(pg.edge_count(), g.edges().len());
}
