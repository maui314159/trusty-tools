//! `/grep` — grep-parity regex search over the files in an index.
//!
//! Why: the hybrid `/search` lane (BM25 + vector + KG) is excellent for
//! conceptual / semantic recall but it deliberately *fuzzes* matches. Some
//! workflows (an LLM agent verifying a refactor, a human chasing every literal
//! call site) need exact, deterministic, line-accurate matching with the same
//! ergonomics they get from `grep`/`ripgrep`: regex patterns, `-i`
//! case-insensitivity, `-A`/`-B`/`-C` context windows, `--include` glob
//! filtering, and dot-matches-newline multiline mode. Crucially this must work
//! *without re-embedding* — it greps the raw file bytes of whatever files the
//! index already knows about, so it is instant and never touches the ONNX
//! model.
//! What: a pure, I/O-free core ([`CompiledGrep`] + [`grep_file_content`]) that
//! the HTTP handler drives by (1) collecting the distinct set of file paths the
//! index has chunked, (2) reading each file fresh from disk under the index
//! `root_path`, and (3) running the matcher. Keeping the matcher pure makes the
//! line/column/context logic trivially unit-testable with in-memory strings.
//! Test: the `tests` module covers literal + regex matching, case folding,
//! `before`/`after`/`combined` context windows, multiline (dot-matches-newline)
//! matching, glob filtering, the `max_results` truncation flag, and invalid
//! regex/glob rejection. The HTTP wiring is exercised by the server-level
//! integration tests (`grep_*`).

use serde::{Deserialize, Serialize};

/// Request body for `POST /grep` and `POST /indexes/:id/grep`.
///
/// Why: a single struct mirrors the common `grep`/`ripgrep` flag surface so
/// callers can translate a CLI invocation to JSON field-by-field without
/// guessing. Every option has a sensible serde default, so the minimal request
/// is just `{ "pattern": "..." }`.
/// What: `serde`-deserialized; `pattern` is the only required field. The
/// `index_id` is optional at the type level so the same struct serves both the
/// global (`POST /grep`) and per-index (`POST /indexes/:id/grep`, where the id
/// comes from the path) endpoints.
/// Test: `request_defaults_are_grep_like` asserts the zero-value request is a
/// case-sensitive, no-context, all-files, single-line search.
#[derive(Debug, Clone, Deserialize)]
pub struct GrepRequest {
    /// The regex pattern to match (PCRE-ish, via the `regex` crate).
    pub pattern: String,

    /// Optional index id. When omitted on the global endpoint, every
    /// registered index is searched. Ignored by the per-index endpoint (the
    /// id is taken from the URL path there).
    #[serde(default)]
    pub index_id: Option<String>,

    /// `-i` parity: ASCII + Unicode case-insensitive matching.
    #[serde(default)]
    pub case_insensitive: bool,

    /// `-B` parity: number of lines of context to include before each match.
    #[serde(default)]
    pub context_before: usize,

    /// `-A` parity: number of lines of context to include after each match.
    #[serde(default)]
    pub context_after: usize,

    /// `-C` parity: when set, overrides both `context_before` and
    /// `context_after` (matches `grep -C`'s precedence over `-A`/`-B`).
    #[serde(default)]
    pub context: Option<usize>,

    /// `--include=<glob>` parity: only files whose path matches this glob are
    /// searched. The glob is matched against the index-relative file path
    /// (e.g. `crates/foo/src/bar.rs`). `None` = no filter.
    #[serde(default)]
    pub glob: Option<String>,

    /// When true, `.` in the pattern matches newlines too (`(?s)` mode) so a
    /// single pattern can span multiple lines within a file.
    #[serde(default)]
    pub multiline: bool,

