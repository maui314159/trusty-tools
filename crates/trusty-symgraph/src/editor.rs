//! AST-aware editing primitives (#347).
//!
//! Why: Free-form `write_file` invites whole-file rewrites that explode token
//! cost and risk regressions. Surgical primitives — replace-by-symbol-name,
//! insert-after-anchor, add-import — let the LLM emit the smallest possible
//! change while we keep authority over splice points and syntax validation.
//! What: `Patch` (id + file + before/after + unified diff), four edit
//! constructors that produce a `Patch` without touching disk, plus
//! `validate_syntax` and `apply_patch`. Each constructor parses the file with
//! tree-sitter, locates the splice range, validates the modified source, and
//! returns the `Patch` for the caller (or a tool's `PatchStore`) to apply
//! later.
//! Test: `replace_symbol_round_trips`, `validate_syntax_*`, `emit_diff_*`,
//! `add_import_skips_duplicates` cover the core paths.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use tree_sitter::{Language, Node, Parser};
use uuid::Uuid;

use crate::symbol::{detect_language, extract_symbols};

/// One pending edit produced by an AST tool.
///
/// Why: Tools return `Patch` objects to the orchestrator instead of mutating
/// the filesystem directly. The orchestrator (or an explicit `apply_patch`
/// call) decides when to commit, so the LLM can review the diff first.
/// What: `id` is a uuid for store lookup; `original`/`modified` are full file
/// contents; `diff` is a unified-diff string.
/// Test: Built and round-tripped in `replace_symbol_round_trips`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Patch {
    pub id: String,
    pub file: PathBuf,
    pub original: String,
    pub modified: String,
    pub diff: String,
}

/// Splice the source of a named symbol with `new_source`.
///
/// Why: The most common edit shape — replace a function body without re-
/// emitting the rest of the file. Caller passes the symbol name; we locate
/// it via `extract_symbols` and splice the byte range.
/// What: Reads `file`, finds the first symbol with `name`, splices, validates
/// the modified source, returns a `Patch`. Disk is untouched.
/// Test: `replace_symbol_round_trips` swaps a function body and asserts the
/// modified source still parses.
pub fn replace_symbol(file: &Path, name: &str, new_source: &str) -> Result<Patch> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let (lang, _) = detect_language(file)
        .with_context(|| format!("unsupported file extension: {}", file.display()))?;
    let symbols = extract_symbols(&source, lang.clone(), file);
    let sym = symbols
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow!("symbol '{name}' not found in {}", file.display()))?;

    let mut modified = String::with_capacity(source.len() + new_source.len());
    modified.push_str(&source[..sym.start_byte]);
    modified.push_str(new_source);
    modified.push_str(&source[sym.end_byte..]);

    if let Err(e) = validate_syntax(&modified, lang) {
        return Err(anyhow!(
            "modified source has syntax errors after replacing '{name}': {e}"
        ));
    }

    let diff = emit_diff(
        &source,
        &modified,
        file.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
    );
    Ok(Patch {
        id: Uuid::new_v4().to_string(),
        file: file.to_path_buf(),
        original: source,
        modified,
        diff,
    })
}

/// Insert `new_source` immediately after `anchor`'s closing byte.
///
/// Why: Adds a new function/struct adjacent to a known symbol without
/// disturbing imports or surrounding scope.
/// What: Locates `anchor` via `extract_symbols`, splices `\n\n<new_source>`
/// after its `end_byte`, validates, returns a `Patch`.
/// Test: Implicit via the editor tests; the splice mechanics are identical
/// to `replace_symbol`.
pub fn insert_after_symbol(file: &Path, anchor: &str, new_source: &str) -> Result<Patch> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let (lang, _) = detect_language(file)
        .with_context(|| format!("unsupported file extension: {}", file.display()))?;
    let symbols = extract_symbols(&source, lang.clone(), file);
    let sym = symbols
        .into_iter()
        .find(|s| s.name == anchor)
        .ok_or_else(|| anyhow!("anchor symbol '{anchor}' not found in {}", file.display()))?;

    let mut modified = String::with_capacity(source.len() + new_source.len() + 2);
    modified.push_str(&source[..sym.end_byte]);
    modified.push_str("\n\n");
    modified.push_str(new_source);
    modified.push_str(&source[sym.end_byte..]);

    if let Err(e) = validate_syntax(&modified, lang) {
        return Err(anyhow!(
            "modified source has syntax errors after inserting after '{anchor}': {e}"
        ));
    }

    let diff = emit_diff(
        &source,
        &modified,
        file.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
    );
    Ok(Patch {
        id: Uuid::new_v4().to_string(),
        file: file.to_path_buf(),
        original: source,
        modified,
        diff,
    })
}

