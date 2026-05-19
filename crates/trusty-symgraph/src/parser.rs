//! Tree-sitter source → SymbolRegistry parser (#350).
//!
//! Why: Populating the registry deterministically from real source code is
//! the entry point for the AST↔code substrate. Reuses the same tree-sitter
//! grammars already wired in `search/indexer.rs` and `ast/symbol.rs`.
//! What: `Language` enum, language detection, file→module-path derivation,
//! and `parse_source` / `parse_file` / `parse_directory` helpers.
//! Test: See unit tests at the bottom — covers module-path derivation
//! (normal, `main.rs`, `mod.rs`) and basic Rust function/struct parsing.

use super::registry::{SymbolEntry, SymbolId, SymbolKind, SymbolRegistry};
use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use std::path::Path;
use tree_sitter::{Node, Parser};
use walkdir::WalkDir;

/// Source languages currently supported by the parser.
///
/// Why: A small closed enum keeps language-specific dispatch tables
/// honest at compile time.
/// What: Mirrors the grammars declared in `Cargo.toml`.
/// Test: Indirectly through `detect_language` tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    Go,
    Java,
    TypeScript,
    C,
    Cpp,
}

/// Detect language from a file extension.
///
/// Why: Unifies extension→language mapping in one place so callers don't
/// duplicate the match arms.
/// What: Returns `Some(Language)` for `.rs`/`.py`/`.js`/`.jsx`/`.go`,
/// `None` otherwise.
/// Test: Indirectly through `parse_directory` filtering.
pub fn detect_language(path: &Path) -> Option<Language> {
    match path.extension()?.to_str()? {
        "rs" => Some(Language::Rust),
        "py" => Some(Language::Python),
        "js" | "jsx" => Some(Language::JavaScript),
        "go" => Some(Language::Go),
        "java" => Some(Language::Java),
        "ts" | "tsx" => Some(Language::TypeScript),
        "c" | "h" => Some(Language::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(Language::Cpp),
        _ => None,
    }
}

/// Friendly language tag stored in `SymbolEntry.language`.
fn language_name(lang: Language) -> &'static str {
    match lang {
        Language::Rust => "rust",
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::Go => "go",
        Language::Java => "java",
        Language::TypeScript => "typescript",
        Language::C => "c",
        Language::Cpp => "cpp",
    }
}

/// Derive a Rust-style module path from a file path relative to project root.
///
/// Why: The registry keys symbols by `module::name`, so we need a single
/// canonical conversion from file paths.
/// What: Strips a leading `src/` if present; collapses `mod.rs` to its
/// parent; treats `main.rs` and `lib.rs` as the root module (`""`); replaces
/// path separators with `::`.
/// Test: See `test_file_to_module_path_*`.
pub fn file_to_module_path(file: &Path, project_root: &Path) -> String {
    let rel = match file.strip_prefix(project_root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let rel = rel.strip_prefix("src/").unwrap_or(rel);
    let stem = rel.with_extension("");
    let s = stem.to_string_lossy();

    let s = if s.ends_with("/mod") || s.ends_with("\\mod") {
        s[..s.len() - 4].to_string()
    } else if s == "main" || s == "lib" {
        String::new()
    } else {
        s.into_owned()
    };
    s.replace(['/', '\\'], "::")
}

/// Per-language node-kind → SymbolKind table.
///
/// Why: Tree-sitter exposes flat node-kind strings; mapping to our
/// `SymbolKind` once at the top keeps `parse_source` legible.
/// What: Returns a static slice of `(ts_node_kind, SymbolKind)` pairs.
/// Test: Indirectly through `test_parse_source_*`.
fn symbol_kinds_for_language(lang: Language) -> &'static [(&'static str, SymbolKind)] {
    match lang {
        Language::Rust => &[
            ("function_item", SymbolKind::Function),
            ("impl_item", SymbolKind::Impl),
            ("struct_item", SymbolKind::Struct),
            ("trait_item", SymbolKind::Trait),
            ("use_declaration", SymbolKind::Import),
            ("type_item", SymbolKind::TypeAlias),
            ("const_item", SymbolKind::Const),
        ],
        Language::Python => &[
            ("function_definition", SymbolKind::Function),
            ("async_function_definition", SymbolKind::Function),
            ("class_definition", SymbolKind::Class),
            ("import_statement", SymbolKind::Import),
            ("import_from_statement", SymbolKind::Import),
        ],
        Language::JavaScript => &[
            ("function_declaration", SymbolKind::Function),
            ("method_definition", SymbolKind::Method),
            ("class_declaration", SymbolKind::Class),
            ("import_declaration", SymbolKind::Import),
        ],
        Language::Go => &[
            ("function_declaration", SymbolKind::Function),
            ("method_declaration", SymbolKind::Method),
            ("type_declaration", SymbolKind::TypeAlias),
            ("import_declaration", SymbolKind::Import),
        ],
        Language::Java => &[
            ("method_declaration", SymbolKind::Method),
            ("constructor_declaration", SymbolKind::Method),
            ("class_declaration", SymbolKind::Class),
            ("interface_declaration", SymbolKind::Trait),
            ("enum_declaration", SymbolKind::Class),
            ("import_declaration", SymbolKind::Import),
        ],
        Language::TypeScript => &[
            ("function_declaration", SymbolKind::Function),
            ("method_definition", SymbolKind::Method),
            ("class_declaration", SymbolKind::Class),
            ("interface_declaration", SymbolKind::Trait),
            ("type_alias_declaration", SymbolKind::TypeAlias),
            ("import_statement", SymbolKind::Import),
        ],
        Language::C => &[
            ("function_definition", SymbolKind::Function),
            ("struct_specifier", SymbolKind::Struct),
            ("type_definition", SymbolKind::TypeAlias),
            ("preproc_include", SymbolKind::Import),
        ],
        Language::Cpp => &[
            ("function_definition", SymbolKind::Function),
            ("class_specifier", SymbolKind::Class),
            ("struct_specifier", SymbolKind::Struct),
            ("type_definition", SymbolKind::TypeAlias),
            ("preproc_include", SymbolKind::Import),
        ],
    }
}

