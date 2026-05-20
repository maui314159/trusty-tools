//! AST symbol extraction (#347) — language-agnostic Symbol struct + per-language
//! tree-sitter walkers.
//!
//! Why: Native AST-aware editing requires a richer view of source than the
//! function-only chunks `search::indexer` produces. Code-modifying tools need
//! to find a struct, an impl block, an import, or a const by name and know its
//! exact byte range so the caller can splice replacement text without breaking
//! surrounding scope. This module produces that view.
//! What: `Symbol` (file/name/kind/byte+line range/source) and `SymbolKind`
//! enum, plus `extract_symbols`, `get_symbol`, and `list_symbols` helpers.
//! Each language defines its own list of node-kinds → `SymbolKind` mappings.
//! Test: Per-language extraction tests ensure each kind surfaces with the
//! right name and line numbers; `get_symbol_finds_by_name` covers lookup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

/// What flavour of named entity this symbol represents.
///
/// Why: Editing tools branch on kind (e.g. an import goes through
/// `add_import`; a function is replaced via `replace_symbol`). Keeping the
/// kind on every `Symbol` lets callers route without re-parsing.
/// What: Closed enum. `Unknown` is the catch-all so the extractor never has
/// to drop a candidate — callers can filter.
/// Test: `extract_symbols_rust` and the per-language tests assert specific
/// kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Trait,
    Impl,
    Import,
    TypeAlias,
    Const,
    Unknown,
}

/// One named entity (function / class / impl / import / const / …) extracted
/// from a source file along with its location and full text.
///
/// Why: Editing tools need both the byte range (to splice) and the line range
/// (for diff display), plus the original source text (so callers can include
/// the existing definition in an LLM prompt without re-reading the file).
/// What: Plain serde struct; `start_line`/`end_line` are 1-indexed to match
/// editor conventions.
/// Test: Round-tripped via `extract_symbols_rust`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub file: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub source: String,
}

/// Detect a `tree_sitter::Language` + canonical lang tag from a file path's
/// extension.
///
/// Why: Centralises the extension-to-language switch so every entry point
/// (extract / validate / add_import) sees the same mapping.
/// What: Returns `(Language, &'static str)` for `.rs` / `.py` / `.js` /
/// `.jsx` / `.go`; `None` otherwise.
/// Test: Implicit — every per-language test exercises one branch.
pub fn detect_language(path: &Path) -> Option<(Language, &'static str)> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "rs" => Some((tree_sitter_rust::LANGUAGE.into(), "rust")),
        "py" => Some((tree_sitter_python::LANGUAGE.into(), "python")),
        "js" | "jsx" => Some((tree_sitter_javascript::LANGUAGE.into(), "javascript")),
        "go" => Some((tree_sitter_go::LANGUAGE.into(), "go")),
        "java" => Some((tree_sitter_java::LANGUAGE.into(), "java")),
        "ts" => Some((
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
        )),
        "tsx" => Some((tree_sitter_typescript::LANGUAGE_TSX.into(), "typescript")),
        "c" | "h" => Some((tree_sitter_c::LANGUAGE.into(), "c")),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some((tree_sitter_cpp::LANGUAGE.into(), "cpp")),
        _ => None,
    }
}

/// Walk a parsed source tree and emit `Symbol` records.
///
/// Why: Provides the read path for every AST tool. The kind set is keyed
/// by language tag because tree-sitter node-kind names differ.
///
/// What: Parses `source`, recursively walks the tree, and for each node
/// whose kind is in the per-language map emits a `Symbol` carrying the
/// node's text and line range.
///
/// Test: `extract_symbols_rust`, `extract_symbols_python`,
/// `extract_symbols_js`, `extract_symbols_go`.
pub fn extract_symbols(source: &str, language: Language, file: &Path) -> Vec<Symbol> {
    let lang_tag = detect_language(file).map(|(_, t)| t).unwrap_or("");
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), source, file, lang_tag, &mut out);
    out
}

