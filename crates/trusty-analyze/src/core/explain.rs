//! LLM-generated prose explanation of a [`ReviewReport`].
//!
//! Why: Deterministic metrics tell developers *what* is wrong; prose explains
//! *why it matters* and *what to do*, making findings actionable for
//! developers of all experience levels. The `--explain` flag and the
//! `?explain=true` query parameter both feed a built [`ReviewReport`] into
//! this module to produce a single string of LLM narrative that travels with
//! the report as [`ReviewReport::narrative`].
//!
//! What: builds a concise, grounded prompt from a `ReviewReport` (overall
//! grade, smell count, changed-line count, the worst files, the top smells,
//! detected frameworks, and the existing static recommendations) and asks a
//! [`trusty_common::chat::ChatProvider`] for a 2-3 paragraph explanation. The
//! provider only exposes a streaming API, so we drain the stream into a
//! `String` (collecting `Delta` events, terminating at `Done` or `Error`).
//!
//! Test: see `mod tests` — snapshot-style coverage of `build_explain_prompt`
//! plus a stub-`ChatProvider` test that confirms `explain_report` returns the
//! provider's text.

use trusty_common::chat::{ChatEvent, ChatProvider, ToolDef};
use trusty_common::ChatMessage;

use crate::core::review::ReviewReport;
use crate::types::complexity::ComplexityGrade;

/// Cap on the number of files / smells we list in the prompt. Keeps the
/// prompt under ~1500 tokens on a typical fast model so the call stays cheap.
const MAX_FILES_IN_PROMPT: usize = 3;
const MAX_SMELLS_IN_PROMPT: usize = 5;
const MAX_RECS_IN_PROMPT: usize = 5;

/// Generate a prose explanation of `report` using `provider`.
///
/// Why: turns a structured `ReviewReport` into a paragraph a human can read
/// and act on. Calls out the highest-leverage smells, ties them to the worst
/// files, and (when present) frames the advice in terms of the project's
/// detected frameworks.
/// What: builds a grounded prompt with `build_explain_prompt`, sends it as a
/// single `user` message via the provider's streaming chat API, and
/// accumulates `Delta` events into a `String`. Returns an error if the
/// provider returns an error event or its stream task fails.
/// Test: `explain_report_collects_stream_deltas` exercises the streaming
/// accumulation against a stub provider; `build_explain_prompt_*` tests cover
/// the prompt shape.
pub async fn explain_report(
    report: &ReviewReport,
    provider: &dyn ChatProvider,
) -> anyhow::Result<String> {
    let prompt = build_explain_prompt(report);
    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: SYSTEM_PROMPT.to_string(),
            tool_call_id: None,
            tool_calls: None,
        },
        ChatMessage {
            role: "user".to_string(),
            content: prompt,
            tool_call_id: None,
            tool_calls: None,
        },
    ];

    // Buffered channel keeps the provider task from blocking on a slow reader.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ChatEvent>(32);
    let stream_fut = provider.chat_stream(messages, Vec::<ToolDef>::new(), tx);

    let drain = async {
        let mut out = String::new();
        let mut stream_error: Option<String> = None;
        while let Some(event) = rx.recv().await {
            match event {
                ChatEvent::Delta(s) => out.push_str(&s),
                ChatEvent::ToolCall(_) => {
                    // Tools aren't used for explain; ignore any spurious tool calls.
                }
                ChatEvent::Done => break,
                ChatEvent::Error(msg) => {
                    stream_error = Some(msg);
                    break;
                }
            }
        }
        if let Some(msg) = stream_error {
            anyhow::bail!("chat provider stream error: {msg}");
        }
        Ok::<String, anyhow::Error>(out)
    };

    let (stream_res, narrative) = tokio::join!(stream_fut, drain);
    stream_res?;
    narrative
}

/// System prompt used for every explain call.
///
/// Why: keeps the LLM focused on actionable, framework-aware code review
/// guidance instead of generic advice or hallucinated metrics.
/// What: short instructions framing the assistant as a code-review explainer.
/// Test: covered transitively by `build_explain_prompt_*` (it's appended into
/// the message vector by `explain_report`).
const SYSTEM_PROMPT: &str = "You are a code review assistant. Given structured \
metrics from a pull-request review (complexity grade, code smells, recommendations, \
and detected frameworks), explain the findings in 2-3 short paragraphs of plain prose. \
Focus on why these issues matter and the highest-priority next steps the developer \
should take. Be specific: reference the file paths, smell categories, and frameworks \
provided. Do not invent metrics that aren't in the input.";