/// Parse a single file and return its symbol entries.
///
/// Why: The most common entry point — caller owns iteration over file lists.
/// What: Detects language from extension, reads source, derives module
/// path from `project_root`, delegates to `parse_source`.
/// Test: Indirectly through `parse_directory`.
pub fn parse_file(file: &Path, project_root: &Path) -> Result<Vec<SymbolEntry>> {
    let lang = detect_language(file).ok_or_else(|| anyhow!("Unsupported: {}", file.display()))?;
    let source = std::fs::read_to_string(file)?;
    let module_path = file_to_module_path(file, project_root);
    parse_source(&source, lang, &module_path, file)
}

/// Parse a source string into symbol entries.
///
/// Why: Decouples parsing from disk I/O so tests can drive it with
/// in-memory strings.
/// What: Sets up a `tree_sitter::Parser` for `lang`, walks the tree,
/// matches node kinds against the language's symbol table, builds
/// `SymbolEntry`s with content hashes and inferred dependencies.
/// Test: `test_parse_source_rust_function`, `test_parse_source_rust_struct`.
pub fn parse_source(
    source: &str,
    lang: Language,
    module_path: &str,
    _file: &Path,
) -> Result<Vec<SymbolEntry>> {
    let mut parser = Parser::new();
    let ts_lang = match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
    };
    parser
        .set_language(&ts_lang)
        .map_err(|e| anyhow!("set_language failed: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("Parse failed"))?;

    let kinds = symbol_kinds_for_language(lang);
    let source_bytes = source.as_bytes();
    let mut entries = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut matched = false;
        for (kind_str, sym_kind) in kinds {
            if node.kind() == *kind_str {
                let name = node
                    .child_by_field_name("name")
                    .or_else(|| {
                        if node.kind() == "impl_item" {
                            node.child_by_field_name("type")
                        } else {
                            None
                        }
                    })
                    .map(|n| n.utf8_text(source_bytes).unwrap_or("").to_string())
                    .unwrap_or_else(|| {
                        node.utf8_text(source_bytes)
                            .unwrap_or("")
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    });
                if name.is_empty() {
                    matched = true;
                    break;
                }

                let sym_source = node.utf8_text(source_bytes)?.to_string();

                let actual_kind = if *sym_kind == SymbolKind::Function && lang == Language::Rust {
                    if is_test_function(&node, source_bytes) {
                        SymbolKind::Test
                    } else {
                        sym_kind.clone()
                    }
                } else {
                    sym_kind.clone()
                };

                let id = SymbolId::new(module_path, &name);
                let mut entry = SymbolEntry::new(id, actual_kind, sym_source, language_name(lang));
                entry.dependencies = infer_dependencies(&node, source_bytes, lang);

                entries.push(entry);
                matched = true;
                break;
            }
        }
        // Don't descend into matched containers — symbols are extracted at
        // the top container node and their full source is preserved verbatim.
        if !matched {
            let child_count = node.child_count();
            for i in (0..child_count).rev() {
                stack.push(node.child(i as u32).unwrap());
            }
        }
    }
    Ok(entries)
}

