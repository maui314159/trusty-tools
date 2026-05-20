//! `ClangtidyTool` — C/C++ diagnostics via `clang-tidy`.
//!
//! Why: clang-tidy is the standard Clang-based linter; it has no JSON output
//! mode, so we parse its line-oriented diagnostic format.
//! What: runs `clang-tidy <file> --` and parses stderr lines of the form
//! `<file>:<line>:<col>: <severity>: <message> [<check>]`.
//! Test: `parse_clangtidy_line_extracts_warning` parses a captured line.

use std::path::Path;

use anyhow::Result;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// C/C++ static-analysis tool backed by `clang-tidy`.
pub struct ClangtidyTool;

impl StaticTool for ClangtidyTool {
    fn name(&self) -> &str {
        "clang-tidy"
    }

    fn language(&self) -> &str {
        // Tree-sitter detection collapses C and C++ to the `cpp` tag, so we
        // register under `cpp` to cover both extensions.
        "cpp"
    }

    fn is_available(&self) -> bool {
        which::which("clang-tidy").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command("clang-tidy", &[&path, "--"], dir) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("clang-tidy invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        // clang-tidy writes diagnostics to stderr; some builds echo to stdout.
        let combined = format!("{}\n{}", out.stdout, out.stderr);
        Ok(parse_clangtidy_output(&combined))
    }
}

/// Parse clang-tidy's textual diagnostic lines into diagnostics.
fn parse_clangtidy_output(text: &str) -> Vec<ToolDiagnostic> {
    text.lines().filter_map(parse_clangtidy_line).collect()
}

/// Parse a single clang-tidy diagnostic line.
///
/// Expected shape: `path:line:col: severity: message [check-name]`.
fn parse_clangtidy_line(line: &str) -> Option<ToolDiagnostic> {
    // Split off the `path:line:col` prefix. The path itself may contain
    // colons on Windows, but our daemon runs on POSIX paths, so we take the
    // first three colon-delimited fields after the path.
    let mut parts = line.splitn(4, ':');
    let file = parts.next()?.trim();
    let line_no: u32 = parts.next()?.trim().parse().ok()?;
    let col: u32 = parts.next()?.trim().parse().ok()?;
    let rest = parts.next()?.trim();

    // `rest` is `severity: message [check]`.
    let (severity_str, after) = rest.split_once(':')?;
    let severity = match severity_str.trim() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "note" => return None,
        _ => Severity::Info,
    };

    let after = after.trim();
    let (message, code) = match (after.rfind('['), after.ends_with(']')) {
        (Some(idx), true) => {
            let msg = after[..idx].trim().to_string();
            let code = after[idx + 1..after.len() - 1].trim().to_string();
            (msg, Some(code))
        }
        _ => (after.to_string(), None),
    };

    Some(ToolDiagnostic {
        tool: "clang-tidy".into(),
        file: file.to_string(),
        line: line_no,
        col,
        severity,
        code,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clangtidy_line_extracts_warning() {
        let line =
            "src/main.c:12:5: warning: variable 'x' is unused [clang-diagnostic-unused-variable]";
        let d = parse_clangtidy_line(line).expect("should parse");
        assert_eq!(d.file, "src/main.c");
        assert_eq!(d.line, 12);
        assert_eq!(d.col, 5);
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.code.as_deref(), Some("clang-diagnostic-unused-variable"));
        assert_eq!(d.message, "variable 'x' is unused");
    }

    #[test]
    fn parse_clangtidy_line_skips_notes_and_noise() {
        assert!(parse_clangtidy_line("src/main.c:1:1: note: expanded from here").is_none());
        assert!(parse_clangtidy_line("random log line").is_none());
        assert!(parse_clangtidy_line("").is_none());
    }

    #[test]
    fn parse_clangtidy_output_collects_multiple() {
        let text = "a.c:1:1: warning: w1 [check-a]\na.c:2:2: error: e1 [check-b]\nnoise\n";
        let diags = parse_clangtidy_output(text);
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[1].severity, Severity::Error);
    }
}
