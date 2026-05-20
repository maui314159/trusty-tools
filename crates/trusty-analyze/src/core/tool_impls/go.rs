//! `StaticcheckTool` — Go diagnostics via `staticcheck`.
//!
//! Why: staticcheck is the de-facto Go static analyzer; its line-delimited
//! JSON output is easy to normalize.
//! What: runs `staticcheck -f json <file>` and parses one JSON object per
//! line into a `ToolDiagnostic`.
//! Test: `parse_staticcheck_json_extracts_diagnostic` parses a captured line.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Go static-analysis tool backed by `staticcheck`.
pub struct StaticcheckTool;

impl StaticTool for StaticcheckTool {
    fn name(&self) -> &str {
        "staticcheck"
    }

    fn language(&self) -> &str {
        "go"
    }

    fn is_available(&self) -> bool {
        which::which("staticcheck").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command("staticcheck", &["-f", "json", &path], dir) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("staticcheck invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_staticcheck_json(&out.stdout))
    }
}

/// Parse staticcheck's newline-delimited JSON into diagnostics.
fn parse_staticcheck_json(stdout: &str) -> Vec<ToolDiagnostic> {
    let mut diags = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(d) = staticcheck_obj_to_diag(&obj) {
            diags.push(d);
        }
    }
    diags
}

/// Convert a single staticcheck JSON object into a `ToolDiagnostic`.
fn staticcheck_obj_to_diag(obj: &Value) -> Option<ToolDiagnostic> {
    let location = obj.get("location")?;
    let file = location.get("file").and_then(Value::as_str)?.to_string();
    let line = location.get("line").and_then(Value::as_u64).unwrap_or(0) as u32;
    let col = location.get("column").and_then(Value::as_u64).unwrap_or(0) as u32;
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let code = obj.get("code").and_then(Value::as_str).map(str::to_string);
    Some(ToolDiagnostic {
        tool: "staticcheck".into(),
        file,
        line,
        col,
        severity: Severity::Warning,
        code,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_staticcheck_json_extracts_diagnostic() {
        let line = r#"{"code":"SA4006","message":"value never used","location":{"file":"main.go","line":12,"column":3}}"#;
        let diags = parse_staticcheck_json(line);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 12);
        assert_eq!(diags[0].code.as_deref(), Some("SA4006"));
        assert_eq!(diags[0].severity, Severity::Warning);
    }

    #[test]
    fn parse_staticcheck_json_tolerates_garbage() {
        assert!(parse_staticcheck_json("not json\n{}\n").is_empty());
    }
}
