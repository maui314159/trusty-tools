//! Deterministic NLP prompt compression.
//!
//! Why: Long prompts waste tokens; compressing at API call construction time
//! lets us cut cost without mutating stored originals.
//! What: A pipeline that extracts code/JSON blocks, deduplicates sections,
//! strips stop words and discourse markers, applies a simple TF-IDF filter,
//! and optionally truncates to a token budget.
//! Test: Same input always produces same output (deterministic); code blocks
//! are preserved verbatim; negations like "do not" are never stripped.

pub mod context;
pub mod dedup;
pub mod history;
pub mod output_prompt;
pub mod session;
pub mod tool_output;

pub use context::{TokenBudget, truncate_history};
pub use dedup::dedup_sections;
pub use output_prompt::{OutputStyle, output_compression_prompt};
pub use tool_output::{compress_tool_output, compress_tool_output_async};

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;

/// Configuration for the compression pipeline.
#[derive(Debug, Clone)]
pub struct CompressConfig {
    /// Enable stop-word removal (default: true).
    pub remove_stop_words: bool,
    /// Enable TF-IDF sentence filtering (default: true).
    pub tfidf_filter: bool,
    /// Enable discourse marker removal (default: true).
    pub strip_discourse_markers: bool,
    /// Sentences scoring below this are dropped (0.0-1.0, default: 0.1).
    pub tfidf_threshold: f64,
    /// Optional hard token budget — truncate to this many tokens if set.
    pub token_budget: Option<usize>,
}

impl Default for CompressConfig {
    fn default() -> Self {
        Self {
            remove_stop_words: true,
            tfidf_filter: true,
            strip_discourse_markers: true,
            tfidf_threshold: 0.1,
            token_budget: None,
        }
    }
}

/// Result of compression with metrics.
#[derive(Debug, Clone)]
pub struct CompressResult {
    pub text: String,
    pub original_len: usize,
    pub compressed_len: usize,
    pub reduction_pct: f64,
}

/// Main compression entry point.
///
/// Why: One-call entry for callers that want the full pipeline.
/// What: Protects code blocks, deduplicates, strips stop words / discourse
/// markers, applies TF-IDF filtering, optionally truncates to a token budget.
/// Test: `test_compress_is_deterministic` — same input yields same output.
pub fn compress(input: &str, config: &CompressConfig) -> CompressResult {
    let original_len = input.len();
    if input.is_empty() {
        return CompressResult {
            text: String::new(),
            original_len: 0,
            compressed_len: 0,
            reduction_pct: 0.0,
        };
    }

    // 1. Protect code/JSON blocks.
    let (mut text, blocks) = extract_code_blocks(input);

    // 2. Deduplicate repeated paragraphs.
    text = deduplicate_sections(&text);

    // 3. Strip discourse markers.
    if config.strip_discourse_markers {
        text = strip_discourse_markers(&text);
    }

    // 4. TF-IDF sentence filtering.
    if config.tfidf_filter {
        text = tfidf_filter_sentences(&text, config.tfidf_threshold);
    }

    // 5. Stop-word removal (negation-aware).
    if config.remove_stop_words {
        text = remove_stop_words(&text);
    }

    // 6. Restore protected blocks.
    text = restore_code_blocks(&text, &blocks);

    // 7. Truncate to token budget if set.
    if let Some(budget) = config.token_budget {
        text = truncate_to_budget(&text, budget);
    }

    let compressed_len = text.len();
    let reduction_pct = if original_len > 0 {
        (1.0 - compressed_len as f64 / original_len as f64) * 100.0
    } else {
        0.0
    };

    CompressResult {
        text,
        original_len,
        compressed_len,
        reduction_pct,
    }
}

// -------- Code-block extraction --------

static FENCED_BLOCK: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)```[^\n]*\n.*?\n```").unwrap());
static INLINE_CODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"`[^`\n]+`").unwrap());

