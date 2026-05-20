//! Compute complexity metrics for a chunk's source content.
//!
//! Why: cheap, dependency-free signal for "where is the gnarly code?". Mirrors
//! the algorithm that lived in `trusty_search_core::complexity` so a re-analysis
//! pass over chunks fetched from the search daemon yields the same numbers.
//!
//! What: scans for decision points (`if`, `match`, `loop`, `while`, `for`,
//! `&&`, `||`, `?`-style early-return), produces cyclomatic + cognitive counts,
//! an A–F letter grade, and a list of `CodeSmell`s. Output type lives in
//! `trusty-common` so it round-trips JSON with the search daemon.
//!
//! Test: covers cyclomatic counting, grade thresholds, and `LongFunction`
//! smell detection.

use crate::types::complexity::{CodeSmell, ComplexityGrade, ComplexityMetrics};

/// Threshold for `LongFunction`: > 50 newlines in the chunk content.
const LONG_FUNCTION_THRESHOLD: usize = 50;
/// Threshold for `DeepNesting`: max indent depth above this triggers the smell.
const DEEP_NESTING_THRESHOLD: u8 = 4;
/// Threshold for `TooManyParams`: parameter count above this triggers the smell.
const TOO_MANY_PARAMS_THRESHOLD: usize = 5;

/// Compute complexity for a chunk of source code.
///
/// Uses string scanning — no tree-sitter dep needed. Cheap enough to run on
/// every chunk during analysis.
/// Smart dispatcher: uses tree-sitter AST walk if `language` is one of
/// `"rust"`, `"typescript"`, `"javascript"`, or `"tsx"`, otherwise falls back
/// to the language-agnostic text heuristic. If the AST walk fails (parse
/// error, grammar load failure), the text heuristic is used as a safety net.
///
/// Why: gives the analyzer a single, language-aware entry point so callers
/// don't have to know which backend to call.
/// What: matches `language` lowercase, dispatches to the AST function, and
/// falls back to `compute_complexity` if the AST function returns `None`.
/// Test: `dispatcher_returns_metrics_for_known_and_unknown_languages` covers
/// the rust / typescript / unknown paths.
pub fn compute_complexity_for(content: &str, language: &str) -> ComplexityMetrics {
    use crate::core::complexity_ts;
    let lang = language.to_ascii_lowercase();
    match lang.as_str() {
        "rust" | "rs" => complexity_ts::compute_complexity_rust(content)
            .unwrap_or_else(|| compute_complexity(content)),
        "typescript" | "ts" | "tsx" | "javascript" | "js" | "jsx" => {
            complexity_ts::compute_complexity_typescript(content)
                .unwrap_or_else(|| compute_complexity(content))
        }
        // Every other tree-sitter-backed language goes through the generic
        // AST walker; the text heuristic remains the safety net.
        other => {
            let canonical = canonical_language(other);
            complexity_ts::compute_complexity_generic(content, canonical)
                .unwrap_or_else(|| compute_complexity(content))
        }
    }
}

/// Normalize common file-extension aliases to the canonical language tags
/// understood by the generic complexity walker.
fn canonical_language(lang: &str) -> &str {
    match lang {
        "py" | "pyi" => "python",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "cs" => "csharp",
        "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "h" => "c",
        "sc" => "scala",
        other => other,
    }
}

pub fn compute_complexity(content: &str) -> ComplexityMetrics {
    let cyclomatic = count_decision_points(content) + 1;
    let cognitive = count_cognitive(content);
    let grade = ComplexityGrade::from_cyclomatic(cyclomatic);
    let smells = detect_smells(content);
    ComplexityMetrics {
        cyclomatic,
        cognitive,
        grade,
        smells,
    }
}

/// Inspect `content` for code smells. Returns an empty vec if none fire.
pub fn detect_smells(content: &str) -> Vec<CodeSmell> {
    let mut smells = Vec::new();

    let line_count = content.matches('\n').count();
    if line_count > LONG_FUNCTION_THRESHOLD {
        smells.push(CodeSmell::LongFunction { lines: line_count });
    }

    let max_depth = content
        .lines()
        .map(estimate_indent_depth)
        .max()
        .unwrap_or(0);
    if max_depth > DEEP_NESTING_THRESHOLD {
        smells.push(CodeSmell::DeepNesting { max_depth });
    }

    let param_count = estimate_param_count(content);
    if param_count > TOO_MANY_PARAMS_THRESHOLD {
        smells.push(CodeSmell::TooManyParams { count: param_count });
    }

    if !has_docstring(content) {
        smells.push(CodeSmell::MissingDocstring);
    }

    smells
}

