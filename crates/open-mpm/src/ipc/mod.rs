//! NDJSON IPC protocol for PM <-> sub-agent communication.
//!
//! Why: Provides a minimal, newline-delimited JSON protocol so the PM and
//! sub-agent subprocesses can exchange structured messages over stdin/stdout
//! without framing ambiguity.
//! What: Defines the `IpcMessage` enum (Task / Result / Error) and helpers
//! to serialize to/from single-line JSON.
//! Test: Round-trip each variant through `serialize_message` + `parse_message`
//! and assert equality (see unit tests below).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::perf::TokenUsage;
use crate::session::HistoryMessage;

/// A single IPC message exchanged between the PM and a sub-agent.
///
/// Why: Discriminated union keeps message handling type-safe while serializing
/// to a compact single-line JSON form suitable for NDJSON framing.
/// What: Three variants — Task (PM -> sub), Result (sub -> PM success),
/// Error (sub -> PM failure). All carry a correlation `id`.
/// Test: Serialize each variant, assert the `"type"` tag matches; parse back
/// and assert structural equality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcMessage {
    /// PM -> sub-agent: new task to execute.
    ///
    /// `history`, when present, carries prior user/assistant turns that the
    /// sub-agent should prepend to its chat request (issue #51 — persistent
    /// agent sessions). `session_reset`, when true, instructs the sub-agent
    /// to behave as if no history exists; its primary use is flushing stale
    /// state mid-run without round-tripping through the PM.
    /// Both fields are optional and omitted from the wire when absent so
    /// existing tools and older agents keep working unchanged.
    Task {
        id: String,
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        history: Option<Vec<HistoryMessage>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_reset: Option<bool>,
    },
    /// Sub-agent -> PM: successful result.
    ///
    /// `content` is the full agent output (used for `## File:` extraction and
    /// written to disk). `summary` is an optional, concise summary (~200-500
    /// words) that downstream workflow phases substitute via `{{phase_name}}`
    /// templates — keeping prompt sizes bounded. Missing = no summary extracted.
    Result {
        id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Aggregated LLM token usage for this task (#47).
        ///
        /// Why: Sub-agents own the LLM round-trips; PM/WorkflowEngine needs
        /// the counts to produce per-phase performance records. Optional for
        /// backward compat with tool-only (non-LLM) results.
        /// What: `TokenUsage` with Anthropic cache fields; serializes under
        /// the `"usage"` key and is omitted when absent.
        /// Test: `ipc::tests::result_with_usage_roundtrip`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        status: String,
    },
    /// Sub-agent -> PM: execution failure.
    Error {
        id: String,
        error: String,
        status: String,
    },
}

impl IpcMessage {
    /// Convenience constructor for a Task message with a fresh UUIDv4 id.
    pub fn new_task(task: impl Into<String>) -> Self {
        IpcMessage::Task {
            id: uuid::Uuid::new_v4().to_string(),
            task: task.into(),
            history: None,
            session_reset: None,
        }
    }

    /// Convenience constructor for a Task carrying prior session history.
    ///
    /// Why: Persistent-session agents (issue #51) need their caller to forward
    /// the accumulated user/assistant turns so the sub-agent can rebuild
    /// context in a fresh process.
    /// What: Same as `new_task` but sets `history` when non-empty.
    /// Test: `ipc::tests::task_with_history_roundtrip`.
    #[allow(dead_code)]
    pub fn new_task_with_history(task: impl Into<String>, history: Vec<HistoryMessage>) -> Self {
        IpcMessage::Task {
            id: uuid::Uuid::new_v4().to_string(),
            task: task.into(),
            history: if history.is_empty() {
                None
            } else {
                Some(history)
            },
            session_reset: None,
        }
    }

    /// Convenience constructor for a success Result message (no summary).
    #[allow(dead_code)]
    pub fn new_result(id: impl Into<String>, content: impl Into<String>) -> Self {
        IpcMessage::Result {
            id: id.into(),
            content: content.into(),
            summary: None,
            usage: None,
            status: "success".into(),
        }
    }