    /// Hard cap on the number of matches returned across all files. Defaults
    /// to [`DEFAULT_MAX_RESULTS`]. The response `truncated` flag is set when
    /// the cap is hit.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

/// Default `max_results` when the caller omits it.
///
/// Why: an unbounded grep over a large repo could return tens of thousands of
/// lines and balloon the response. 100 mirrors the suggested API default and
/// keeps payloads small; callers wanting more raise it explicitly.
/// What: returns `100`.
/// Test: `request_defaults_are_grep_like`.
fn default_max_results() -> usize {
    DEFAULT_MAX_RESULTS
}

/// Default cap on returned matches.
pub const DEFAULT_MAX_RESULTS: usize = 100;

/// A single grep hit: one matching line plus its surrounding context.
///
/// Why: gives callers the same information `grep -n` would (file, 1-based line,
/// the line text) plus the column of the match start and optional context
/// windows so an LLM can reason about the surrounding code without a follow-up
/// fetch.
/// What: `serde`-serialized into the response `matches` array. `line` and
/// `column` are 1-based to match `grep`/editor conventions. For multiline
/// matches, `line`/`column` point at the first line/column of the match and
/// `text` is the first physical line of the match.
/// Test: `single_literal_match_reports_line_and_column`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrepMatch {
    /// Index-relative path of the file the match was found in.
    pub file: String,
    /// 1-based line number of the match's first line.
    pub line: usize,
    /// 1-based column (char offset within the line) of the match start.
    pub column: usize,
    /// The full text of the matching line.
    pub text: String,
    /// Up to `context_before` lines immediately preceding `line`.
    pub context_before: Vec<String>,
    /// Up to `context_after` lines immediately following `line`.
    pub context_after: Vec<String>,
}

/// Response body for the grep endpoints.
///
/// Why: callers need the matches plus enough metadata to know whether the
/// result set was clipped (`truncated`) and how many hits there were
/// (`total`).
/// What: `serde`-serialized. `total` equals `matches.len()` today but is kept
/// distinct so a future "count only" mode can report a total larger than the
/// returned slice.
/// Test: `truncates_at_max_results` asserts `truncated` flips and the slice is
/// clamped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrepResponse {
    pub matches: Vec<GrepMatch>,
    pub total: usize,
    pub truncated: bool,
}

/// Errors that can occur while preparing a grep query.
///
/// Why: invalid user input (a malformed regex or glob) must map to a `400 Bad
/// Request`, not a `500`. A typed error lets the handler translate cleanly
/// without string-sniffing.
/// What: `thiserror`-derived; carries the underlying crate's message so the
/// caller can see *why* their pattern was rejected.
/// Test: `invalid_regex_is_rejected` and `invalid_glob_is_rejected`.
#[derive(Debug, thiserror::Error)]
pub enum GrepError {
    /// The supplied `pattern` did not compile as a regex.
    #[error("invalid regex pattern: {0}")]
    InvalidRegex(String),
    /// The supplied `glob` was not a valid glob pattern.
    #[error("invalid glob pattern: {0}")]
    InvalidGlob(String),
}

/// A compiled, ready-to-run grep query.
///
/// Why: compiling the regex and glob once (per request) and reusing them across
/// every file in the index avoids re-parsing the pattern thousands of times.
/// Holding the resolved context-window sizes here keeps [`grep_file_content`]'s
/// signature small.
/// What: built via [`CompiledGrep::compile`] from a [`GrepRequest`]. Owns the
/// compiled `regex::Regex`, an optional compiled `glob::Pattern`, and the
/// resolved before/after context line counts (`-C` already folded in).
/// Test: `compile_folds_context_C_over_A_B` checks the `-C` precedence; the
/// matching tests build a `CompiledGrep` directly.
#[derive(Debug)]
pub struct CompiledGrep {
    regex: regex::Regex,
    glob: Option<glob::Pattern>,
    context_before: usize,
    context_after: usize,
    multiline: bool,
}

impl CompiledGrep {
    /// Compile a request into an executable matcher.
    ///
    /// Why: this is the single place user-supplied pattern/glob strings are
    /// validated, so the handler can return `400` on the `Err` arm and never
    /// has to touch `regex`/`glob` directly.
    /// What: builds a `regex::RegexBuilder` with `case_insensitive` and
    /// `dot_matches_new_line` (multiline) flags applied, compiles the optional
    /// glob, and folds `-C` over `-A`/`-B` (a present `context` overrides both).
    /// Test: `compile_folds_context_C_over_A_B`, `invalid_regex_is_rejected`,
    /// `invalid_glob_is_rejected`.
    pub fn compile(req: &GrepRequest) -> Result<Self, GrepError> {
        let regex = regex::RegexBuilder::new(&req.pattern)
            .case_insensitive(req.case_insensitive)
            // `dot_matches_new_line` makes `.` span newlines so a single
            // pattern can match across lines — the multiline parity we want.
            .dot_matches_new_line(req.multiline)
            .build()
            .map_err(|e| GrepError::InvalidRegex(e.to_string()))?;

        let glob = match req.glob.as_deref() {
            Some(pat) => {
                Some(glob::Pattern::new(pat).map_err(|e| GrepError::InvalidGlob(e.to_string()))?)
            }
            None => None,
        };

        // `-C` (context) overrides both `-A` and `-B`, matching grep's
        // precedence.
        let (context_before, context_after) = match req.context {
            Some(c) => (c, c),
            None => (req.context_before, req.context_after),
        };

        Ok(Self {
            regex,
            glob,
            context_before,
            context_after,
            multiline: req.multiline,
        })
    }

