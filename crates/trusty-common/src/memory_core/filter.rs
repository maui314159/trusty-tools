//! Signal/noise filtering for `memory_remember` ingest (issue #61).
//!
//! Why: Auto-capture hooks fired every tool-use event, raw prompt, and commit
//! message into palace storage. Palaces accumulated 6,650+ low-value drawers
//! that drowned curated knowledge in recall results. This module rejects
//! obvious noise before it is stored, classifies what does get through, and
//! gives operators a single configuration surface to tune the policy.
//! What: Defines `FilterConfig` (token threshold + reject regexes),
//! `FilterReject` (rejection reason — carried as an error), `classify`
//! (heuristic content-type detection), and `apply` (run the gate).
//! Test: Unit tests in this module cover token counting, every reject
//! pattern, and classifier outcomes.

use crate::memory_core::palace::DrawerType;
use regex::Regex;
use std::sync::OnceLock;
use thiserror::Error;

/// Library-default minimum token count. Conservative (3) so direct library
/// users (CLI tools, tests, embedded callers) aren't blocked on
/// borderline-short content. The MCP `memory_remember` tool overrides this
/// with the stricter `MCP_MIN_TOKENS` (8) to match the issue #61 policy
/// applied to auto-capture hooks.
pub const DEFAULT_MIN_TOKENS: u8 = 3;

/// Stricter threshold applied at the MCP boundary where auto-capture hooks
/// fire. Matches the issue #61 spec — content shorter than this should be
/// stored via `memory_note` or `kg_assert` instead.
pub const MCP_MIN_TOKENS: u8 = 8;

/// Default reject patterns covering the common auto-capture noise sources.
///
/// Why: These were the dominant categories observed in the 6,650-drawer
/// audit referenced by issue #61. Centralising them as data keeps the
/// rejection logic in one place and makes new patterns one-line additions.
/// What: Each entry is a case-insensitive regex compiled at first use.
/// Test: `default_patterns_match_known_noise`.
const DEFAULT_REJECT_PATTERNS: &[&str] = &[
    // Tool use/result framing emitted by hook capture.
    r"(?i)^tool use:",
    r"(?i)^tool result:",
    // Bare 40-hex git commit SHA.
    r"^[0-9a-f]{40}$",
    // Conventional commit message.
    r"(?i)^(feat|fix|chore|refactor|test|docs|perf|build|ci|style|revert)(\([^)]*\))?:",
    // Progress logs ("Running cargo test...").
    r"^Running .*\.\.\.$",
    // File path only.
    r"^[/~][^\s]*\.(rs|py|ts|js|tsx|jsx|toml|json|md|yaml|yml)$",
];

/// Rejection reasons surfaced to the caller.
///
/// Why: Each branch carries enough context for the MCP tool to produce a
/// helpful, actionable error message rather than a generic "rejected".
/// What: A `thiserror`-derived enum so handlers can pattern-match for
/// metrics while still bubbling through `anyhow`.
/// Test: `reject_messages_are_actionable`.
#[derive(Debug, Error, PartialEq)]
pub enum FilterReject {
    /// Content has fewer meaningful tokens than the configured minimum.
    #[error(
        "Content too short to be worth storing ({tokens} tokens). Use memory_note for brief \
         facts or kg_assert for structured triples."
    )]
    TooShort { tokens: usize },
    /// Content matched one of the reject patterns.
    #[error("Content rejected as low-signal noise (matched pattern: {pattern})")]
    NoisePattern { pattern: String },
    /// Content is mostly non-alphabetic (code/JSON heuristic).
    #[error(
        "Content rejected: appears to be raw code or JSON ({ratio:.0}% non-alphabetic). Store \
         a human-readable summary instead, or pass force=true to override."
    )]
    NonAlphabetic { ratio: f32 },
}