    /// Convenience constructor for a success Result message with a summary.
    ///
    /// Why: Sub-agents producing output for downstream phases should emit a
    /// concise summary so prompt context doesn't balloon. This helper keeps
    /// callers from having to spell out the struct.
    /// What: Returns a `Result` variant with both `content` and `summary` set.
    /// Test: See `ipc::tests::result_with_summary_roundtrip`.
    #[allow(dead_code)]
    pub fn new_result_with_summary(
        id: impl Into<String>,
        content: impl Into<String>,
        summary: Option<String>,
    ) -> Self {
        IpcMessage::Result {
            id: id.into(),
            content: content.into(),
            summary,
            usage: None,
            status: "success".into(),
        }
    }

    /// Constructor for a success `Result` message with optional summary and
    /// aggregated token usage.
    ///
    /// Why: (#47) Sub-agents aggregate per-turn `TokenUsage` across the whole
    /// task and bubble it up via IPC so `WorkflowEngine` can build the
    /// per-phase performance record.
    /// What: Returns a `Result` variant populating all three optional fields.
    /// Test: `ipc::tests::result_with_usage_roundtrip`.
    pub fn new_result_full(
        id: impl Into<String>,
        content: impl Into<String>,
        summary: Option<String>,
        usage: Option<TokenUsage>,
    ) -> Self {
        IpcMessage::Result {
            id: id.into(),
            content: content.into(),
            summary,
            usage,
            status: "success".into(),
        }
    }

    /// Convenience constructor for an Error message.
    pub fn new_error(id: impl Into<String>, error: impl Into<String>) -> Self {
        IpcMessage::Error {
            id: id.into(),
            error: error.into(),
            status: "error".into(),
        }
    }
}

/// Serialize an IpcMessage to a single NDJSON line (trailing `\n`).
///
/// Why: Callers write one message per line to the IPC pipe; centralizing the
/// newline here prevents framing bugs at call sites.
/// What: Returns `"{...json...}\n"`.
/// Test: Assert output ends with `\n` and contains no embedded newlines in
/// the JSON prefix.
pub fn serialize_message(msg: &IpcMessage) -> Result<String> {
    let mut s = serde_json::to_string(msg).context("failed to serialize IpcMessage")?;
    s.push('\n');
    Ok(s)
}

/// Parse a single NDJSON line into an IpcMessage.
///
/// Why: Callers read one line at a time from the IPC pipe; this helper
/// strips any trailing newline and decodes the JSON object.
/// What: Returns `Ok(IpcMessage)` or an error with context.
/// Test: Feed known-good JSON for each variant and assert equality; feed
/// malformed JSON and assert `Err`.
pub fn parse_message(line: &str) -> Result<IpcMessage> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    serde_json::from_str::<IpcMessage>(trimmed)
        .with_context(|| format!("failed to parse IpcMessage from line: {trimmed}"))
}

/// Extract a summary from an agent's content output.
///
/// Why: Downstream workflow phases need a concise summary (~200-500 words)
/// rather than 30k chars of raw code. Agents are instructed to append a
/// `## Summary` section; this helper extracts it so the engine can forward
/// only the summary into subsequent phase templates.
/// What: Looks for a case-insensitive `## Summary` heading at the end of the
/// content. If found, returns everything AFTER that header (trimmed). If not
/// found, returns the first 500 chars of content as a fallback.
/// Test: See `extract_summary_finds_trailing_section` and
/// `extract_summary_fallback_uses_prefix`.
pub fn extract_summary(content: &str) -> String {
    let lower = content.to_ascii_lowercase();
    // Find the LAST `## summary` heading (case-insensitive) so agents that
    // reference the word "summary" inline don't trigger a false match.
    let mut best: Option<usize> = None;
    let needle = "## summary";
    let mut start = 0usize;
    while let Some(pos) = lower[start..].find(needle) {
        let abs = start + pos;
        // Must be at start of line (preceded by newline or be at offset 0).
        let at_line_start = abs == 0 || lower.as_bytes()[abs - 1] == b'\n';
        if at_line_start {
            best = Some(abs);
        }
        start = abs + needle.len();
    }

    if let Some(h) = best {
        // Advance past the rest of the header line.
        let after_header = &content[h..];
        if let Some(nl) = after_header.find('\n') {
            return after_header[nl + 1..].trim().to_string();
        }
        // Header with no newline after it — nothing to return.
        return String::new();
    }

    // Fallback: first 500 chars.
    let trimmed = content.trim();
    if trimmed.chars().count() <= 500 {
        return trimmed.to_string();
    }
    let prefix: String = trimmed.chars().take(500).collect();
    prefix
}