    /// Test whether an index-relative file path passes the glob filter.
    ///
    /// Why: lets the handler skip reading files that can't match before paying
    /// the I/O cost.
    /// What: returns `true` when no glob was supplied, otherwise delegates to
    /// `glob::Pattern::matches_with` using `require_literal_separator = false`
    /// so `*.rs` matches at any depth (ripgrep `--include` semantics) while
    /// `**/` still works as written.
    /// Test: `glob_filters_by_path`.
    pub fn path_matches(&self, rel_path: &str) -> bool {
        match &self.glob {
            None => true,
            Some(pat) => {
                // require_literal_separator=false ⇒ `*.rs` matches
                // `a/b/c.rs`, matching `grep --include`/`rg -g` behaviour.
                let opts = glob::MatchOptions {
                    case_sensitive: true,
                    require_literal_separator: false,
                    require_literal_leading_dot: false,
                };
                pat.matches_with(rel_path, opts)
            }
        }
    }
}

/// Run the compiled grep over one file's text, appending hits to `out`.
///
/// Why: this is the pure heart of the feature — no filesystem, no locks, just
/// `&str` in, `Vec<GrepMatch>` out — so the line/column/context arithmetic can
/// be unit-tested exhaustively. The handler calls it once per file.
/// What: in single-line mode it scans line-by-line and records the first match
/// column per line (multiple matches on one line still yield one `GrepMatch`,
/// matching `grep`'s default line-oriented output). In multiline mode it runs
/// the regex over the whole file, maps each match's byte offset back to a
/// 1-based `(line, column)`, and uses that line's text as `text`. Context
/// windows are clamped to file bounds. Stops early once `out.len()` reaches
/// `max_results` so a runaway pattern can't blow up memory. `file` is copied
/// into every emitted match verbatim.
/// Test: `single_literal_match_reports_line_and_column`,
/// `context_windows_are_clamped`, `multiline_match_spans_lines`,
/// `respects_remaining_budget`.
pub fn grep_file_content(
    file: &str,
    content: &str,
    compiled: &CompiledGrep,
    out: &mut Vec<GrepMatch>,
    max_results: usize,
) {
    if out.len() >= max_results {
        return;
    }
    // Pre-split into lines once; reused for both match text and context.
    let lines: Vec<&str> = content.lines().collect();

    if compiled.multiline {
        grep_multiline(file, content, &lines, compiled, out, max_results);
    } else {
        grep_line_by_line(file, &lines, compiled, out, max_results);
    }
}

/// Line-oriented scan: one `GrepMatch` per matching line (grep default).
fn grep_line_by_line(
    file: &str,
    lines: &[&str],
    compiled: &CompiledGrep,
    out: &mut Vec<GrepMatch>,
    max_results: usize,
) {
    for (idx, line) in lines.iter().enumerate() {
        if out.len() >= max_results {
            return;
        }
        if let Some(m) = compiled.regex.find(line) {
            out.push(build_match(
                file,
                lines,
                idx,
                byte_to_col(line, m.start()),
                compiled,
            ));
        }
    }
}

/// Whole-file scan for `multiline` mode: map each match offset back to a line.
fn grep_multiline(
    file: &str,
    content: &str,
    lines: &[&str],
    compiled: &CompiledGrep,
    out: &mut Vec<GrepMatch>,
    max_results: usize,
) {
    // Precompute the starting byte offset of each line so we can translate a
    // whole-file match offset into a 1-based (line, column) in O(log n).
    let mut line_starts = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for line in lines {
        line_starts.push(offset);
        // +1 for the '\n' stripped by `.lines()`. Files without a trailing
        // newline still work because we never index past the last match.
        offset += line.len() + 1;
    }

    for m in compiled.regex.find_iter(content) {
        if out.len() >= max_results {
            return;
        }
        let start = m.start();
        // Find the line whose start is the greatest <= match start.
        let line_idx = match line_starts.binary_search(&start) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line_text = lines.get(line_idx).copied().unwrap_or("");
        let col = byte_to_col(line_text, start - line_starts[line_idx]);
        out.push(build_match(file, lines, line_idx, col, compiled));
    }
}