/// Add an import statement at the language-appropriate location.
///
/// Why: Imports cluster at the top of files; LLM-generated whole-file rewrites
/// are wasteful when only one import needs to land. This tool handles the
/// language-specific placement (after the last existing import / at top).
/// What: Reads `file`, scans for existing imports. If `import_stmt` already
/// appears verbatim, returns a no-op `Patch` (original == modified).
/// Otherwise inserts after the last existing import line, or at the top of
/// the file when none are present. Validates, returns a `Patch`.
/// Test: `add_import_skips_duplicates`, `add_import_inserts_at_top`.
pub fn add_import(file: &Path, import_stmt: &str) -> Result<Patch> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let (lang, lang_tag) = detect_language(file)
        .with_context(|| format!("unsupported file extension: {}", file.display()))?;

    // Duplicate-skip: simple substring check.
    if source.contains(import_stmt.trim()) {
        let diff = emit_diff(
            &source,
            &source,
            file.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
        );
        return Ok(Patch {
            id: Uuid::new_v4().to_string(),
            file: file.to_path_buf(),
            original: source.clone(),
            modified: source,
            diff,
        });
    }

    // Find the byte offset of the line just past the last import.
    let import_prefix: &[&str] = match lang_tag {
        "rust" => &["use "],
        "python" => &["import ", "from "],
        "javascript" => &["import "],
        "go" => &["import "],
        _ => &[],
    };
    let mut insert_at: usize = 0;
    let mut byte_pos: usize = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if import_prefix.iter().any(|p| trimmed.starts_with(p)) {
            insert_at = byte_pos + line.len();
        }
        byte_pos += line.len();
    }

    let mut to_insert = String::new();
    to_insert.push_str(import_stmt.trim_end());
    to_insert.push('\n');

    let mut modified = String::with_capacity(source.len() + to_insert.len());
    modified.push_str(&source[..insert_at]);
    modified.push_str(&to_insert);
    modified.push_str(&source[insert_at..]);

    if let Err(e) = validate_syntax(&modified, lang) {
        return Err(anyhow!(
            "modified source has syntax errors after adding import '{import_stmt}': {e}"
        ));
    }

    let diff = emit_diff(
        &source,
        &modified,
        file.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
    );
    Ok(Patch {
        id: Uuid::new_v4().to_string(),
        file: file.to_path_buf(),
        original: source,
        modified,
        diff,
    })
}

/// Parse `source` with the given language and return Ok if the parse tree
/// has no error nodes.
///
/// Why: Every editor primitive validates before returning a `Patch` so we
/// never persist a broken file. Exposed publicly for tools to reuse.
/// What: Walks the parsed tree; collects up to a few error-node positions
/// and returns them as a single error string.
/// Test: `validate_syntax_ok`, `validate_syntax_err`.
pub fn validate_syntax(source: &str, lang: Language) -> Result<(), String> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("set_language: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "parser returned no tree".to_string())?;
    let root = tree.root_node();
    if !root.has_error() {
        return Ok(());
    }
    let mut errors: Vec<String> = Vec::new();
    collect_errors(root, source.as_bytes(), &mut errors);
    if errors.is_empty() {
        // has_error() set but no specific node found — surface a generic msg.
        return Err("parse tree contains errors".to_string());
    }
    Err(errors.join("; "))
}