/// Extract code blocks and inline code; replace with sentinels.
///
/// Why: Compression must never mangle code — a stripped stop-word inside a
/// function name is a correctness bug.
/// What: Replaces fenced blocks and inline code with numbered placeholders,
/// returns the (text, placeholder_pairs) tuple so the caller can restore.
/// Test: `test_extract_code_blocks_roundtrip` — extract + restore == identity.
fn extract_code_blocks(text: &str) -> (String, Vec<(String, String)>) {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut counter = 0usize;

    // Fenced blocks first (longest match; inline regex won't catch multi-line).
    let after_fenced = FENCED_BLOCK
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let placeholder = format!("\u{E000}CODE_BLOCK_{counter}\u{E001}");
            blocks.push((placeholder.clone(), caps[0].to_string()));
            counter += 1;
            placeholder
        })
        .into_owned();

    // Inline code next.
    let after_inline = INLINE_CODE
        .replace_all(&after_fenced, |caps: &regex::Captures<'_>| {
            let placeholder = format!("\u{E000}INLINE_CODE_{counter}\u{E001}");
            blocks.push((placeholder.clone(), caps[0].to_string()));
            counter += 1;
            placeholder
        })
        .into_owned();

    (after_inline, blocks)
}

/// Restore protected blocks from placeholders.
fn restore_code_blocks(text: &str, blocks: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (placeholder, original) in blocks {
        out = out.replace(placeholder.as_str(), original);
    }
    out
}

// -------- Deduplication --------

/// Hash-based deduplication of repeated paragraphs.
///
/// Why: LLM prompts often contain stock phrasing repeated across sections.
/// What: Splits on blank lines, keeps the first occurrence of each unique
/// (whitespace-normalized) paragraph, preserving order.
/// Test: `test_compress_deduplicates_repeated_sections`.
fn deduplicate_sections(text: &str) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<&str> = Vec::new();
    for section in text.split("\n\n") {
        let key: String = section.split_whitespace().collect::<Vec<_>>().join(" ");
        if key.is_empty() {
            kept.push(section);
            continue;
        }
        if seen.insert(key) {
            kept.push(section);
        }
    }
    kept.join("\n\n")
}

// -------- Stop words (negation-aware) --------

/// Negation terms we must never strip, even when they're stop words.
static NEGATIONS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "not",
        "no",
        "never",
        "cannot",
        "can't",
        "don't",
        "won't",
        "isn't",
        "aren't",
        "doesn't",
        "didn't",
        "wasn't",
        "weren't",
        "shouldn't",
        "wouldn't",
        "couldn't",
        "mustn't",
    ]
    .into_iter()
    .collect()
});

static STOP_WORDS: Lazy<HashSet<String>> = Lazy::new(|| {
    let raw = stop_words::get(stop_words::LANGUAGE::English);
    raw.into_iter().map(|s| s.to_lowercase()).collect()
});

/// Remove stop words from prose while preserving negations and the words
/// that follow them (so "do not call" keeps all three tokens).
///
/// Why: Stop words are the biggest compressible surface; removing them
/// naively destroys meaning when negations are involved.
/// What: Tokenizes on whitespace, skips stop words unless (a) the token is
/// a negation, (b) the previous token was a negation, or (c) the token is
/// inside a code-block placeholder.
/// Test: `test_compress_preserves_negations`.
fn remove_stop_words(text: &str) -> String {
    // Split by whitespace but preserve newline grouping by iterating line-by-line.
    let mut out_lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let mut kept: Vec<&str> = Vec::with_capacity(tokens.len());
        let mut prev_was_negation = false;
        for tok in tokens {
            let lower = tok.to_lowercase();
            let stripped: String = lower
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect();

            // Preserve placeholders verbatim.
            if tok.contains('\u{E000}') || tok.contains('\u{E001}') {
                kept.push(tok);
                prev_was_negation = false;
                continue;
            }

            let is_negation = NEGATIONS.contains(stripped.as_str());
            if is_negation {
                kept.push(tok);
                prev_was_negation = true;
                continue;
            }

            if prev_was_negation {
                kept.push(tok);
                prev_was_negation = false;
                continue;
            }

            if STOP_WORDS.contains(&stripped) {
                prev_was_negation = false;
                continue;
            }

            kept.push(tok);
            prev_was_negation = false;
        }
        out_lines.push(kept.join(" "));
    }
    out_lines.join("\n")
}

// -------- Discourse markers --------

static DISCOURSE_MARKERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(furthermore|moreover|additionally|in addition|as mentioned|as noted|it is worth noting that|it should be noted that|please note that)\b[,]?\s*",
    )
    .unwrap()
});

