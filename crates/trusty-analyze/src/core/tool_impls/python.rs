//! `RuffTool` — Python diagnostics via `ruff check`.
//!
//! Why: ruff is the fast, ubiquitous Python linter; its JSON output is stable
//! and trivial to normalize.
//! What: runs `ruff check --output-format=json --no-cache <file>` and maps
//! each result to a `ToolDiagnostic`.
//! Test: `parse_ruff_json_extracts_diagnostic` parses a captured JSON array.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Python static-analysis tool backed by `ruff`.
pub struct RuffTool;

impl StaticTool for RuffTool {
    fn name(&self) -> &str {
        "ruff"
    }

    fn language(&self) -> &str {
        "python"
    }

    fn is_available(&self) -> bool {
        which::which("ruff").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command(
            "ruff",
            &["check", "--output-format=json", "--no-cache", &path],
            dir,
        ) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("ruff invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_ruff_json(&out.stdout))
    }
}

/// Parse ruff's JSON array output into diagnostics.
fn parse_ruff_json(stdout: &str) -> Vec<ToolDiagnostic> {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(stdout.trim()) else {
        return Vec::new();
    };
    items.iter().filter_map(ruff_item_to_diag).collect()
}

/// Convert a single ruff result object into a `ToolDiagnostic`.
fn ruff_item_to_diag(item: &Value) -> Option<ToolDiagnostic> {
    let file = item.get("filename").and_then(Value::as_str)?.to_string();
    let location = item.get("location")?;
    let line = location.get("row").and_then(Value::as_u64).unwrap_or(0) as u32;
    let col = location.get("column").and_then(Value::as_u64).unwrap_or(0) as u32;
    let code = item.get("code").and_then(Value::as_str).map(str::to_string);
    let message = item
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let severity = severity_from_code(code.as_deref());
    Some(ToolDiagnostic {
        tool: "ruff".into(),
        file,
        line,
        col,
        severity,
        code,
        message,
    })
}

/// Map a ruff rule code prefix to a `Severity`.
fn severity_from_code(code: Option<&str>) -> Severity {
    match code.and_then(|c| c.chars().next()) {
        Some('E') | Some('F') => Severity::Error,
        Some('W') | Some('S') => Severity::Warning,
        Some(_) => Severity::Info,
        None => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ruff_json_extracts_diagnostic() {
        let json = r#"[{"filename":"a.py","location":{"row":3,"column":1},"code":"F401","message":"imported but unused"}]"#;
        let diags = parse_ruff_json(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 3);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("F401"));
    }

    #[test]
    fn parse_ruff_json_tolerates_empty_and_garbage() {
        assert!(parse_ruff_json("[]").is_empty());
        assert!(parse_ruff_json("not json").is_empty());
    }

    #[test]
    fn severity_from_code_buckets() {
        assert_eq!(severity_from_code(Some("E501")), Severity::Error);
        assert_eq!(severity_from_code(Some("S101")), Severity::Warning);
        assert_eq!(severity_from_code(Some("C901")), Severity::Info);
        assert_eq!(severity_from_code(None), Severity::Info);
    }
}
