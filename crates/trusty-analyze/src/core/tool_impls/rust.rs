//! `ClippyTool` — Rust diagnostics via `cargo clippy`.
//!
//! Why: clippy is the canonical Rust linter; running it on demand surfaces
//! lints tree-sitter heuristics never catch.
//! What: runs `cargo clippy --message-format=json` in the file's parent
//! directory, parses the streaming JSON compiler messages, and keeps the ones
//! whose primary span points at the requested file.
//! Test: `parse_clippy_json_extracts_warning` parses a captured message line.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Rust static-analysis tool backed by `cargo clippy`.
pub struct ClippyTool;

impl StaticTool for ClippyTool {
    fn name(&self) -> &str {
        "clippy"
    }

    fn language(&self) -> &str {
        "rust"
    }

    fn is_available(&self) -> bool {
        which::which("cargo").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let out = match run_command(
            "cargo",
            &["clippy", "--message-format=json", "--quiet"],
            dir,
        ) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("clippy invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_clippy_json(&out.stdout, file))
    }
}

/// Parse newline-delimited cargo JSON messages into diagnostics for `file`.
fn parse_clippy_json(stdout: &str, file: &Path) -> Vec<ToolDiagnostic> {
    let want = file.to_string_lossy();
    let mut diags = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(root) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // cargo wraps the compiler message under `message` for build messages.
        let Some(msg) = root.get("message") else {
            continue;
        };
        if let Some(d) = clippy_message_to_diag(msg, &want) {
            diags.push(d);
        }
    }
    diags
}

/// Convert a single rustc/clippy `message` object into a `ToolDiagnostic`.
fn clippy_message_to_diag(msg: &Value, want_file: &str) -> Option<ToolDiagnostic> {
    let level = msg.get("level").and_then(Value::as_str).unwrap_or("");
    if level == "note" || level.is_empty() {
        return None;
    }
    let spans = msg.get("spans").and_then(Value::as_array)?;
    let span = spans
        .iter()
        .find(|s| {
            s.get("is_primary")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .or_else(|| spans.first())?;

    let span_file = span.get("file_name").and_then(Value::as_str)?;
    if !file_matches(span_file, want_file) {
        return None;
    }

    let line = span.get("line_start").and_then(Value::as_u64).unwrap_or(0) as u32;
    let col = span
        .get("column_start")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let message = msg
        .get("rendered")
        .and_then(Value::as_str)
        .or_else(|| msg.get("message").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let code = msg
        .get("code")
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);

    Some(ToolDiagnostic {
        tool: "clippy".into(),
        file: span_file.to_string(),
        line,
        col,
        severity: severity_from_level(level),
        code,
        message,
    })
}

/// True if the clippy span file path refers to the target file.
fn file_matches(span_file: &str, want: &str) -> bool {
    span_file == want || want.ends_with(span_file) || span_file.ends_with(want)
}

/// Map a rustc level string to a `Severity`.
fn severity_from_level(level: &str) -> Severity {
    match level {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "help" => Severity::Hint,
        _ => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clippy_json_extracts_warning() {
        let line = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unneeded return","rendered":"warning: unneeded return statement","code":{"code":"clippy::needless_return"},"spans":[{"is_primary":true,"file_name":"src/main.rs","line_start":7,"column_start":5}]}}"#;
        let diags = parse_clippy_json(line, Path::new("src/main.rs"));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 7);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[0].code.as_deref(), Some("clippy::needless_return"));
    }

    #[test]
    fn parse_clippy_json_skips_notes_and_other_files() {
        let note = r#"{"message":{"level":"note","message":"n","spans":[{"is_primary":true,"file_name":"src/main.rs","line_start":1,"column_start":1}]}}"#;
        let other = r#"{"message":{"level":"warning","message":"w","spans":[{"is_primary":true,"file_name":"other.rs","line_start":1,"column_start":1}]}}"#;
        let input = format!("{note}\n{other}\n");
        let diags = parse_clippy_json(&input, Path::new("src/main.rs"));
        assert!(diags.is_empty());
    }

    #[test]
    fn parse_clippy_json_tolerates_garbage() {
        let diags = parse_clippy_json("not json\n{}\n", Path::new("src/main.rs"));
        assert!(diags.is_empty());
    }
}
