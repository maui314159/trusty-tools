//! Mode-based file-type filtering (issue #77, final design).
//!
//! Why: the prior implementation of #77 applied a 3x3 multiplicative penalty
//! matrix (file class x search mode) to demote off-target file types. In
//! practice the penalties still let prose/config leak into the top-k for
//! `code` mode whenever their raw BM25 score was high enough — CHANGELOG.md
//! routinely came back at rank 1. The revised design replaces the penalty
//! matrix with a **hard file-type filter**: each mode declares the set of
//! file types it returns, and chunks outside that set are dropped from the
//! result list entirely. No score distortion, no cross-contamination, no
//! tuning matrix to maintain.
//! What: pure path classifiers and a single [`is_allowed_for_mode`] predicate
//! that decides whether a chunk's file is in the allowed set for a given
//! [`SearchMode`]. The post-RRF pipeline (see `indexer::search`) calls this
//! once per result and drops chunks that don't match. `SearchMode::All`
//! short-circuits to `true` so the unfiltered behaviour is opt-in.
//!
//! ## Mode → allowed file types
//!
//! - `code`: source-code extensions (`.rs`, `.ts`, `.py`, `.go`, …) — the
//!   default. Strictly source files only.
//! - `text`: prose / documentation extensions (`.md`, `.rst`, `.txt`, …) plus
//!   path-based well-known docs (README*, CHANGELOG*, LICENSE*, NOTICE*,
//!   CONTRIBUTING*) regardless of extension. `.xml` is **not** in this set —
//!   it is assigned to `data` (structured markup).
//! - `data`: structured-data / config extensions (`.json`, `.yaml`, `.toml`,
//!   `.csv`, `.xml`, `.sql`, …). `.xml` and `.toml` live here.
//! - `all`: no filter — the predicate always returns `true`.
//!
//! Test: see the `tests` submodule.

use super::SearchMode;

/// Source-code file extensions for `SearchMode::Code`.
///
/// Why: matches every mainstream compiled / scripted language we expect to
/// see in a polyglot repo. Lowercased and including the leading dot to keep
/// the comparison cheap (`ends_with`).
/// `.sql` is intentionally included here even though it also appears in
/// [`DATA_EXTENSIONS`] — SQL files are executable logic (migrations, stored
/// procs, queries) and belong in `code` mode results just as much as in
/// `data` mode results. The two sets are independent: `is_allowed_for_mode`
/// dispatches on mode and checks only the relevant list.
/// What: a flat constant slice; no allocations at runtime.
/// Test: `test_code_mode_allows_source_extensions`, `test_sql_allowed_in_code_and_data`.
const CODE_EXTENSIONS: &[&str] = &[
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".go", ".java", ".c", ".cpp",
    ".cc", ".cxx", ".h", ".hpp", ".cs", ".rb", ".swift", ".kt", ".kts", ".scala", ".ex", ".exs",
    ".hs", ".ml", ".elm", ".zig", ".nim", ".v", ".sol", ".sh", ".bash", ".zsh", ".fish", ".ps1",
    ".lua", ".r", ".jl", ".dart", ".cr", ".clj", ".cljs", ".erl", ".fs", ".fsx", ".sql",
];

/// Prose / documentation file extensions for `SearchMode::Text`.
///
/// Why: prose docs are the *target* of `text` mode. `.xml` is intentionally
/// absent — see [`DATA_EXTENSIONS`] for the rationale.
/// What: lowercased extensions including the leading dot.
/// Test: `test_text_mode_allows_prose_extensions`.
const TEXT_EXTENSIONS: &[&str] = &[
    ".md",
    ".mdx",
    ".rst",
    ".txt",
    ".adoc",
    ".asciidoc",
    ".html",
    ".htm",
    ".tex",
    ".org",
    ".wiki",
    ".rtf",
];

/// Path-keyword prefixes (matched against the basename, case-insensitive) for
/// `SearchMode::Text`.
///
/// Why: many repos ship a `LICENSE` with no extension, `CHANGELOG` (no
/// extension), or `CONTRIBUTING.txt` / `.md`. The extension classifier alone
/// would miss the no-extension case, so we additionally check the basename
/// against a prefix list.
/// What: ASCII prefix match against the lowercased basename. Matched chunks
/// are admitted to `text` mode regardless of their extension.
/// Test: `test_text_mode_allows_named_docs_without_extension`.
const TEXT_NAME_PREFIXES: &[&str] = &["readme", "changelog", "license", "notice", "contributing"];