/// Heuristic: is this function tagged with `#[test]` / `#[tokio::test]`?
///
/// Why: Distinguishes test functions from production functions in Rust so
/// the registry can carry the `Test` kind (and later link to the symbol
/// under test via `test_covers`).
/// What: Inspects the function's own source text for `#[test]` markers —
/// good enough for the common case where the attribute sits directly on
/// the function.
/// Test: Indirectly via parsing.
fn is_test_function(node: &Node, source: &[u8]) -> bool {
    let node_text = node.utf8_text(source).unwrap_or("");
    node_text.contains("#[test]") || node_text.contains("#[tokio::test]")
}

/// Best-effort call-graph dependency inference.
///
/// Why: Even rough deps let the emitter topologically order symbols within
/// a file so callers tend to follow callees.
/// What: Walks the subtree looking for `call_expression` nodes and
/// extracts the simple textual form of the callee. Resolution to real
/// `SymbolId`s happens later (or not at all — extras get filtered out
/// during topological sort).
/// Test: Indirectly via emitter tests.
fn infer_dependencies(node: &Node, source: &[u8], _lang: Language) -> BTreeSet<SymbolId> {
    let mut deps = BTreeSet::new();
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if n.kind() == "call_expression"
            && let Some(func) = n.child_by_field_name("function")
        {
            let name = func.utf8_text(source).unwrap_or("").to_string();
            if !name.contains(' ') && !name.is_empty() {
                deps.insert(SymbolId(name));
            }
        }
        for i in (0..n.child_count()).rev() {
            stack.push(n.child(i as u32).unwrap());
        }
    }
    deps
}

/// Walk a directory and build a populated `SymbolRegistry`.
///
/// Why: Bulk indexing for the `--parse-to-registry` CLI flag.
/// What: Skips `target/` and `.git/`, parses each supported file, inserts
/// every emitted entry into the registry. Per-file errors are logged at
/// debug level and skipped (the build is best-effort).
/// Test: Indirect — exercised by the CLI flag.
pub fn parse_directory(dir: &Path, project_root: &Path) -> Result<SymbolRegistry> {
    let mut registry = SymbolRegistry::new(project_root.to_path_buf());
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() && detect_language(path).is_some() {
            let path_str = path.to_string_lossy();
            if path_str.contains("/target/") || path_str.contains("/.git/") {
                continue;
            }
            match parse_file(path, project_root) {
                Ok(entries) => {
                    for e in entries {
                        registry.insert(e);
                    }
                }
                Err(e) => {
                    tracing::debug!("parse_file skipped {}: {e}", path.display());
                }
            }
        }
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_to_module_path_normal() {
        let root = std::path::Path::new("/proj");
        let file = std::path::Path::new("/proj/src/api/handlers.rs");
        assert_eq!(file_to_module_path(file, root), "api::handlers");
    }

    #[test]
    fn test_file_to_module_path_main() {
        let root = std::path::Path::new("/proj");
        let file = std::path::Path::new("/proj/src/main.rs");
        assert_eq!(file_to_module_path(file, root), "");
    }

    #[test]
    fn test_file_to_module_path_mod() {
        let root = std::path::Path::new("/proj");
        let file = std::path::Path::new("/proj/src/api/mod.rs");
        assert_eq!(file_to_module_path(file, root), "api");
    }

    #[test]
    fn test_parse_source_rust_function() {
        let source = "fn hello_world() -> String { \"hello\".to_string() }";
        let entries = parse_source(
            source,
            Language::Rust,
            "mymod",
            std::path::Path::new("src/mymod.rs"),
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id.as_str(), "mymod::hello_world");
        assert_eq!(entries[0].language, "rust");
    }

    #[test]
    fn test_parse_source_java_class() {
        let source = r#"
public class LruCache<K, V> {
    public V get(K key) { return null; }
    public void put(K key, V value) {}
}
"#;
        let entries = parse_source(
            source,
            Language::Java,
            "com.example",
            Path::new("LruCache.java"),
        )
        .unwrap();
        let kinds: Vec<_> = entries.iter().map(|e| e.kind.clone()).collect();
        assert!(kinds.contains(&crate::registry::SymbolKind::Class));
    }

    #[test]
    fn test_parse_source_typescript_interface() {
        let source = r#"
interface UserSchema {
    id: number;
    name: string;
}
function validate(u: unknown): UserSchema {
    return u as UserSchema;
}
"#;
        let entries = parse_source(
            source,
            Language::TypeScript,
            "validator",
            Path::new("validator.ts"),
        )
        .unwrap();
        let kinds: Vec<_> = entries.iter().map(|e| e.kind.clone()).collect();
        assert!(kinds.contains(&crate::registry::SymbolKind::Trait));
        assert!(kinds.contains(&crate::registry::SymbolKind::Function));
    }

    #[test]
    fn test_parse_source_rust_struct() {
        let source = "pub struct Config { pub port: u16 }";
        let entries = parse_source(
            source,
            Language::Rust,
            "",
            std::path::Path::new("src/main.rs"),
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id.as_str(), "Config");
    }
}
