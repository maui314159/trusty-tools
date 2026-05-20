//! `PmdTool` — Java diagnostics via `pmd`.
//!
//! Why: PMD is a mature Java static analyzer with a structured JSON report.
//! What: runs `pmd check -f json -d <file> --no-fail-on-violation` and walks
//! `files[].violations[]`, mapping PMD priority to a `Severity`.
//! Test: `parse_pmd_json_extracts_violation` parses a captured report.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Java static-analysis tool backed by `pmd`.
pub struct PmdTool;

impl StaticTool for PmdTool {
    fn name(&self) -> &str {
        "pmd"
    }

    fn language(&self) -> &str {
        "java"
    }

    fn is_available(&self) -> bool {
        which::which("pmd").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let out = match run_command(
            "pmd",
            &["check", "-f", "json", "-d", &path, "--no-fail-on-violation"],
            dir,
        ) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("pmd invocation failed: {e}");
                return Ok(Vec::new());
            }
        };
        Ok(parse_pmd_json(&out.stdout))
    }
}

/// Parse PMD's JSON report into diagnostics.
fn parse_pmd_json(stdout: &str) -> Vec<ToolDiagnostic> {
    let Ok(root) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(files) = root.get("files").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut diags = Vec::new();
    for file_entry in files {
        let file = file_entry
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(violations) = file_entry.get("violations").and_then(Value::as_array) else {
            continue;
        };
        for v in violations {
            diags.push(pmd_violation_to_diag(v, &file));
        }
    }
    diags
}

/// Convert one PMD violation into a `ToolDiagnostic`.
fn pmd_violation_to_diag(v: &Value, file: &str) -> ToolDiagnostic {
    let line = v.get("beginline").and_then(Value::as_u64).unwrap_or(0) as u32;
    let col = v.get("begincolumn").and_then(Value::as_u64).unwrap_or(0) as u32;
    let priority = v.get("priority").and_then(Value::as_u64).unwrap_or(3);
    let message = v
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let code = v.get("rule").and_then(Value::as_str).map(str::to_string);
    ToolDiagnostic {
        tool: "pmd".into(),
        file: file.to_string(),
        line,
        col,
        severity: severity_from_priority(priority),
        code,
        message,
    }
}

/// Map PMD priority (1 = highest) to a `Severity`.
fn severity_from_priority(priority: u64) -> Severity {
    match priority {
        1 | 2 => Severity::Error,
        3 => Severity::Warning,
        _ => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pmd_json_extracts_violation() {
        let json = r#"{"files":[{"filename":"A.java","violations":[{"beginline":4,"begincolumn":2,"priority":1,"rule":"UnusedImport","description":"unused import"}]}]}"#;
        let diags = parse_pmd_json(json);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 4);
        assert_eq!(diags[0].severity, Severity::Error);
        assert_eq!(diags[0].code.as_deref(), Some("UnusedImport"));
    }

    #[test]
    fn parse_pmd_json_tolerates_garbage() {
        assert!(parse_pmd_json("not json").is_empty());
        assert!(parse_pmd_json("{}").is_empty());
    }

    #[test]
    fn severity_from_priority_buckets() {
        assert_eq!(severity_from_priority(1), Severity::Error);
        assert_eq!(severity_from_priority(3), Severity::Warning);
        assert_eq!(severity_from_priority(5), Severity::Info);
    }
}
