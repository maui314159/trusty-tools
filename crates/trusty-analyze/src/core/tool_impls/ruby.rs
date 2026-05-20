//! `RubocopTool` — Ruby diagnostics via `rubocop`.
//!
//! Why: RuboCop is the standard Ruby linter; its JSON formatter is stable.
//! What: runs `rubocop --format=json <file>` and walks `files[].offenses[]`.
//! Test: `parse_rubocop_json_extracts_offense` parses a captured report.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Ruby static-analysis tool backed by `rubocop`.
pub struct RubocopTool;

impl StaticTool for RubocopTool {
    fn name(&self) -> &str {
        "rubocop"
    }

    fn language(&self) -> &str {
        "ruby"
    }

    fn is_available(&self) -> bool {
        which::which("rubocop").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command("rubocop", &["--format=json", &path], dir) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("rubocop invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_rubocop_json(&out.stdout))
    }
}

/// Parse RuboCop's JSON report into diagnostics.
fn parse_rubocop_json(stdout: &str) -> Vec<ToolDiagnostic> {
    let Ok(root) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(files) = root.get("files").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut diags = Vec::new();
    for file_entry in files {
        let file = file_entry
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(offenses) = file_entry.get("offenses").and_then(Value::as_array) else {
            continue;
        };
        for o in offenses {
            diags.push(rubocop_offense_to_diag(o, &file));
        }
    }
    diags
}

/// Convert one RuboCop offense into a `ToolDiagnostic`.
fn rubocop_offense_to_diag(o: &Value, file: &str) -> ToolDiagnostic {
    let location = o.get("location");
    let line = location
        .and_then(|l| l.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let col = location
        .and_then(|l| l.get("column"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let message = o
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let code = o
        .get("cop_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let severity = severity_from_str(o.get("severity").and_then(Value::as_str).unwrap_or(""));
    ToolDiagnostic {
        tool: "rubocop".into(),
        file: file.to_string(),
        line,
        col,
        severity,
        code,
        message,
    }
}

/// Map a RuboCop severity string to a `Severity`.
fn severity_from_str(s: &str) -> Severity {
    match s {
        "error" | "fatal" => Severity::Error,
        "warning" => Severity::Warning,
        "convention" | "refactor" => Severity::Info,
        _ => Severity::Hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rubocop_json_extracts_offense() {
        let json = r#"{"files":[{"path":"a.rb","offenses":[{"cop_name":"Style/StringLiterals","message":"prefer single quotes","severity":"convention","location":{"line":2,"column":5}}]}]}"#;
        let diags = parse_rubocop_json(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 2);
        assert_eq!(diags[0].severity, Severity::Info);
        assert_eq!(diags[0].code.as_deref(), Some("Style/StringLiterals"));
    }

    #[test]
    fn parse_rubocop_json_tolerates_garbage() {
        assert!(parse_rubocop_json("not json").is_empty());
        assert!(parse_rubocop_json("{}").is_empty());
    }
}