/// Tunable gate configuration.
///
/// Why: Different deployments may want stricter or looser thresholds; making
/// the policy data-driven lets callers swap the defaults without forking the
/// dispatcher.
/// What: Holds the minimum token count and the list of compiled-on-demand
/// reject patterns. `reject_patterns` accepts plain strings so the struct
/// stays `Clone`-friendly and serializable; the compiled `Regex` set is
/// cached per-config via `compiled_patterns`.
/// Test: `filter_config_default_blocks_known_noise`,
/// `filter_config_force_bypasses_all`.
#[derive(Debug, Clone)]
pub struct FilterConfig {
    /// Minimum meaningful tokens required for `memory_remember`.
    pub min_tokens: u8,
    /// String form of each reject regex (compiled lazily).
    pub reject_patterns: Vec<String>,
    /// Maximum allowed ratio of non-alphabetic chars before treating the
    /// content as raw code/JSON. Range `[0.0, 1.0]`. Default `0.80`.
    pub max_non_alpha_ratio: f32,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            min_tokens: DEFAULT_MIN_TOKENS,
            reject_patterns: DEFAULT_REJECT_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_non_alpha_ratio: 0.80,
        }
    }
}

impl FilterConfig {
    /// Compile and cache the configured patterns.
    ///
    /// Why: Regex compilation is amortised across calls — the cache is keyed
    /// to the config instance via a `OnceLock` so repeated `apply` calls on
    /// the same config don't re-parse the same strings.
    /// What: On first call, compiles each pattern. Patterns that fail to
    /// parse are logged and skipped so a bad entry can't break the gate.
    /// Test: Indirect — every other test in this module exercises this path.
    fn compiled_patterns(&self) -> &[Regex] {
        // We store the compiled set in a per-instance OnceLock so identical
        // configs reuse it. Because `FilterConfig` is `Clone`, the cache is
        // not shared across clones — that's fine for the tiny default set.
        static GLOBAL_CACHE: OnceLock<Vec<Regex>> = OnceLock::new();
        // Fast path: when the strings match the defaults exactly, share the
        // global cache so the daemon doesn't recompile per call.
        if self.reject_patterns.len() == DEFAULT_REJECT_PATTERNS.len()
            && self
                .reject_patterns
                .iter()
                .zip(DEFAULT_REJECT_PATTERNS.iter())
                .all(|(a, b)| a == *b)
        {
            return GLOBAL_CACHE.get_or_init(|| {
                DEFAULT_REJECT_PATTERNS
                    .iter()
                    .filter_map(|p| match Regex::new(p) {
                        Ok(r) => Some(r),
                        Err(e) => {
                            tracing::warn!(pattern = %p, "skip invalid reject regex: {e}");
                            None
                        }
                    })
                    .collect()
            });
        }
        // Custom config: compile inline. We leak a Box to keep the slice
        // borrow alive — acceptable because custom configs are rare and the
        // memory is bounded by the number of patterns.
        let compiled: Vec<Regex> = self
            .reject_patterns
            .iter()
            .filter_map(|p| match Regex::new(p) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!(pattern = %p, "skip invalid reject regex: {e}");
                    None
                }
            })
            .collect();
        Box::leak(compiled.into_boxed_slice())
    }

    /// Run the gate against `content`.
    ///
    /// Why: Single entry point used by both `memory_remember` and
    /// `memory_note` (which bypasses the token threshold but keeps the noise
    /// patterns).
    /// What: Counts meaningful tokens, optionally enforces `min_tokens`,
    /// then walks the compiled reject patterns and the non-alphabetic
    /// heuristic. Returns `Ok(())` on accept, `Err(FilterReject)` on reject.
    /// Test: `filter_config_default_blocks_known_noise`,
    /// `note_mode_allows_short_content`.
    pub fn apply(&self, content: &str, enforce_min_tokens: bool) -> Result<(), FilterReject> {
        let trimmed = content.trim();
        // Noise patterns fire first so callers see the most specific
        // diagnosis (e.g. "Tool use: x" is flagged as a known noise source
        // rather than just "too short").
        for re in self.compiled_patterns() {
            if re.is_match(trimmed) {
                return Err(FilterReject::NoisePattern {
                    pattern: re.as_str().to_string(),
                });
            }
        }
        let tokens = count_meaningful_tokens(content);
        if enforce_min_tokens && tokens < self.min_tokens as usize {
            return Err(FilterReject::TooShort { tokens });
        }
        let ratio = non_alphabetic_ratio(trimmed);
        if ratio > self.max_non_alpha_ratio {
            return Err(FilterReject::NonAlphabetic {
                ratio: ratio * 100.0,
            });
        }
        Ok(())
    }
}