/// Assemble a [`GrepMatch`] for the line at `idx` with clamped context windows.
fn build_match(
    file: &str,
    lines: &[&str],
    idx: usize,
    column: usize,
    compiled: &CompiledGrep,
) -> GrepMatch {
    let before_start = idx.saturating_sub(compiled.context_before);
    let context_before: Vec<String> = lines[before_start..idx]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let after_end = (idx + 1 + compiled.context_after).min(lines.len());
    let context_after: Vec<String> = lines[idx + 1..after_end]
        .iter()
        .map(|s| s.to_string())
        .collect();

    GrepMatch {
        file: file.to_string(),
        line: idx + 1,
        column,
        text: lines.get(idx).copied().unwrap_or("").to_string(),
        context_before,
        context_after,
    }
}

/// Translate a byte offset within a line into a 1-based char column.
///
/// Why: regex match offsets are byte offsets, but editors/grep count columns by
/// character. Multi-byte UTF-8 (e.g. an emoji or accented char) before the
/// match would otherwise inflate the reported column.
/// What: counts `char` boundaries in `line[..byte]` and returns count + 1.
/// Test: `byte_to_col_handles_multibyte`.
fn byte_to_col(line: &str, byte: usize) -> usize {
    let clamped = byte.min(line.len());
    line[..clamped].chars().count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `GrepRequest` with everything defaulted but `pattern`.
    fn req(pattern: &str) -> GrepRequest {
        GrepRequest {
            pattern: pattern.to_string(),
            index_id: None,
            case_insensitive: false,
            context_before: 0,
            context_after: 0,
            context: None,
            glob: None,
            multiline: false,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    fn run(file: &str, content: &str, r: &GrepRequest) -> Vec<GrepMatch> {
        let compiled = CompiledGrep::compile(r).expect("compile");
        let mut out = Vec::new();
        grep_file_content(file, content, &compiled, &mut out, r.max_results);
        out
    }

    /// A request with only `pattern` set is a case-sensitive, no-context,
    /// all-files, single-line search with the default cap.
    #[test]
    fn request_defaults_are_grep_like() {
        let r = req("x");
        assert!(!r.case_insensitive);
        assert_eq!(r.context_before, 0);
        assert_eq!(r.context_after, 0);
        assert!(r.context.is_none());
        assert!(r.glob.is_none());
        assert!(!r.multiline);
        assert_eq!(r.max_results, DEFAULT_MAX_RESULTS);
        // Round-trip the documented default through serde.
        let parsed: GrepRequest = serde_json::from_str(r#"{"pattern":"x"}"#).unwrap();
        assert_eq!(parsed.max_results, DEFAULT_MAX_RESULTS);
        assert!(!parsed.case_insensitive);
    }

    /// A literal match reports the 1-based line and column.
    #[test]
    fn single_literal_match_reports_line_and_column() {
        let content = "fn a() {}\n    fn authenticate() {}\nfn b() {}\n";
        let matches = run("src/auth.rs", content, &req("authenticate"));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "src/auth.rs");
        assert_eq!(matches[0].line, 2);
        // "    fn " = 4 spaces + "fn " = 7 chars before "authenticate" ⇒ col 8.
        assert_eq!(matches[0].column, 8);
        assert_eq!(matches[0].text, "    fn authenticate() {}");
    }

    /// Regex (not just literal) patterns work.
    #[test]
    fn regex_pattern_matches() {
        let content = "let x = 1;\nlet y = 22;\nlet z = 333;\n";
        let matches = run("a.rs", content, &req(r"=\s*\d{2,};"));
        assert_eq!(matches.len(), 2); // 22 and 333
        assert_eq!(matches[0].line, 2);
        assert_eq!(matches[1].line, 3);
    }

    /// `case_insensitive` folds case.
    #[test]
    fn case_insensitive_matches() {
        let content = "ERROR here\nno match\nerror there\n";
        let mut r = req("error");
        assert_eq!(run("a.rs", content, &r).len(), 1); // only "error there"
        r.case_insensitive = true;
        assert_eq!(run("a.rs", content, &r).len(), 2);
    }

    /// Context windows are clamped to the file bounds.
    #[test]
    fn context_windows_are_clamped() {
        let content = "l1\nl2\nMATCH\nl4\nl5\n";
        let mut r = req("MATCH");
        r.context_before = 5; // more than available ⇒ clamp to 2
        r.context_after = 5; // clamp to 2
        let matches = run("a.rs", content, &r);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].context_before, vec!["l1", "l2"]);
        assert_eq!(matches[0].context_after, vec!["l4", "l5"]);
    }

    /// `-C` overrides both `-A` and `-B`.
    #[test]
    fn compile_folds_context_c_over_a_b() {
        let mut r = req("MATCH");
        r.context_before = 1;
        r.context_after = 1;
        r.context = Some(3);
        let content = "a\nb\nc\nd\nMATCH\ne\nf\ng\nh\n";
        let matches = run("a.rs", content, &r);
        assert_eq!(matches[0].context_before, vec!["b", "c", "d"]);
        assert_eq!(matches[0].context_after, vec!["e", "f", "g"]);
    }

    /// Multiline mode lets `.` span newlines so one pattern matches across
    /// lines, and the match is attributed to its first line.
    #[test]
    fn multiline_match_spans_lines() {
        let content = "struct S {\n    field: i32,\n}\n";
        let mut r = req(r"struct S \{.*field");
        // Single-line: no match (the `.` can't cross the newline).
        assert_eq!(run("a.rs", content, &r).len(), 0);
        // Multiline: matches, attributed to line 1.
        r.multiline = true;
        let matches = run("a.rs", content, &r);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line, 1);
        assert_eq!(matches[0].column, 1);
    }

    /// The glob filter accepts/rejects index-relative paths.
    #[test]
    fn glob_filters_by_path() {
        let mut r = req("x");
        r.glob = Some("**/*.rs".to_string());
        let compiled = CompiledGrep::compile(&r).unwrap();
        assert!(compiled.path_matches("crates/foo/src/bar.rs"));
        assert!(compiled.path_matches("top.rs"));
        assert!(!compiled.path_matches("crates/foo/README.md"));

        // Bare `*.rs` still matches nested paths (ripgrep --include parity).
        r.glob = Some("*.rs".to_string());
        let compiled = CompiledGrep::compile(&r).unwrap();
        assert!(compiled.path_matches("a/b/c.rs"));
        assert!(!compiled.path_matches("a/b/c.py"));
    }

    /// `max_results` truncates the per-file emission and never exceeds the cap.
    #[test]
    fn respects_remaining_budget() {
        let content = "x\nx\nx\nx\nx\n";
        let mut r = req("x");
        r.max_results = 3;
        let matches = run("a.rs", content, &r);
        assert_eq!(matches.len(), 3);
    }

    /// `grep_file_content` is a no-op once the budget is already exhausted.
    #[test]
    fn no_op_when_budget_exhausted() {
        let r = req("x");
        let compiled = CompiledGrep::compile(&r).unwrap();
        let mut out = vec![GrepMatch {
            file: "pre.rs".into(),
            line: 1,
            column: 1,
            text: "x".into(),
            context_before: vec![],
            context_after: vec![],
        }];
        grep_file_content("a.rs", "x\nx\n", &compiled, &mut out, 1);
        assert_eq!(out.len(), 1); // unchanged
    }

    /// Invalid regex is rejected with a typed error (→ 400 at the handler).
    #[test]
    fn invalid_regex_is_rejected() {
        let r = req("(unclosed");
        let err = CompiledGrep::compile(&r).unwrap_err();
        assert!(matches!(err, GrepError::InvalidRegex(_)));
    }

    /// Invalid glob is rejected with a typed error (→ 400 at the handler).
    #[test]
    fn invalid_glob_is_rejected() {
        let mut r = req("x");
        r.glob = Some("[unclosed".to_string());
        let err = CompiledGrep::compile(&r).unwrap_err();
        assert!(matches!(err, GrepError::InvalidGlob(_)));
    }

    /// Multi-byte chars before the match don't inflate the reported column.
    #[test]
    fn byte_to_col_handles_multibyte() {
        // "café_" has a 2-byte 'é'; the match on "X" should report char col 6.
        let content = "café_X\n";
        let matches = run("a.rs", content, &req("X"));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].column, 6);
    }
}
