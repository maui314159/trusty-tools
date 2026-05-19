// Gated on the `parser` feature — these integration tests exercise the
// tree-sitter-backed pipeline (parser, emitter, graph, registry, strategies).
#![cfg(feature = "parser")]

//! Integration tests for `parse_source` / `parse_directory`.
//!
//! Why: Validates the parser path through the public crate API.
//! What: Parses Rust source strings and a tiny on-disk directory.
//! Test: `cargo test -p trusty-symgraph --test parser_tests`.

use std::fs;
use tempfile::TempDir;
use trusty_symgraph::parse_directory;
use trusty_symgraph::parser::{Language, parse_source};

#[test]
fn parse_rust_function() {
    let entries = parse_source(
        "fn answer() -> i32 { 42 }",
        Language::Rust,
        "demo",
        std::path::Path::new("src/demo.rs"),
    )
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id.as_str(), "demo::answer");
}

#[test]
fn parse_directory_picks_up_files() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.rs"), "fn aaa() {}\n").unwrap();
    fs::write(src.join("b.rs"), "fn bbb() {}\n").unwrap();

    let reg = parse_directory(&src, tmp.path()).unwrap();
    assert!(reg.len() >= 2, "expected ≥2 symbols, got {}", reg.len());
}