/// Strip low-value discourse markers.
fn strip_discourse_markers(text: &str) -> String {
    DISCOURSE_MARKERS.replace_all(text, "").into_owned()
}

// -------- TF-IDF sentence filtering --------

/// Split text into sentences on `. `, `! `, `? ` boundaries.
///
/// Why: Rust's `regex` crate has no lookbehind, so we walk the string
/// manually.
fn split_sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if (b == b'.' || b == b'!' || b == b'?')
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_whitespace()
        {
            // Include the punctuation in the current sentence.
            let end = i + 1;
            out.push(&text[start..end]);
            // Skip whitespace to next sentence start.
            let mut j = end;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            start = j;
            i = j;
            continue;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push(&text[start..]);
    }
    out
}

/// Score each sentence by average TF-IDF over its tokens and drop sentences
/// scoring below threshold. Always keeps the first sentence of each paragraph
/// as a structural anchor.
///
/// Why: Removes "filler" sentences that restate material without adding signal.
/// What: Simple TF-IDF where each sentence is a document. Score = mean weight
/// of its non-stop-word tokens. Below-threshold sentences are dropped.
/// Test: `test_compress_reduces_tokens`.
fn tfidf_filter_sentences(text: &str, threshold: f64) -> String {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut out_paragraphs: Vec<String> = Vec::with_capacity(paragraphs.len());

    for para in paragraphs {
        let sentences: Vec<&str> = split_sentences(para);
        if sentences.len() <= 1 {
            out_paragraphs.push(para.to_string());
            continue;
        }

        // Build TF-IDF weights.
        let docs: Vec<Vec<String>> = sentences
            .iter()
            .map(|s| {
                s.split_whitespace()
                    .map(|w| {
                        w.chars()
                            .filter(|c| c.is_alphanumeric())
                            .collect::<String>()
                            .to_lowercase()
                    })
                    .filter(|w| !w.is_empty() && !STOP_WORDS.contains(w))
                    .collect()
            })
            .collect();

        let n_docs = docs.len() as f64;
        let mut df: HashMap<String, usize> = HashMap::new();
        for doc in &docs {
            let unique: HashSet<&String> = doc.iter().collect();
            for term in unique {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
        }

        let scores: Vec<f64> = docs
            .iter()
            .map(|doc| {
                if doc.is_empty() {
                    return 0.0;
                }
                let mut tf: HashMap<&String, usize> = HashMap::new();
                for t in doc {
                    *tf.entry(t).or_insert(0) += 1;
                }
                let doc_len = doc.len() as f64;
                let mut sum = 0.0f64;
                for (term, count) in &tf {
                    let tf_val = *count as f64 / doc_len;
                    let df_val = *df.get(*term).unwrap_or(&1) as f64;
                    let idf = (n_docs / df_val).ln().max(0.0) + 1.0;
                    sum += tf_val * idf;
                }
                sum / doc.len() as f64
            })
            .collect();

        // Normalize scores to [0, 1] for threshold comparison.
        let max_score = scores.iter().cloned().fold(0.0f64, f64::max).max(1e-9);
        let kept: Vec<&str> = sentences
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let normalized = scores[i] / max_score;
                if i == 0 || normalized >= threshold {
                    Some(*s)
                } else {
                    None
                }
            })
            .collect();

        out_paragraphs.push(kept.join(" "));
    }

    out_paragraphs.join("\n\n")
}

// -------- Token estimation and truncation --------

/// Estimate approximate tokens (word count * 1.3).
///
/// Why: Cheap heuristic avoids depending on tiktoken at compile time while
/// staying within ~10% of the real count for English prose.
/// What: `words * 1.3` rounded down.
/// Test: `test_estimate_tokens_rough`.
pub fn estimate_tokens(text: &str) -> usize {
    (text.split_whitespace().count() as f64 * 1.3) as usize
}