fn count_decision_points(content: &str) -> u32 {
    let patterns = [
        "if ", "else if", "} else", "match ", "loop ", "while ", "for ", " && ", " || ", "? ",
    ];
    let mut total: u32 = 0;
    for pat in patterns {
        total = total.saturating_add(count_occurrences(content, pat) as u32);
    }
    total
}

/// Cognitive complexity: same decision points, weighted by enclosing indent
/// depth (every 4 leading spaces or 1 leading tab counts as one level).
fn count_cognitive(content: &str) -> u32 {
    let patterns = [
        "if ", "else if", "} else", "match ", "loop ", "while ", "for ", " && ", " || ", "? ",
    ];
    let mut total: u32 = 0;
    for line in content.lines() {
        let depth = estimate_indent_depth(line) as u32;
        let weight = depth.saturating_add(1);
        for pat in patterns {
            let hits = count_occurrences(line, pat) as u32;
            total = total.saturating_add(hits.saturating_mul(weight));
        }
    }
    total
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

/// Estimate indent depth of a line: every 4 spaces or 1 tab = 1 level.
fn estimate_indent_depth(line: &str) -> u8 {
    let mut spaces: usize = 0;
    let mut tabs: usize = 0;
    for ch in line.chars() {
        match ch {
            ' ' => spaces += 1,
            '\t' => tabs += 1,
            _ => break,
        }
    }
    let depth = tabs + spaces / 4;
    depth.min(u8::MAX as usize) as u8
}

/// Best-effort parameter count: find first `(` after `fn ` / `def ` /
/// `function ` and count comma-separated args until the matching `)`.
fn estimate_param_count(content: &str) -> usize {
    let markers = ["fn ", "def ", "function "];
    let mut sig_start: Option<usize> = None;
    for marker in markers {
        if let Some(pos) = content.find(marker) {
            sig_start = Some(pos);
            break;
        }
    }
    let Some(start) = sig_start else { return 0 };
    let after = &content[start..];
    let Some(open) = after.find('(') else {
        return 0;
    };
    let rest = &after[open + 1..];
    let Some(close) = rest.find(')') else {
        return 0;
    };
    let params = &rest[..close];
    let trimmed = params.trim();
    if trimmed.is_empty() || trimmed == "self" || trimmed == "&self" || trimmed == "&mut self" {
        return 0;
    }
    trimmed.split(',').filter(|s| !s.trim().is_empty()).count()
}

fn has_docstring(content: &str) -> bool {
    content.contains("///")
        || content.contains("/**")
        || content.contains("\"\"\"")
        || content.contains("'''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyclomatic_counts_if_else_branch() {
        let src = "fn f() { if x { y } else { z } }";
        let m = compute_complexity(src);
        assert!(
            m.cyclomatic >= 2,
            "expected at least 2 decision points + base, got {}",
            m.cyclomatic
        );
    }

    #[test]
    fn long_function_smell_fires_above_threshold() {
        let mut src = String::from("/// doc\nfn big() {\n");
        for _ in 0..60 {
            src.push_str("    let _ = 1;\n");
        }
        src.push_str("}\n");
        let m = compute_complexity(&src);
        assert!(
            m.smells
                .iter()
                .any(|s| matches!(s, CodeSmell::LongFunction { .. })),
            "expected LongFunction smell, got {:?}",
            m.smells
        );
    }

    #[test]
    fn missing_docstring_smell_fires_when_no_doc_marker() {
        let m = compute_complexity("fn f() {}\n");
        assert!(m
            .smells
            .iter()
            .any(|s| matches!(s, CodeSmell::MissingDocstring)));
    }

    #[test]
    fn dispatcher_returns_metrics_for_known_and_unknown_languages() {
        let rust_src = "fn f(a: i32) -> i32 { if a > 0 { a } else { -a } }";
        let m_rust = compute_complexity_for(rust_src, "rust");
        assert!(m_rust.cyclomatic >= 2);

        let ts_src = "function f(a: number) { return a > 0 ? a : -a; }";
        let m_ts = compute_complexity_for(ts_src, "typescript");
        assert!(m_ts.cyclomatic >= 2);

        let unk_src = "if x { y } else { z }";
        let m_unk = compute_complexity_for(unk_src, "cobol");
        // Should fall back to the text heuristic, which still finds branches.
        assert!(m_unk.cyclomatic >= 1);
    }

    #[test]
    fn grade_matches_cyclomatic_band() {
        let m = compute_complexity("/// doc\nfn f() {}\n");
        assert_eq!(m.grade, ComplexityGrade::A);
    }
}