/// Build the user-message prompt body from `report`.
///
/// Why: keeps the prompt construction deterministic and testable in isolation
/// from the network. Grounding the prose in the exact metrics from the report
/// limits LLM hallucination.
/// What: assembles a markdown-ish text block listing overall grade, smell
/// count, changed-line count, detected frameworks, the worst
/// [`MAX_FILES_IN_PROMPT`] files (worst first), the first
/// [`MAX_SMELLS_IN_PROMPT`] smells across the report, and the first
/// [`MAX_RECS_IN_PROMPT`] static recommendations.
/// Test: `build_explain_prompt_includes_top_level_metrics` and
/// `build_explain_prompt_lists_worst_files_first` snapshot-style coverage.
pub fn build_explain_prompt(report: &ReviewReport) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "Overall grade: {}\nSmell count: {}\nChanged lines: {}\nFiles reviewed: {}\n",
        report.overall_grade,
        report.smell_count,
        report.changed_lines,
        report.files.len(),
    ));

    if !report.frameworks.is_empty() {
        out.push_str(&format!(
            "Detected frameworks: {}\n",
            report.frameworks.join(", ")
        ));
    }

    out.push('\n');
    out.push_str(&format!("Summary: {}\n\n", report.summary));

    // Worst files (highest grade enum value wins). Stable sort: preserve the
    // original ordering for files with the same grade.
    let mut worst: Vec<&_> = report.files.iter().collect();
    worst.sort_by(|a, b| b.grade.cmp(&a.grade));
    worst.truncate(MAX_FILES_IN_PROMPT);

    if !worst.is_empty() {
        out.push_str("Worst files (worst first):\n");
        for f in &worst {
            out.push_str(&format!(
                "- {} (grade {}, cyclomatic {}, cognitive {}, {} smell(s))\n",
                f.path,
                f.grade,
                f.complexity.cyclomatic,
                f.complexity.cognitive,
                f.smells.len(),
            ));
        }
        out.push('\n');
    }

    // Top smells across all files.
    let mut smells: Vec<(&str, &_)> = Vec::new();
    for f in &report.files {
        for s in &f.smells {
            smells.push((f.path.as_str(), s));
            if smells.len() >= MAX_SMELLS_IN_PROMPT {
                break;
            }
        }
        if smells.len() >= MAX_SMELLS_IN_PROMPT {
            break;
        }
    }
    if !smells.is_empty() {
        out.push_str("Top smells:\n");
        for (path, s) in &smells {
            out.push_str(&format!(
                "- {} at {}:{} (severity {})\n",
                s.category, path, s.line, s.severity,
            ));
        }
        out.push('\n');
    }

    // Existing static recommendations — feed these in so the LLM can expand
    // on them rather than reinvent them.
    let recs: Vec<&str> = report
        .files
        .iter()
        .flat_map(|f| f.recommendations.iter().map(String::as_str))
        .take(MAX_RECS_IN_PROMPT)
        .collect();
    if !recs.is_empty() {
        out.push_str("Existing static recommendations:\n");
        for r in &recs {
            out.push_str(&format!("- {r}\n"));
        }
        out.push('\n');
    }

    out.push_str(
        "Write 2-3 short paragraphs explaining why these findings matter and the prioritised \
         next steps for the developer. Reference specific files, smells, and (where relevant) \
         the detected frameworks.",
    );

    // Hint: avoid an empty grade-A report producing a confusing prompt.
    if report.files.is_empty() && report.overall_grade == ComplexityGrade::A {
        out.push_str(
            "\n\nNote: this diff is empty or has no measurable findings; \
             explain briefly that no review-worthy issues were detected.",
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::review::{FileReview, ReviewComplexity, ReviewSource, SmellHit};
    use async_trait::async_trait;
    use tokio::sync::mpsc::Sender;

    /// Stub provider: replays a fixed string as a single `Delta` then `Done`.
    ///
    /// Why: lets unit tests exercise `explain_report` without network or a
    /// real LLM.
    /// What: implements `ChatProvider` minimally; ignores messages and tools.
    /// Test: used by `explain_report_collects_stream_deltas`.
    struct StubProvider {
        text: String,
    }

    #[async_trait]
    impl ChatProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub-model"
        }
        async fn chat_stream(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDef>,
            tx: Sender<ChatEvent>,
        ) -> anyhow::Result<()> {
            tx.send(ChatEvent::Delta(self.text.clone())).await.ok();
            tx.send(ChatEvent::Done).await.ok();
            Ok(())
        }
    }

    /// Stub provider that errors mid-stream.
    struct ErrorProvider;

    #[async_trait]
    impl ChatProvider for ErrorProvider {
        fn name(&self) -> &str {
            "stub-err"
        }
        fn model(&self) -> &str {
            "stub-err"
        }
        async fn chat_stream(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<ToolDef>,
            tx: Sender<ChatEvent>,
        ) -> anyhow::Result<()> {
            tx.send(ChatEvent::Error("boom".into())).await.ok();
            Ok(())
        }
    }

    fn sample_report() -> ReviewReport {
        ReviewReport {
            files: vec![
                FileReview {
                    path: "src/big.rs".to_string(),
                    grade: ComplexityGrade::D,
                    complexity: ReviewComplexity {
                        cyclomatic: 32,
                        cognitive: 50,
                    },
                    smells: vec![SmellHit {
                        category: "long_method".into(),
                        line: 12,
                        severity: "medium".into(),
                    }],
                    recommendations: vec!["Split into helpers".into()],
                    source: ReviewSource::NewFile,
                },
                FileReview {
                    path: "src/small.rs".to_string(),
                    grade: ComplexityGrade::A,
                    complexity: ReviewComplexity {
                        cyclomatic: 1,
                        cognitive: 1,
                    },
                    smells: vec![],
                    recommendations: vec![],
                    source: ReviewSource::NewFile,
                },
            ],
            overall_grade: ComplexityGrade::D,
            changed_lines: 80,
            smell_count: 1,
            summary: "2 files analyzed".into(),
            narrative: None,
            frameworks: vec!["Next.js".into(), "React".into()],
        }
    }

    #[test]
    fn build_explain_prompt_includes_top_level_metrics() {
        let report = sample_report();
        let prompt = build_explain_prompt(&report);
        assert!(prompt.contains("Overall grade: D"), "got:\n{prompt}");
        assert!(prompt.contains("Smell count: 1"));
        assert!(prompt.contains("Changed lines: 80"));
        assert!(prompt.contains("Files reviewed: 2"));
    }

    #[test]
    fn build_explain_prompt_lists_worst_files_first() {
        let report = sample_report();
        let prompt = build_explain_prompt(&report);
        let big_idx = prompt.find("src/big.rs").expect("big.rs listed");
        let small_idx = prompt.find("src/small.rs");
        // small.rs is grade A so it may or may not be listed within
        // MAX_FILES_IN_PROMPT — but if it is, big.rs (grade D) must come
        // before it.
        if let Some(small_idx) = small_idx {
            assert!(big_idx < small_idx, "worst file must come first");
        }
    }

    #[test]
    fn build_explain_prompt_mentions_detected_frameworks() {
        let report = sample_report();
        let prompt = build_explain_prompt(&report);
        assert!(prompt.contains("Detected frameworks:"));
        assert!(prompt.contains("Next.js"));
        assert!(prompt.contains("React"));
    }

    #[test]
    fn build_explain_prompt_includes_smells_and_recommendations() {
        let report = sample_report();
        let prompt = build_explain_prompt(&report);
        assert!(prompt.contains("long_method"));
        assert!(prompt.contains("src/big.rs:12"));
        assert!(prompt.contains("Split into helpers"));
    }

    #[test]
    fn build_explain_prompt_handles_empty_report() {
        let report = ReviewReport {
            files: vec![],
            overall_grade: ComplexityGrade::A,
            changed_lines: 0,
            smell_count: 0,
            summary: "0 files".into(),
            narrative: None,
            frameworks: vec![],
        };
        let prompt = build_explain_prompt(&report);
        assert!(prompt.contains("no review-worthy issues"));
    }

    #[tokio::test]
    async fn explain_report_collects_stream_deltas() {
        let provider = StubProvider {
            text: "This change is mostly fine.".to_string(),
        };
        let report = sample_report();
        let narrative = explain_report(&report, &provider).await.unwrap();
        assert_eq!(narrative, "This change is mostly fine.");
    }

    #[tokio::test]
    async fn explain_report_surfaces_stream_error() {
        let provider = ErrorProvider;
        let report = sample_report();
        let err = explain_report(&report, &provider)
            .await
            .expect_err("error event should propagate");
        assert!(err.to_string().contains("boom"), "got: {err}");
    }
}