/// Structured-data / config / schema extensions for `SearchMode::Data`.
///
/// Why: structured data is the *target* of `data` mode. `.xml` and `.toml`
/// land here (not in `text`) because they are machine-readable markup /
/// config rather than prose. `.lock` is generic so it covers Cargo.lock,
/// poetry.lock, pnpm-lock.yaml (also matched by `.yaml`), etc.
/// What: lowercased extensions including the leading dot.
/// Test: `test_data_mode_allows_data_extensions`.
const DATA_EXTENSIONS: &[&str] = &[
    ".json", ".jsonl", ".ndjson", ".csv", ".tsv", ".psv", ".yaml", ".yml", ".toml", ".xml", ".xls",
    ".xlsx", ".ods", ".parquet", ".avro", ".arrow", ".proto", ".graphql", ".sql", ".db", ".sqlite",
    ".lock",
];

/// Lowercase the basename of a path once for prefix matching.
///
/// Why: the text-mode named-doc rule operates on the basename, not the full
/// path (so a directory called `license/` is not mistaken for a LICENSE
/// file).
/// What: returns the substring after the final `/` (or the whole path when
/// no `/` is present), lowercased.
/// Test: `test_text_mode_allows_named_docs_without_extension`.
fn basename_lower(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

/// Test whether `path` ends with any of `exts`. Extensions must include the
/// leading dot and be lowercased.
///
/// Why: shared by every mode classifier; avoids re-lowercasing the full path
/// once per extension.
/// What: lowercases `path` once and short-circuits on the first match.
/// Test: implicitly covered by every mode test.
fn has_extension(path: &str, exts: &[&str]) -> bool {
    let lower = path.to_ascii_lowercase();
    exts.iter().any(|ext| lower.ends_with(ext))
}

/// Decide whether a chunk's file is in the allowed set for the requested
/// search mode (issue #77, final design).
///
/// Why: post-RRF filtering — each mode returns ONLY chunks whose file type
/// is in its allowed bucket. Replaces the prior penalty matrix entirely.
/// What: dispatches on [`SearchMode`] and runs the matching extension /
/// name-prefix check. `SearchMode::All` short-circuits to `true`.
/// Test: see the per-mode tests in the `tests` submodule.
pub(crate) fn is_allowed_for_mode(chunk_file: &str, mode: SearchMode) -> bool {
    match mode {
        SearchMode::Code => has_extension(chunk_file, CODE_EXTENSIONS),
        SearchMode::Text => {
            if has_extension(chunk_file, TEXT_EXTENSIONS) {
                return true;
            }
            let bn = basename_lower(chunk_file);
            TEXT_NAME_PREFIXES.iter().any(|p| bn.starts_with(p))
        }
        SearchMode::Data => has_extension(chunk_file, DATA_EXTENSIONS),
        SearchMode::All => true,
    }
}

/// Compute the mode-aware multiplicative score penalty for a chunk file
/// (issue #77 final design — the penalty-matrix variant).
///
/// Why: the hard-filter classifier [`is_allowed_for_mode`] gives the
/// strongest signal but drops off-mode files entirely, which loses
/// useful context (e.g. a CHANGELOG entry that mentions the queried
/// symbol can still be relevant in `code` mode). The penalty-matrix
/// variant keeps every result but applies a multiplicative score
/// penalty so off-mode files sink in ranking without disappearing.
/// This function exists alongside `is_allowed_for_mode` so callers can
/// pick the appropriate strategy.
/// What: classifies the path into Source / Text / Data (using the same
/// extension lists as the hard-filter classifier) and returns the
/// multiplier from the 3x3 penalty matrix for the requested mode plus
/// an optional `archive_reason`-style label naming the matching bucket.
/// `SearchMode::All` is treated as `Code` for compatibility with the
/// existing default direction.
/// Test: see the `tests` submodule (`test_doc_score_penalty_*`).
#[allow(dead_code)] // Reserved for future score-based ranking; current pipeline uses is_allowed_for_mode.
pub(crate) fn doc_score_penalty(chunk_file: &str, mode: SearchMode) -> (f32, Option<String>) {
    // Penalty matrix (file class x mode):
    //                 code   text   data
    //  Source         1.0    0.5    0.3
    //  Text/Prose     0.1    1.0    0.3
    //  Data/Config    0.2    0.3    1.0
    let mode = match mode {
        SearchMode::All => SearchMode::Code,
        other => other,
    };

    // Classify: text wins over data wins over code (a `.md` file is text
    // even though it also tokenises like source).
    let is_text = has_extension(chunk_file, TEXT_EXTENSIONS) || {
        let bn = basename_lower(chunk_file);
        TEXT_NAME_PREFIXES.iter().any(|p| bn.starts_with(p))
    };
    let is_data = !is_text && has_extension(chunk_file, DATA_EXTENSIONS);
    let is_code = !is_text && !is_data && has_extension(chunk_file, CODE_EXTENSIONS);

    let (mult, reason) = if is_text {
        let m = match mode {
            SearchMode::Code => 0.1,
            SearchMode::Text => 1.0,
            SearchMode::Data => 0.3,
            SearchMode::All => 1.0,
        };
        (m, Some(format!("text:{}", basename_lower(chunk_file))))
    } else if is_data {
        let m = match mode {
            SearchMode::Code => 0.2,
            SearchMode::Text => 0.3,
            SearchMode::Data => 1.0,
            SearchMode::All => 1.0,
        };
        (m, Some(format!("data:{}", basename_lower(chunk_file))))
    } else if is_code {
        let m: f32 = match mode {
            SearchMode::Code => 1.0,
            SearchMode::Text => 0.5,
            SearchMode::Data => 0.3,
            SearchMode::All => 1.0,
        };
        // Source files only get a reason label when actually penalised.
        let reason = if (m - 1.0_f32).abs() > f32::EPSILON {
            Some(format!("source:{}", basename_lower(chunk_file)))
        } else {
            None
        };
        (m, reason)
    } else {
        // Unknown file type: leave score unchanged.
        (1.0_f32, None)
    };

    if (mult - 1.0_f32).abs() < f32::EPSILON {
        (1.0, None)
    } else {
        (mult, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- code mode -------------------------------------------------------

    #[test]
    fn test_code_mode_allows_source_extensions() {
        // Why: code mode must admit every mainstream source-file extension.
        for path in &[
            "src/main.rs",
            "src/lib/auth.ts",
            "components/Button.tsx",
            "pkg/handler.go",
            "app/views.py",
            "src/index.js",
            "src/index.mjs",
            "Main.java",
            "kernel.c",
            "engine.cpp",
            "include/header.h",
            "App.swift",
            "Module.kt",
            "lib.scala",
            "build.zig",
            "scripts/deploy.sh",
            "lib/util.lua",
            "app.rb",
            "Component.fs",
        ] {
            assert!(
                is_allowed_for_mode(path, SearchMode::Code),
                "{path}: expected to be allowed in code mode"
            );
        }
    }

    #[test]
    fn test_code_mode_rejects_prose_and_data() {
        // Why: hard filter — prose docs and config/data must not appear in
        // code-mode results, even when their BM25 score is high.
        for path in &[
            "README.md",
            "CHANGELOG.md",
            "docs/intro.rst",
            "guide.txt",
            "Cargo.toml",
            "package.json",
            "pnpm-lock.yaml",
            "schema.xml",
            "rates.csv",
            "LICENSE",
        ] {
            assert!(
                !is_allowed_for_mode(path, SearchMode::Code),
                "{path}: expected to be rejected in code mode"
            );
        }
    }

    // ---- text mode -------------------------------------------------------

    #[test]
    fn test_text_mode_allows_prose_extensions() {
        for path in &[
            "docs/intro.md",
            "docs/INTRO.MD",
            "guide.rst",
            "notes.txt",
            "manual.adoc",
            "docs/overview.html",
            "paper.tex",
            "diary.org",
        ] {
            assert!(
                is_allowed_for_mode(path, SearchMode::Text),
                "{path}: expected to be allowed in text mode"
            );
        }
    }

    #[test]
    fn test_text_mode_allows_named_docs_without_extension() {
        // Why: many repos ship LICENSE / CHANGELOG / NOTICE with no
        // extension. The basename-prefix rule must catch them.
        for path in &[
            "LICENSE",
            "CHANGELOG",
            "README",
            "NOTICE",
            "CONTRIBUTING",
            "docs/CHANGELOG.rst",
            "subdir/license-policy",
            "ReadMe",
        ] {
            assert!(
                is_allowed_for_mode(path, SearchMode::Text),
                "{path}: expected to be allowed in text mode"
            );
        }
    }

    #[test]
    fn test_text_mode_rejects_source_and_data() {
        for path in &[
            "src/main.rs",
            "src/lib/auth.ts",
            "pkg/handler.go",
            "Cargo.toml",
            "package.json",
            "config.yaml",
            "schema.xml",
            "rates.csv",
        ] {
            assert!(
                !is_allowed_for_mode(path, SearchMode::Text),
                "{path}: expected to be rejected in text mode"
            );
        }
    }

    // ---- data mode -------------------------------------------------------

    #[test]
    fn test_data_mode_allows_data_extensions() {
        for path in &[
            "Cargo.toml",
            "package.json",
            "data.jsonl",
            "config.yaml",
            "config.yml",
            "schema.xml",
            "rates.csv",
            "rates.TSV",
            "Cargo.lock",
            "pnpm-lock.yaml",
            "migration.sql",
            "schema.graphql",
            "service.proto",
            "data.parquet",
            "db.sqlite",
        ] {
            assert!(
                is_allowed_for_mode(path, SearchMode::Data),
                "{path}: expected to be allowed in data mode"
            );
        }
    }

    #[test]
    fn test_data_mode_rejects_source_and_prose() {
        for path in &[
            "src/main.rs",
            "src/lib/auth.ts",
            "pkg/handler.go",
            "README.md",
            "CHANGELOG.md",
            "LICENSE",
            "docs/intro.rst",
            "notes.txt",
        ] {
            assert!(
                !is_allowed_for_mode(path, SearchMode::Data),
                "{path}: expected to be rejected in data mode"
            );
        }
    }

    // ---- all mode --------------------------------------------------------

    #[test]
    fn test_all_mode_allows_everything() {
        // Why: `all` is the escape hatch — no filter, return whatever the
        // index produced.
        for path in &[
            "src/main.rs",
            "README.md",
            "Cargo.toml",
            "LICENSE",
            "rates.csv",
            "schema.xml",
            "weird-file-no-extension",
            "",
        ] {
            assert!(
                is_allowed_for_mode(path, SearchMode::All),
                "{path}: expected to be allowed in all mode"
            );
        }
    }

    // ---- xml routing -----------------------------------------------------

    #[test]
    fn test_xml_is_data_not_text() {
        // Why: `.xml` appears in both buckets historically; the spec assigns
        // it to `data` (structured markup) and excludes it from `text`.
        assert!(is_allowed_for_mode("schema.xml", SearchMode::Data));
        assert!(!is_allowed_for_mode("schema.xml", SearchMode::Text));
        assert!(!is_allowed_for_mode("schema.xml", SearchMode::Code));
    }

    #[test]
    fn test_toml_is_data_not_text() {
        // Why: `.toml` is structured config; spec routes it to `data`.
        assert!(is_allowed_for_mode("Cargo.toml", SearchMode::Data));
        assert!(!is_allowed_for_mode("Cargo.toml", SearchMode::Text));
        assert!(!is_allowed_for_mode("Cargo.toml", SearchMode::Code));
    }

    #[test]
    fn test_sql_allowed_in_code_and_data() {
        // Why: SQL is both executable logic (migrations, stored procs, queries)
        // and structured/queryable data — it belongs in both `code` and `data`
        // mode results. It is not prose, so `text` mode correctly excludes it.
        for path in &[
            "migrations/0001_init.sql",
            "db/schema.SQL",
            "queries/users.sql",
        ] {
            assert!(
                is_allowed_for_mode(path, SearchMode::Code),
                "{path}: expected to be allowed in code mode"
            );
            assert!(
                is_allowed_for_mode(path, SearchMode::Data),
                "{path}: expected to be allowed in data mode"
            );
            assert!(
                !is_allowed_for_mode(path, SearchMode::Text),
                "{path}: expected to be rejected in text mode"
            );
        }
    }
}
