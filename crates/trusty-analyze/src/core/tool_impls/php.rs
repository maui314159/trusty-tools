//! `PhpstanTool` — PHP diagnostics via `phpstan`.
//!
//! Why: PHPStan is the leading PHP static analyzer; its JSON error format
//! keys findings by file path.
//! What: runs `phpstan analyse --no-progress --error-format=json <file>` and
//! walks `files.<path>.messages[]`.
//! Test: `parse_phpstan_json_extracts_message` parses a captured report.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// PHP static-analysis tool backed by `phpstan`.
pub struct PhpstanTool;

impl StaticTool for PhpstanTool {
    fn name(&self) -> &str {
        "phpstan"
    }

    fn language(&self) -> &str {
        "php"
    }

    fn is_available(&self) -> bool {
        which::which("phpstan").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command(
            "phpstan",
            &["analyse", "--no-progress", "--error-format=json", &path],
            dir,
        ) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("phpstan invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_phpstan_json(&out.stdout))
    }
}

/// Parse PHPStan's JSON report into diagnostics.
fn parse_phpstan_json(stdout: &str) -> Vec<ToolDiagnostic> {
    let Ok(root) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(files) = root.get("files").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut diags = Vec::new();
    for (path, entry) in files {
        let Some(messages) = entry.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for m in messages {
            diags.push(phpstan_message_to_diag(m, path));
        }
    }
    diags
}

/// Convert one PHPStan message into a `ToolDiagnostic`.
fn phpstan_message_to_diag(m: &Value, file: &str) -> ToolDiagnostic {
    let line = m.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
    let message = m
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let code = m
        .get("identifier")
        .and_then(Value::as_str)
        .map(str::to_string);
    ToolDiagnostic {
        tool: "phpstan".into(),
        file: file.to_string(),
        line,
        col: 0,
        severity: Severity::Error,
        code,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_phpstan_json_extracts_message() {
        let json = r#"{"files":{"a.php":{"messages":[{"line":9,"message":"Undefined variable $x","identifier":"variable.undefined"}]}}}"#;
        let diags = parse_phpstan_json(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "a.php");
        assert_eq!(diags[0].line, 9);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn parse_phpstan_json_tolerates_garbage() {
        assert!(parse_phpstan_json("not json").is_empty());
        assert!(parse_phpstan_json("{}").is_empty());
    }
}