/// Parse `## File: <path>` / `### \`<path>\`` sections from LLM output into
/// a list of `(relative_path, body)` pairs, without touching the filesystem.
///
/// Why: (#64) The workflow engine must be able to materialize code-phase
/// output to disk BETWEEN phases so the QA phase can run pytest against it.
/// Keeping the parse step pure makes it testable and reusable by both the
/// workflow engine and the legacy `main.rs::extract_files_to_dir` fallback
/// used by `--direct` mode.
/// What: Scans line-by-line for `## File: <path>`, `### File: <path>`, or
/// `## \`path\`` / `### \`path\`` markdown headers, then captures the next
/// fenced code block as the file body. Empty / unterminated blocks are
/// skipped. Returns `Vec<(PathBuf, String)>` in the order they appear.
/// Test: `extract_files_from_content_parses_multiple_files` asserts two
/// files are returned from a document with two `## File:` sections.
pub fn extract_files_from_content(content: &str) -> Vec<(PathBuf, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        let rel_path: Option<String> = if let Some(rest) = trimmed.strip_prefix("## File:") {
            Some(rest.trim().trim_matches('`').to_string())
        } else if let Some(rest) = trimmed.strip_prefix("### File:") {
            Some(rest.trim().trim_matches('`').to_string())
        } else if let Some(rest) = trimmed.strip_prefix("### ") {
            let s = rest.trim();
            if s.starts_with('`') && s.ends_with('`') && s.len() >= 2 {
                Some(s.trim_matches('`').to_string())
            } else {
                None
            }
        } else if let Some(rest) = trimmed.strip_prefix("## ") {
            let s = rest.trim();
            if s.starts_with('`') && s.ends_with('`') && s.len() >= 2 {
                Some(s.trim_matches('`').to_string())
            } else {
                None
            }
        } else {
            None
        };

        let Some(rel) = rel_path else {
            i += 1;
            continue;
        };

        if rel.is_empty() {
            i += 1;
            continue;
        }

        let mut j = i + 1;
        while j < lines.len() && !lines[j].trim_start().starts_with("```") {
            j += 1;
        }
        if j >= lines.len() {
            break;
        }

        let body_start = j + 1;
        let mut k = body_start;
        while k < lines.len() && !lines[k].trim_start().starts_with("```") {
            k += 1;
        }

        let body = if k > body_start {
            lines[body_start..k].join("\n")
        } else {
            String::new()
        };
        let mut final_body = body;
        if !final_body.ends_with('\n') {
            final_body.push('\n');
        }

        out.push((PathBuf::from(rel), final_body));
        i = k + 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_roundtrip() {
        let msg = IpcMessage::Task {
            id: "abc".into(),
            task: "do stuff".into(),
            history: None,
            session_reset: None,
        };
        let wire = serialize_message(&msg).unwrap();
        assert!(wire.ends_with('\n'));
        let back = parse_message(&wire).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn task_with_history_roundtrip() {
        let msg = IpcMessage::new_task_with_history(
            "go",
            vec![
                HistoryMessage::user("earlier-q"),
                HistoryMessage::assistant("earlier-a"),
            ],
        );
        let wire = serialize_message(&msg).unwrap();
        let back = parse_message(&wire).unwrap();
        assert_eq!(msg, back);
        match back {
            IpcMessage::Task { history, .. } => {
                let h = history.expect("history present");
                assert_eq!(h.len(), 2);
                assert_eq!(h[0].role, "user");
                assert_eq!(h[1].role, "assistant");
            }
            _ => panic!("expected Task"),
        }
    }

    #[test]
    fn task_legacy_wire_parses_without_history() {
        // Pre-#51 wire format omits `history`/`session_reset`.
        let wire = r#"{"type":"task","id":"x","task":"y"}"#;
        let msg = parse_message(wire).unwrap();
        match msg {
            IpcMessage::Task {
                history,
                session_reset,
                ..
            } => {
                assert!(history.is_none());
                assert!(session_reset.is_none());
            }
            _ => panic!("expected Task"),
        }
    }

    #[test]
    fn result_roundtrip() {
        let msg = IpcMessage::new_result("id1", "output");
        let wire = serialize_message(&msg).unwrap();
        let back = parse_message(&wire).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn error_roundtrip() {
        let msg = IpcMessage::new_error("id1", "something broke");
        let wire = serialize_message(&msg).unwrap();
        let back = parse_message(&wire).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_message("not json").is_err());
    }

    #[test]
    fn result_with_summary_roundtrip() {
        let msg = IpcMessage::new_result_with_summary(
            "id1",
            "full content body",
            Some("short summary".into()),
        );
        let wire = serialize_message(&msg).unwrap();
        let back = parse_message(&wire).unwrap();
        assert_eq!(msg, back);
        match back {
            IpcMessage::Result { summary, .. } => {
                assert_eq!(summary, Some("short summary".to_string()));
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn result_with_usage_roundtrip() {
        let usage = TokenUsage::new(100, 50, 10, 5);
        let msg = IpcMessage::new_result_full("id1", "content", Some("sum".into()), Some(usage));
        let wire = serialize_message(&msg).unwrap();
        let back = parse_message(&wire).unwrap();
        assert_eq!(msg, back);
        match back {
            IpcMessage::Result { usage, .. } => {
                let u = usage.expect("usage present");
                assert_eq!(u.prompt_tokens, 100);
                assert_eq!(u.completion_tokens, 50);
                assert_eq!(u.cache_read_tokens, 10);
                assert_eq!(u.cache_creation_tokens, 5);
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn result_without_summary_parses_old_wire_format() {
        // Prior-version wire format lacks `summary`.
        let wire = r#"{"type":"result","id":"x","content":"y","status":"success"}"#;
        let msg = parse_message(wire).unwrap();
        match msg {
            IpcMessage::Result { summary, .. } => assert!(summary.is_none()),
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn extract_summary_finds_trailing_section() {
        let content = "Some code here\nmore stuff\n\n## Summary\nThis is the concise summary the agent wrote.";
        let s = extract_summary(content);
        assert_eq!(s, "This is the concise summary the agent wrote.");
    }

    #[test]
    fn extract_summary_is_case_insensitive() {
        let content = "header\n\n## SUMMARY\nbody text here";
        let s = extract_summary(content);
        assert_eq!(s, "body text here");
    }

    #[test]
    fn extract_summary_finds_last_summary_section() {
        // Agents may mention the word "summary" inline but only the trailing
        // ## Summary block counts.
        let content =
            "## Summary of Research Findings (inline)\nignored\n\n## Summary\nreal summary";
        let s = extract_summary(content);
        assert_eq!(s, "real summary");
    }

    #[test]
    fn extract_summary_fallback_uses_prefix() {
        let long = "a".repeat(2000);
        let s = extract_summary(&long);
        assert_eq!(s.chars().count(), 500);
    }

    #[test]
    fn extract_summary_fallback_short_content_verbatim() {
        let content = "short output";
        let s = extract_summary(content);
        assert_eq!(s, "short output");
    }

    #[test]
    fn extract_files_from_content_parses_multiple_files() {
        let content = "Intro text.\n\n## File: app/main.py\n```python\nprint(\"hi\")\n```\n\n## File: app/util.py\n```python\ndef f():\n    return 1\n```\n";
        let files = extract_files_from_content(content);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, PathBuf::from("app/main.py"));
        assert!(files[0].1.contains("print(\"hi\")"));
        assert!(files[0].1.ends_with('\n'));
        assert_eq!(files[1].0, PathBuf::from("app/util.py"));
        assert!(files[1].1.contains("def f():"));
    }

    #[test]
    fn extract_files_from_content_returns_empty_when_no_markers() {
        let files = extract_files_from_content("Just prose, no file sections.");
        assert!(files.is_empty());
    }

    #[test]
    fn extract_files_from_content_handles_backtick_header() {
        let content = "### `scripts/go.sh`\n```bash\necho hi\n```\n";
        let files = extract_files_from_content(content);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, PathBuf::from("scripts/go.sh"));
        assert!(files[0].1.contains("echo hi"));
    }
}
