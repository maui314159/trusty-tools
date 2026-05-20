//! `LanguageAnalyzer` plugin trait and shared result type.
//!
//! Why: All per-language adapters expose the same surface so the
//! `AnalyzerRegistry` can dispatch chunks to them uniformly without caring
//! about which grammar is in use.
//!
//! What: A small trait with `analyze_chunks` as the workhorse, plus a
//! `StaticAnalysisResult` carrier that bundles the produced `KgGraph` with
//! diagnostic counters.
//!
//! Test: `LanguageAnalyzer::supports` default impl is covered by each
//! adapter's tests.

use crate::types::{CodeChunk, KgGraph};

/// Result of static analysis on a set of chunks.
#[derive(Debug, Default)]
pub struct StaticAnalysisResult {
    /// Extracted knowledge graph.
    pub graph: KgGraph,
    /// How many distinct files were touched.
    pub analyzed_files: usize,
    /// How many chunks were processed.
    pub analyzed_chunks: usize,
    /// Non-fatal parse errors (e.g. tree-sitter ERROR nodes).
    pub errors: Vec<String>,
}

/// Plugin interface: one implementation per supported language.
pub trait LanguageAnalyzer: Send + Sync {
    /// Stable identifier, e.g. `"rust"`.
    fn language(&self) -> &str;

    /// File extensions this adapter consumes, including the leading dot.
    fn supported_extensions(&self) -> &[&str];

    /// Build a knowledge graph from the given chunks. Implementations parse
    /// each chunk's `content` with their language grammar and extract nodes
    /// and edges into the shared `KgGraph` schema.
    fn analyze_chunks(&self, chunks: &[CodeChunk]) -> StaticAnalysisResult;

    /// Whether this analyzer supports the given file path. The default
    /// implementation does a case-insensitive extension match.
    fn supports(&self, file: &str) -> bool {
        let lower = file.to_lowercase();
        self.supported_extensions()
            .iter()
            .any(|ext| lower.ends_with(ext))
    }
}
