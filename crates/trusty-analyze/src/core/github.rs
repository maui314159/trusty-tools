//! GitHub REST API integration for PR review.
//!
//! Why: PR review is the highest-leverage moment to catch complexity and
//! smells. This module lets the analyzer fetch a PR's unified diff straight
//! from GitHub, run the existing review pipeline against it, and (optionally)
//! post the resulting report back as a PR comment — without the caller having
//! to shell out to `git` or `gh`.
//!
//! What: three thin async helpers wrapping the GitHub REST API
//! ([`fetch_pr_diff`], [`post_pr_comment`]) plus a pure markdown renderer
//! ([`format_review_as_markdown`]) that turns a [`ReviewReport`] into a
//! human-readable PR comment. The webhook signature verifier
//! ([`verify_webhook_signature`]) lives here too so all GitHub-facing code is
//! in one place.
//!
//! Test: see `mod tests` — covers request-shape errors, markdown rendering for
//! empty/populated reports, and HMAC signature verification.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::core::review::ReviewReport;

/// User-Agent sent on every GitHub API request. GitHub rejects requests that
/// omit a User-Agent header, so this is mandatory.
const USER_AGENT: &str = "trusty-analyze";

/// Errors raised while talking to the GitHub API.
///
/// Why: keeps the GitHub transport layer typed (`thiserror`) so callers can
/// distinguish a missing token from a network failure or a non-2xx response.
/// What: `Transport` wraps reqwest failures; `Api` carries a non-2xx status
/// and the response body; `MissingToken` flags an absent `GITHUB_TOKEN`.
/// Test: `fetch_pr_diff` against an unreachable host yields `Transport`.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// A `GITHUB_TOKEN` was required but not provided.
    #[error("GITHUB_TOKEN is not set; a GitHub token is required for this operation")]
    MissingToken,
    /// The HTTP request itself failed (DNS, connect, timeout, ...).
    #[error("github request failed: {0}")]
    Transport(String),
    /// GitHub returned a non-2xx status.
    #[error("github API returned {status}: {body}")]
    Api { status: u16, body: String },
}

/// Request body for `POST /review/github-pr` and the `review_github_pr` MCP
/// tool.
///
/// Why: a single typed shape shared by the HTTP handler and the MCP dispatch
/// path so the two transports stay in lockstep.
/// What: identifies a PR by `owner`/`repo`/`pr`, names the trusty-search index
/// to cross-reference, and carries an opt-in `post_comment` flag.
/// Test: serde round-trip in `mod tests`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GithubPrRequest {
    /// Repository owner (user or org).
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull-request number.
    pub pr: u64,
    /// trusty-search index ID to cross-reference the diff against.
    pub index_id: String,
    /// When true, the rendered review is posted back as a PR comment.
    #[serde(default)]
    pub post_comment: bool,
}

/// Fetch the unified diff for a pull request.
///
/// Why: the review pipeline takes a unified diff string; this is how we obtain
/// one for a remote PR without cloning the repo.
/// What: `GET /repos/{owner}/{repo}/pulls/{pr}` with the
/// `application/vnd.github.v3.diff` Accept header, which makes GitHub return
/// the raw diff as the response body.
/// Test: `fetch_pr_diff` against `127.0.0.1:1` returns `GithubError::Transport`.
pub async fn fetch_pr_diff(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    pr: u64,
    token: &str,
) -> Result<String, GithubError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/pulls/{pr}");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3.diff")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| GithubError::Transport(format!("GET {url}: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| GithubError::Transport(format!("read body of {url}: {e}")))?;
    if !status.is_success() {
        return Err(GithubError::Api {
            status: status.as_u16(),
            body,
        });
    }
    Ok(body)
}

/// Post a comment on a pull request's conversation thread.
///
/// Why: lets the analyzer publish its review back to the PR so reviewers see
/// it inline. PR comments use the *issues* comments endpoint because every PR
/// is also an issue in GitHub's data model.
/// What: `POST /repos/{owner}/{repo}/issues/{pr}/comments` with a JSON body of
/// `{"body": <markdown>}`.
/// Test: `post_pr_comment` against `127.0.0.1:1` returns `GithubError::Transport`.
pub async fn post_pr_comment(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    pr: u64,
    body: &str,
    token: &str,
) -> Result<(), GithubError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/issues/{pr}/comments");
    let resp = client
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await
        .map_err(|e| GithubError::Transport(format!("POST {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(GithubError::Api {
            status: status.as_u16(),
            body: text,
        });
    }
    Ok(())
}