fn collect_errors(node: Node, _bytes: &[u8], out: &mut Vec<String>) {
    if out.len() >= 5 {
        return;
    }
    if node.is_error() || node.is_missing() {
        let pos = node.start_position();
        out.push(format!(
            "syntax error at line {}, col {} ({})",
            pos.row + 1,
            pos.column + 1,
            node.kind()
        ));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(child, _bytes, out);
    }
}

/// Build a unified diff between `original` and `modified`.
///
/// Why: The LLM (and the user) want a compact view of what an edit changes.
/// Using `similar` keeps us out of the diff-format business while producing
/// a standard `+`/`-` representation.
/// What: Constructs a `TextDiff::from_lines` and renders unified format with
/// `a/<filename>` / `b/<filename>` headers.
/// Test: `emit_diff_contains_plus_minus`.
pub fn emit_diff(original: &str, modified: &str, filename: &str) -> String {
    let diff = TextDiff::from_lines(original, modified);
    let mut out = String::new();
    let header = format!("--- a/{filename}\n+++ b/{filename}\n");
    out.push_str(&header);
    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        out.push_str(&format!("{hunk}"));
    }
    out
}

/// Write `patch.modified` to `patch.file`. The only filesystem-mutating
/// helper; everything else is read-only.
pub fn apply_patch(patch: &Patch) -> Result<()> {
    std::fs::write(&patch.file, &patch.modified)
        .with_context(|| format!("failed to write {}", patch.file.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_tmp(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn validate_syntax_ok() {
        let src = "fn main() { let x = 1; }\n";
        assert!(validate_syntax(src, tree_sitter_rust::LANGUAGE.into()).is_ok());
    }

    #[test]
    fn validate_syntax_err() {
        let src = "fn main( { let x = ; }\n";
        let r = validate_syntax(src, tree_sitter_rust::LANGUAGE.into());
        assert!(r.is_err(), "expected syntax error, got {:?}", r);
    }

    #[test]
    fn emit_diff_contains_plus_minus() {
        let a = "line one\nline two\nline three\n";
        let b = "line one\nline TWO\nline three\n";
        let d = emit_diff(a, b, "test.txt");
        assert!(d.contains("-line two"), "diff missing - line: {d}");
        assert!(d.contains("+line TWO"), "diff missing + line: {d}");
    }

    #[test]
    fn replace_symbol_round_trips() {
        let dir = tempdir().unwrap();
        let path = write_tmp(
            dir.path(),
            "x.rs",
            "fn foo() -> i32 { 1 }\n\nfn bar() -> i32 { 2 }\n",
        );
        let patch = replace_symbol(&path, "foo", "fn foo() -> i32 { 42 }").unwrap();
        assert_ne!(patch.original, patch.modified);
        assert!(patch.modified.contains("42"));
        assert!(!patch.diff.is_empty());
        // Modified source still parses.
        assert!(validate_syntax(&patch.modified, tree_sitter_rust::LANGUAGE.into()).is_ok());
    }

    #[test]
    fn add_import_skips_duplicates() {
        let dir = tempdir().unwrap();
        let body = "use std::io;\n\nfn main() {}\n";
        let path = write_tmp(dir.path(), "x.rs", body);
        let patch = add_import(&path, "use std::io;").unwrap();
        assert_eq!(
            patch.original, patch.modified,
            "duplicate import should noop"
        );
    }

    #[test]
    fn add_import_inserts_after_existing() {
        let dir = tempdir().unwrap();
        let body = "use std::io;\n\nfn main() {}\n";
        let path = write_tmp(dir.path(), "x.rs", body);
        let patch = add_import(&path, "use std::fs;").unwrap();
        assert!(patch.modified.contains("use std::fs;"));
        assert!(patch.modified.contains("use std::io;"));
        // fs comes after io
        let pos_io = patch.modified.find("use std::io;").unwrap();
        let pos_fs = patch.modified.find("use std::fs;").unwrap();
        assert!(pos_fs > pos_io, "fs should be inserted after io");
    }
}