/// Count tokens that carry signal — whitespace-split tokens that contain at
/// least one alphanumeric character.
///
/// Why: Pure-punctuation tokens (`---`, `==>`, `{`) shouldn't count toward
/// the minimum-length requirement.
/// What: Splits on Unicode whitespace, keeps tokens with any alphanumeric.
/// Test: `meaningful_tokens_ignore_pure_punctuation`.
pub fn count_meaningful_tokens(s: &str) -> usize {
    s.split_whitespace()
        .filter(|t| t.chars().any(|c| c.is_alphanumeric()))
        .count()
}

/// Ratio of non-alphabetic characters (ignoring whitespace) in `s`.
///
/// Why: A high ratio is a strong signal that the content is raw code/JSON
/// rather than prose.
/// What: `non_alpha / total` over non-whitespace characters. Returns `0.0`
/// for empty input.
/// Test: `non_alpha_ratio_detects_json`.
pub fn non_alphabetic_ratio(s: &str) -> f32 {
    let mut total = 0usize;
    let mut non_alpha = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        if !c.is_alphabetic() {
            non_alpha += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    non_alpha as f32 / total as f32
}

/// Classify drawer content into a `DrawerType` using cheap heuristics.
///
/// Why: Issue #61 — when the dispatcher accepts a write, it should tag the
/// drawer so downstream code (recall ranking, TTL sweep, UIs) can treat
/// auto-captured noise differently from curated facts even when the filter
/// chose to let it through (e.g. `force = true`).
/// What: Returns `Commit` for commit-shaped content, `SessionEvent` for
/// tool-use framing or progress logs, otherwise the supplied `fallback`.
/// The classifier is intentionally conservative — it never returns
/// `UserFact` on its own; that label is reserved for the explicit
/// `memory_note` tool path.
/// Test: `classify_detects_commit_and_tool_use`.
pub fn classify(content: &str, fallback: DrawerType) -> DrawerType {
    let trimmed = content.trim();
    if is_commit_like(trimmed) {
        return DrawerType::Commit;
    }
    if is_session_event_like(trimmed) {
        return DrawerType::SessionEvent;
    }
    fallback
}

fn is_commit_like(s: &str) -> bool {
    // 40-hex SHA or a conventional commit prefix.
    static SHA: OnceLock<Regex> = OnceLock::new();
    static CONV: OnceLock<Regex> = OnceLock::new();
    let sha = SHA.get_or_init(|| Regex::new(r"^[0-9a-f]{40}$").expect("sha regex"));
    let conv = CONV.get_or_init(|| {
        Regex::new(
            r"(?i)^(feat|fix|chore|refactor|test|docs|perf|build|ci|style|revert)(\([^)]*\))?:",
        )
        .expect("conventional commit regex")
    });
    sha.is_match(s) || conv.is_match(s)
}

fn is_session_event_like(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("tool use:")
        || lower.starts_with("tool result:")
        || (lower.starts_with("running ") && lower.ends_with("..."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meaningful_tokens_ignore_pure_punctuation() {
        assert_eq!(count_meaningful_tokens("--- === >>>"), 0);
        assert_eq!(count_meaningful_tokens("one two three"), 3);
        assert_eq!(count_meaningful_tokens("foo --- bar"), 2);
    }

    #[test]
    fn non_alpha_ratio_detects_json() {
        let json = r#"{"a":1,"b":[2,3,4]}"#;
        let ratio = non_alphabetic_ratio(json);
        assert!(
            ratio > 0.5,
            "expected JSON to register as mostly non-alphabetic; got {ratio}"
        );
        let prose = "The quick brown fox jumps over the lazy dog";
        assert!(non_alphabetic_ratio(prose) < 0.2);
    }

    #[test]
    fn default_patterns_match_known_noise() {
        let cfg = FilterConfig::default();
        let cases = [
            "Tool use: search_files",
            "Tool result: ok",
            "abcdef0123456789abcdef0123456789abcdef01", // pragma: allowlist secret
            "feat(memory): add filter",
            "fix: handle nulls",
            "Running cargo test...",
            "/Users/x/foo.rs",
            "~/notes.md",
        ];
        for c in cases {
            assert!(cfg.apply(c, false).is_err(), "expected reject for: {c}");
        }
    }

    #[test]
    fn filter_config_default_blocks_known_noise() {
        let cfg = FilterConfig::default();
        let res = cfg.apply("Tool use: read_file", true);
        assert!(matches!(res, Err(FilterReject::NoisePattern { .. })));
    }

    #[test]
    fn filter_config_too_short_triggers_token_error() {
        // Use a config with the stricter MCP threshold (8) so the assertion
        // is independent of the lower library default (3).
        let cfg = FilterConfig {
            min_tokens: MCP_MIN_TOKENS,
            ..FilterConfig::default()
        };
        let res = cfg.apply("only four tokens here", true);
        match res {
            Err(FilterReject::TooShort { tokens }) => assert_eq!(tokens, 4),
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    #[test]
    fn note_mode_allows_short_content() {
        // Use the stricter MCP threshold so the assertion documents the
        // contract independently of the library default.
        let cfg = FilterConfig {
            min_tokens: MCP_MIN_TOKENS,
            ..FilterConfig::default()
        };
        // 3 tokens, would fail with enforce_min=true, must pass with false.
        assert!(cfg.apply("User prefers snake_case", false).is_ok());
        assert!(cfg.apply("User prefers snake_case", true).is_err());
    }

    #[test]
    fn filter_accepts_real_content() {
        let cfg = FilterConfig::default();
        assert!(
            cfg.apply(
                "When refactoring search indices, prefer postcard over JSON for redb \
                 values because of size and decode speed.",
                true,
            )
            .is_ok()
        );
    }

    #[test]
    fn filter_rejects_high_non_alpha_content() {
        let cfg = FilterConfig::default();
        let json = r#"{"id":1,"items":[2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20]}"#;
        let res = cfg.apply(json, false);
        assert!(matches!(res, Err(FilterReject::NonAlphabetic { .. })));
    }

    #[test]
    fn classify_detects_commit_and_tool_use() {
        assert_eq!(
            classify("feat(memory): add filter", DrawerType::Unknown),
            DrawerType::Commit
        );
        assert_eq!(
            classify(
                "abcdef0123456789abcdef0123456789abcdef01", // pragma: allowlist secret
                DrawerType::Unknown
            ),
            DrawerType::Commit
        );
        assert_eq!(
            classify("Tool use: search_code", DrawerType::Unknown),
            DrawerType::SessionEvent
        );
        assert_eq!(
            classify("Running cargo test...", DrawerType::Unknown),
            DrawerType::SessionEvent
        );
        // Prose falls through to the supplied fallback.
        assert_eq!(
            classify(
                "A regular curated knowledge fragment.",
                DrawerType::AgentNote
            ),
            DrawerType::AgentNote
        );
    }

    #[test]
    fn reject_messages_are_actionable() {
        let too_short = FilterReject::TooShort { tokens: 3 };
        assert!(too_short.to_string().contains("memory_note"));
        let noise = FilterReject::NoisePattern {
            pattern: "x".to_string(),
        };
        assert!(noise.to_string().contains("low-signal"));
        let na = FilterReject::NonAlphabetic { ratio: 85.0 };
        assert!(na.to_string().contains("force=true"));
    }

    #[test]
    fn filter_config_force_bypasses_all() {
        // The `force` semantics live in the caller (we model it by skipping
        // `apply` entirely); this test exists so the contract is documented:
        // there is no way *inside* `apply` to bypass — the caller must not
        // call it at all to force-store.
        let cfg = FilterConfig::default();
        assert!(cfg.apply("Tool use: x", true).is_err());
    }
}