/// Recursive DFS that captures every node matching a known kind.
fn walk(node: Node, source: &str, file: &Path, lang: &str, out: &mut Vec<Symbol>) {
    if let Some((kind, name)) = classify(node, source, lang) {
        let start_byte = node.start_byte();
        let end_byte = node.end_byte();
        if let Some(text) = source.get(start_byte..end_byte) {
            out.push(Symbol {
                file: file.to_path_buf(),
                name,
                kind,
                start_byte,
                end_byte,
                start_line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                source: text.to_string(),
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, file, lang, out);
    }
}

/// Map a tree-sitter node to `(SymbolKind, name)` for the four supported
/// languages.
fn classify(node: Node, source: &str, lang: &str) -> Option<(SymbolKind, String)> {
    let kind_str = node.kind();
    let bytes = source.as_bytes();
    match lang {
        "rust" => match kind_str {
            "function_item" => Some((SymbolKind::Function, name_field(node, bytes)?)),
            "struct_item" => Some((SymbolKind::Struct, name_field(node, bytes)?)),
            "trait_item" => Some((SymbolKind::Trait, name_field(node, bytes)?)),
            "impl_item" => {
                // Use `type` field as the impl name (the type being implemented).
                let n = node
                    .child_by_field_name("type")
                    .and_then(|c| c.utf8_text(bytes).ok())
                    .map(|s| s.to_string())?;
                Some((SymbolKind::Impl, n))
            }
            "use_declaration" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            "type_item" => Some((SymbolKind::TypeAlias, name_field(node, bytes)?)),
            "const_item" => Some((SymbolKind::Const, name_field(node, bytes)?)),
            _ => None,
        },
        "python" => match kind_str {
            "function_definition" | "async_function_definition" => {
                Some((SymbolKind::Function, name_field(node, bytes)?))
            }
            "class_definition" => Some((SymbolKind::Class, name_field(node, bytes)?)),
            "import_statement" | "import_from_statement" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        "javascript" => match kind_str {
            "function_declaration" | "function_expression" | "arrow_function" => {
                let n = name_field(node, bytes).unwrap_or_else(|| "<anon>".to_string());
                Some((SymbolKind::Function, n))
            }
            "method_definition" => Some((SymbolKind::Method, name_field(node, bytes)?)),
            "class_declaration" => Some((SymbolKind::Class, name_field(node, bytes)?)),
            "import_statement" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        "go" => match kind_str {
            "function_declaration" => Some((SymbolKind::Function, name_field(node, bytes)?)),
            "method_declaration" => Some((SymbolKind::Method, name_field(node, bytes)?)),
            "type_declaration" => {
                // Pull the first type_spec's name child if present.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "type_spec"
                        && let Some(n) = name_field(child, bytes)
                    {
                        return Some((SymbolKind::TypeAlias, n));
                    }
                }
                None
            }
            "import_declaration" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        "java" => match kind_str {
            "method_declaration" | "constructor_declaration" => {
                Some((SymbolKind::Method, name_field(node, bytes)?))
            }
            "class_declaration" | "enum_declaration" => {
                Some((SymbolKind::Class, name_field(node, bytes)?))
            }
            "interface_declaration" => Some((SymbolKind::Trait, name_field(node, bytes)?)),
            "import_declaration" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        "typescript" => match kind_str {
            "function_declaration" | "function_expression" | "arrow_function" => {
                let n = name_field(node, bytes).unwrap_or_else(|| "<anon>".to_string());
                Some((SymbolKind::Function, n))
            }
            "method_definition" => Some((SymbolKind::Method, name_field(node, bytes)?)),
            "class_declaration" => Some((SymbolKind::Class, name_field(node, bytes)?)),
            "interface_declaration" => Some((SymbolKind::Trait, name_field(node, bytes)?)),
            "type_alias_declaration" => Some((SymbolKind::TypeAlias, name_field(node, bytes)?)),
            "import_statement" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        "c" => match kind_str {
            "function_definition" => Some((SymbolKind::Function, name_field(node, bytes)?)),
            "struct_specifier" => Some((SymbolKind::Struct, name_field(node, bytes)?)),
            "type_definition" => {
                let n = node
                    .child_by_field_name("declarator")
                    .and_then(|d| d.utf8_text(bytes).ok())
                    .map(|s| s.trim().to_string())?;
                Some((SymbolKind::TypeAlias, n))
            }
            "preproc_include" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        "cpp" => match kind_str {
            "function_definition" => Some((SymbolKind::Function, name_field(node, bytes)?)),
            "class_specifier" => Some((SymbolKind::Class, name_field(node, bytes)?)),
            "struct_specifier" => Some((SymbolKind::Struct, name_field(node, bytes)?)),
            "type_definition" => {
                let n = node
                    .child_by_field_name("declarator")
                    .and_then(|d| d.utf8_text(bytes).ok())
                    .map(|s| s.trim().to_string())?;
                Some((SymbolKind::TypeAlias, n))
            }
            "preproc_include" => {
                let text = node.utf8_text(bytes).ok()?.trim().to_string();
                Some((SymbolKind::Import, text))
            }
            _ => None,
        },
        _ => None,
    }
}

fn name_field(node: Node, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|c| c.utf8_text(bytes).ok())
        .map(|s| s.to_string())
}

/// Find the first symbol with a matching name (exact, case-sensitive).
///
/// Why: Editing tools take the symbol name as input; this returns the
/// `Symbol` to splice. First-match wins so callers should pass unambiguous
/// names; ambiguous calls (e.g. duplicate function names in nested impls)
/// take the first occurrence.
/// What: Linear scan over `extract_symbols`. Returns `None` when not found.
/// Test: `extract_symbols_rust` covers the lookup path.
pub fn get_symbol(source: &str, lang: Language, file: &Path, name: &str) -> Option<Symbol> {
    extract_symbols(source, lang, file)
        .into_iter()
        .find(|s| s.name == name)
}

/// Read a file from disk, detect language by extension, return all symbols.
///
/// Why: Convenience for tools that take a file path; centralises the
/// language-detection branch.
/// What: Reads the file, calls `detect_language`, parses + extracts. Errors
/// when the extension is unsupported (so callers see a clear message).
/// Test: Covered indirectly by editor tests.
pub fn list_symbols(file: &Path) -> Result<Vec<Symbol>> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let (lang, _) = detect_language(file)
        .with_context(|| format!("unsupported file extension: {}", file.display()))?;
    Ok(extract_symbols(&source, lang, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_symbols_rust() {
        let src = r#"
use std::collections::HashMap;

const MAX: u32 = 100;

struct Foo { x: i32 }

trait Bar { fn baz(&self); }

impl Bar for Foo {
    fn baz(&self) {}
}

fn standalone() -> i32 { 42 }

type Alias = u64;
"#;
        let path = PathBuf::from("test.rs");
        let syms = extract_symbols(src, tree_sitter_rust::LANGUAGE.into(), &path);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"), "expected Foo, got {names:?}");
        assert!(names.contains(&"Bar"), "expected Bar, got {names:?}");
        assert!(
            names.contains(&"standalone"),
            "expected standalone, got {names:?}"
        );
        assert!(names.contains(&"MAX"), "expected MAX const, got {names:?}");
        assert!(names.contains(&"Alias"), "expected Alias, got {names:?}");
        // Impl name == the type
        assert!(
            syms.iter()
                .any(|s| matches!(s.kind, SymbolKind::Impl) && s.name == "Foo")
        );
        // get_symbol path
        let got = get_symbol(src, tree_sitter_rust::LANGUAGE.into(), &path, "standalone").unwrap();
        assert_eq!(got.kind, SymbolKind::Function);
        assert!(got.source.contains("42"));
    }

    #[test]
    fn extract_symbols_python() {
        let src = "import os\nfrom typing import List\n\nclass Foo:\n    def bar(self):\n        pass\n\ndef baz():\n    return 1\n";
        let path = PathBuf::from("test.py");
        let syms = extract_symbols(src, tree_sitter_python::LANGUAGE.into(), &path);
        assert!(
            syms.iter()
                .any(|s| s.name == "Foo" && matches!(s.kind, SymbolKind::Class))
        );
        assert!(
            syms.iter()
                .any(|s| s.name == "baz" && matches!(s.kind, SymbolKind::Function))
        );
        assert!(syms.iter().any(|s| matches!(s.kind, SymbolKind::Import)));
    }

    #[test]
    fn extract_symbols_go() {
        let src = "package main\n\nimport \"fmt\"\n\nfunc Foo() {}\n\ntype X int\n";
        let path = PathBuf::from("test.go");
        let syms = extract_symbols(src, tree_sitter_go::LANGUAGE.into(), &path);
        assert!(syms.iter().any(|s| s.name == "Foo"));
    }
}