/// Render a [`ReviewReport`] as a GitHub-flavoured markdown PR comment.
///
/// Why: the comment posted back to a PR should be scannable by a human — a
/// table of per-file grades plus a bullet list of smells, not raw JSON.
/// What: builds a heading, a summary line, a per-file table, and a smell list.
/// Pure (no I/O), so it is trivially unit-testable.
/// Test: `markdown_renders_summary_and_files` and `markdown_handles_empty_report`.
pub fn format_review_as_markdown(report: &ReviewReport) -> String {
    let mut out = String::new();
    out.push_str("## 🔍 trusty-analyze Review\n\n");
    out.push_str(&format!(
        "**Overall grade: {}** | {} changed lines | {} smell{}\n\n",
        report.overall_grade,
        report.changed_lines,
        report.smell_count,
        if report.smell_count == 1 { "" } else { "s" },
    ));

    if report.files.is_empty() {
        out.push_str("_No files changed._\n");
    } else {
        out.push_str("### Files\n\n");
        out.push_str("| File | Grade | Cyclomatic | Cognitive | Smells |\n");
        out.push_str("|------|-------|-----------|---------|--------|\n");
        for f in &report.files {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                f.path,
                f.grade,
                f.complexity.cyclomatic,
                f.complexity.cognitive,
                f.smells.len(),
            ));
        }
        out.push('\n');

        let any_smells = report.files.iter().any(|f| !f.smells.is_empty());
        if any_smells {
            out.push_str("### Smells\n\n");
            for f in &report.files {
                for s in &f.smells {
                    out.push_str(&format!(
                        "- `{}:{}` — **{}** ({})\n",
                        f.path, s.line, s.category, s.severity,
                    ));
                }
            }
            out.push('\n');
        }
    }

    out.push_str("---\n");
    out.push_str("*Generated by [trusty-analyze](https://github.com/bobmatnyc/trusty-analyze)*\n");
    out
}

/// Verify a GitHub webhook's `X-Hub-Signature-256` HMAC.
///
/// Why: webhook payloads are unauthenticated by default; the shared-secret
/// HMAC is the only thing proving a request actually came from GitHub.
/// What: computes `HMAC-SHA256(secret, body)` and constant-time compares its
/// hex digest against the `sha256=<hex>` signature header.
/// Test: `webhook_signature_accepts_valid` / `webhook_signature_rejects_invalid`.
pub fn verify_webhook_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::review::{FileReview, ReviewComplexity, ReviewSource, SmellHit};
    use crate::types::complexity::ComplexityGrade;

    fn sample_report() -> ReviewReport {
        ReviewReport {
            files: vec![FileReview {
                path: "src/foo.rs".into(),
                grade: ComplexityGrade::B,
                complexity: ReviewComplexity {
                    cyclomatic: 12,
                    cognitive: 8,
                },
                smells: vec![SmellHit {
                    category: "long_method".into(),
                    line: 42,
                    severity: "medium".into(),
                }],
                recommendations: vec!["extract a helper".into()],
                source: ReviewSource::NewFile,
            }],
            overall_grade: ComplexityGrade::B,
            changed_lines: 143,
            smell_count: 1,
            summary: "1 file analyzed".into(),
            narrative: None,
            frameworks: Vec::new(),
        }
    }

    #[test]
    fn github_pr_request_round_trips_json() {
        let req = GithubPrRequest {
            owner: "bobmatnyc".into(),
            repo: "trusty-analyze".into(),
            pr: 12,
            index_id: "idx".into(),
            post_comment: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: GithubPrRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn github_pr_request_post_comment_defaults_false() {
        let req: GithubPrRequest =
            serde_json::from_str(r#"{"owner":"o","repo":"r","pr":1,"index_id":"i"}"#).unwrap();
        assert!(!req.post_comment);
    }

    #[test]
    fn markdown_renders_summary_and_files() {
        let md = format_review_as_markdown(&sample_report());
        assert!(md.contains("## 🔍 trusty-analyze Review"));
        assert!(md.contains("Overall grade: B"));
        assert!(md.contains("143 changed lines"));
        assert!(md.contains("1 smell"));
        assert!(md.contains("| `src/foo.rs` | B | 12 | 8 | 1 |"));
        assert!(md.contains("`src/foo.rs:42` — **long_method** (medium)"));
        assert!(md.contains("Generated by [trusty-analyze]"));
    }

    #[test]
    fn markdown_handles_empty_report() {
        let report = ReviewReport {
            files: vec![],
            overall_grade: ComplexityGrade::A,
            changed_lines: 0,
            smell_count: 0,
            summary: "nothing".into(),
            narrative: None,
            frameworks: Vec::new(),
        };
        let md = format_review_as_markdown(&report);
        assert!(md.contains("Overall grade: A"));
        assert!(md.contains("0 smells"));
        assert!(md.contains("_No files changed._"));
    }

    #[test]
    fn webhook_signature_accepts_valid() {
        let secret = "test-hmac-key"; // pragma: allowlist secret
        let body = br#"{"action":"opened"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let digest = hex::encode(mac.finalize().into_bytes());
        let header = format!("sha256={digest}");
        assert!(verify_webhook_signature(secret, body, &header));
    }

    #[test]
    fn webhook_signature_rejects_invalid() {
        let body = br#"{"action":"opened"}"#;
        assert!(!verify_webhook_signature("secret", body, "sha256=deadbeef"));
        // Missing prefix.
        assert!(!verify_webhook_signature("secret", body, "deadbeef"));
        // Wrong secret.
        let mut mac = Hmac::<Sha256>::new_from_slice(b"other").unwrap();
        mac.update(body);
        let digest = hex::encode(mac.finalize().into_bytes());
        assert!(!verify_webhook_signature(
            "secret",
            body,
            &format!("sha256={digest}")
        ));
    }
}
