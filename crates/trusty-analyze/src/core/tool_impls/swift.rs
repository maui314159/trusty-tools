//! `SwiftlintTool` — Swift diagnostics via `swiftlint`.
//!
//! Why: SwiftLint is the standard Swift linter; its JSON reporter is flat and
//! easy to normalize.
//! What: runs `swiftlint lint --reporter json <file>` and maps each entry.
//! Test: `parse_swiftlint_json_extracts_entry` parses a captured array.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Swift static-analysis tool backed by `swiftlint`.
pub struct SwiftlintTool;

impl StaticTool for SwiftlintTool {
    fn name(&self) -> &str {
        "swiftlint"
    }

    fn language(&self) -> &str {
        "swift"
    }

    fn is_available(&self) -> bool {
        which::which("swiftlint").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command("swiftlint", &["lint", "--reporter", "json", &path], dir) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("swiftlint invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_swiftlint_json(&out.stdout))
    }
}

/// Parse SwiftLint's JSON array output into diagnostics.
fn parse_swiftlint_json(stdout: &str) -> Vec<ToolDiagnostic> {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(stdout.trim()) else {
        return Vec::new();
    };
    items.iter().map(swiftlint_item_to_diag).collect()
}

/// Convert one SwiftLint entry into a `ToolDiagnostic`.
fn swiftlint_item_to_diag(item: &Value) -> ToolDiagnostic {
    let file = item
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let line = item.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
    let col = item.get("character").and_then(Value::as_u64).unwrap_or(0) as u32;
    let message = item
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let code = item
        .get("rule_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let severity = severity_from_str(item.get("severity").and_then(Value::as_str).unwrap_or(""));
    ToolDiagnostic {
        tool: "swiftlint".into(),
        file,
        line,
        col,
        severity,
        code,
        message,
    }
}

/// Map a SwiftLint severity string to a `Severity`.
fn severity_from_str(s: &str) -> Severity {
    match s.to_ascii_lowercase().as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_swiftlint_json_extracts_entry() {
        let json = r#"[{"file":"A.swift","line":3,"character":1,"severity":"Warning","rule_id":"line_length","reason":"line too long"}]"#;
        let diags = parse_swiftlint_json(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 3);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[0].code.as_deref(), Some("line_length"));
    }

    #[test]
    fn parse_swiftlint_json_tolerates_garbage() {
        assert!(parse_swiftlint_json("not json").is_empty());
    }
}