/// Truncate text to fit within `budget` tokens, preserving sentence boundaries
/// where possible.
fn truncate_to_budget(text: &str, budget: usize) -> String {
    if estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let sentences: Vec<&str> = split_sentences(text);
    let mut acc = String::new();
    let mut acc_tokens = 0usize;
    for s in sentences {
        let s_tokens = estimate_tokens(s);
        if acc_tokens + s_tokens > budget {
            break;
        }
        if !acc.is_empty() {
            acc.push(' ');
        }
        acc.push_str(s);
        acc_tokens += s_tokens;
    }
    if acc.is_empty() {
        // Fall back to word-level truncation if no sentence fits.
        let words: Vec<&str> = text.split_whitespace().collect();
        let take = ((budget as f64) / 1.3) as usize;
        words.into_iter().take(take).collect::<Vec<_>>().join(" ")
    } else {
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_empty_string() {
        let result = compress("", &CompressConfig::default());
        assert!(result.text.is_empty());
        assert_eq!(result.original_len, 0);
        assert_eq!(result.compressed_len, 0);
    }

    #[test]
    fn test_compress_preserves_code_blocks() {
        let input = "Some prose here.\n\n```rust\nfn main() { let x = 1; }\n```\n\nMore prose.";
        let result = compress(input, &CompressConfig::default());
        assert!(
            result.text.contains("fn main() { let x = 1; }"),
            "code body must be preserved verbatim, got: {}",
            result.text
        );
    }

    #[test]
    fn test_compress_deduplicates_repeated_sections() {
        let para = "The quick brown fox jumps over the lazy dog";
        let input = format!("{para}\n\n{para}\n\n{para}");
        let result = compress(
            &input,
            &CompressConfig {
                remove_stop_words: false,
                tfidf_filter: false,
                strip_discourse_markers: false,
                ..CompressConfig::default()
            },
        );
        // Only one occurrence of the paragraph should remain.
        let count = result.text.matches("quick brown").count();
        assert_eq!(
            count, 1,
            "expected 1 occurrence, got {count} in: {}",
            result.text
        );
    }

    #[test]
    fn test_compress_preserves_negations() {
        let input = "You must do not call this function directly.";
        let result = compress(
            input,
            &CompressConfig {
                tfidf_filter: false,
                strip_discourse_markers: false,
                ..CompressConfig::default()
            },
        );
        assert!(
            result.text.to_lowercase().contains("not"),
            "negation dropped: {}",
            result.text
        );
    }

    #[test]
    fn test_compress_strips_discourse_markers() {
        let input = "Furthermore, the system should validate input carefully.";
        let result = compress(
            input,
            &CompressConfig {
                remove_stop_words: false,
                tfidf_filter: false,
                ..CompressConfig::default()
            },
        );
        assert!(
            !result.text.to_lowercase().contains("furthermore"),
            "discourse marker not stripped: {}",
            result.text
        );
    }

    #[test]
    fn test_compress_reduces_tokens() {
        let input = "This is a very long paragraph about the system. \
            It contains many words that should be removed by the stop word filter. \
            Furthermore, there are also discourse markers in this text. \
            Moreover, the system is indeed quite verbose by design.";
        let result = compress(input, &CompressConfig::default());
        assert!(
            result.compressed_len < result.original_len,
            "compression did not reduce size: {} -> {}",
            result.original_len,
            result.compressed_len
        );
    }

    #[test]
    fn test_compress_is_deterministic() {
        let input = "The quick brown fox jumps over the lazy dog. \
            Additionally, the dog was not amused by this turn of events.";
        let a = compress(input, &CompressConfig::default());
        let b = compress(input, &CompressConfig::default());
        assert_eq!(a.text, b.text);
    }

    #[test]
    fn test_extract_code_blocks_roundtrip() {
        let input = "Before.\n\n```py\nprint('hi')\n```\n\nBetween `inline` and after.";
        let (stripped, blocks) = extract_code_blocks(input);
        let restored = restore_code_blocks(&stripped, &blocks);
        assert_eq!(restored, input);
    }

    #[test]
    fn test_estimate_tokens_rough() {
        let text: String = std::iter::repeat_n("word", 100)
            .collect::<Vec<_>>()
            .join(" ");
        let est = estimate_tokens(&text);
        assert!((120..=140).contains(&est), "estimate out of range: {est}");
    }

    #[test]
    fn test_truncate_to_budget() {
        let input = "Sentence one. Sentence two is here. Sentence three finishes it.";
        let result = compress(
            input,
            &CompressConfig {
                remove_stop_words: false,
                tfidf_filter: false,
                strip_discourse_markers: false,
                token_budget: Some(5),
                ..CompressConfig::default()
            },
        );
        assert!(estimate_tokens(&result.text) <= 6);
    }
}
