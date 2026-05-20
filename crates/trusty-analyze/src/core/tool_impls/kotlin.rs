//! `DetektTool` — Kotlin diagnostics via `detekt`.
//!
//! Why: detekt is the standard Kotlin static analyzer; it emits SARIF, a
//! well-specified diagnostics format.
//! What: runs `detekt --input <file> --report sarif:<tmpfile>`, then parses
//! the SARIF `runs[0].results[]` array.
//! Test: `parse_detekt_sarif_extracts_result` parses a captured SARIF doc.

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use super::run_command;
use crate::core::tools::{Severity, StaticTool, ToolDiagnostic};

/// Kotlin static-analysis tool backed by `detekt`.
pub struct DetektTool;

impl StaticTool for DetektTool {
    fn name(&self) -> &str {
        "detekt"
    }

    fn language(&self) -> &str {
        "kotlin"
    }

    fn is_available(&self) -> bool {
        which::which("detekt").is_ok()
    }

    fn run(&self, file: &Path, _content: &str) -> Result<Vec<ToolDiagnostic>> {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let path = file.to_string_lossy();
        let tmp = tempfile::Builder::new()
            .suffix(".sarif")
            .tempfile()
            .map_err(|e| anyhow::anyhow!("failed to create temp sarif file: {e}"))?;
        let report_arg = format!("sarif:{}", tmp.path().to_string_lossy());
        let run_res = run_command("detekt", &["--input", &path, "--report", &report_arg], dir);
        if let Err(e) = run_res {
            tracing::debug!("detekt invocation failed: {e}");
            return Ok(Vec::new());
        }
        let sarif = std::fs::read_to_string(tmp.path()).unwrap_or_default();
        Ok(parse_detekt_sarif(&sarif))
    }
}

/// Parse a detekt SARIF document into diagnostics.
fn parse_detekt_sarif(sarif: &str) -> Vec<ToolDiagnostic> {
    let Ok(root) = serde_json::from_str::<Value>(sarif.trim()) else {
        return Vec::new();
    };
    let Some(results) = root
        .get("runs")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
        .and_then(|run| run.get("results"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    results.iter().filter_map(sarif_result_to_diag).collect()
}

/// Convert one SARIF result into a `ToolDiagnostic`.
fn sarif_result_to_diag(result: &Value) -> Option<ToolDiagnostic> {
    let code = result
        .get("ruleId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let message = result
        .get("message")
        .and_then(|m| m.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let physical = result
        .get("locations")
        .and_then(Value::as_array)
        .and_then(|l| l.first())
        .and_then(|loc| loc.get("physicalLocation"))?;
    let file = physical
        .get("artifactLocation")
        .and_then(|a| a.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let region = physical.get("region");
    let line = region
        .and_then(|r| r.get("startLine"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let col = region
        .and_then(|r| r.get("startColumn"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let severity = severity_from_str(result.get("level").and_then(Value::as_str).unwrap_or(""));
    Some(ToolDiagnostic {
        tool: "detekt".into(),
        file,
        line,
        col,
        severity,
        code,
        message,
    })
}

/// Map a SARIF level string to a `Severity`.
fn severity_from_str(s: &str) -> Severity {
    match s {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "note" => Severity::Info,
        _ => Severity::Hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_detekt_sarif_extracts_result() {
        let sarif = r#"{"runs":[{"results":[{"ruleId":"MagicNumber","level":"warning","message":{"text":"avoid magic numbers"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"A.kt"},"region":{"startLine":5,"startColumn":2}}}]}]}]}"#;
        let diags = parse_detekt_sarif(sarif);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "A.kt");
        assert_eq!(diags[0].line, 5);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[0].code.as_deref(), Some("MagicNumber"));
    }

    #[test]
    fn parse_detekt_sarif_tolerates_garbage() {
        assert!(parse_detekt_sarif("not json").is_empty());
        assert!(parse_detekt_sarif("{}").is_empty());
    }
}
