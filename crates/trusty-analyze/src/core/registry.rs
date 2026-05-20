//! `AnalyzerRegistry`: dispatches `CodeChunk`s to the right `LanguageAnalyzer`.
//!
//! Why: The service crate doesn't want to know which adapters exist — it
//! just wants to call `registry.analyze(&chunks)` and get back a merged
//! `StaticAnalysisResult`. This module owns the list of built-in adapters
//! and the file-extension-based routing.
//!
//! What: A thin facade over `Vec<Arc<dyn LanguageAnalyzer>>`. The
//! `default_registry` constructor registers every adapter currently
//! shipping. `analyze` partitions chunks by adapter and merges the
//! per-language results.
//!
//! Test: `default_registry_constructs` ensures we can build the registry
//! without panicking and that every advertised adapter is present.

use std::sync::Arc;

use crate::core::facts::{new_fact, FactStore};
use crate::lang::{
    detect_frameworks, CAnalyzer, CSharpAnalyzer, CppAnalyzer, GoAnalyzer, JavaAnalyzer,
    JavaScriptAnalyzer, KotlinAnalyzer, LanguageAnalyzer, PhpAnalyzer, PythonAnalyzer,
    RubyAnalyzer, RustAnalyzer, ScalaAnalyzer, StaticAnalysisResult, SwiftAnalyzer,
    TypeScriptAnalyzer,
};
use crate::types::CodeChunk;
use std::path::Path;

/// Holds all registered language analyzers and dispatches to the right one.
pub struct AnalyzerRegistry {
    analyzers: Vec<Arc<dyn LanguageAnalyzer>>,
}

impl AnalyzerRegistry {
    /// Create a registry with all built-in adapters registered.
    pub fn default_registry() -> Self {
        let analyzers: Vec<Arc<dyn LanguageAnalyzer>> = vec![
            Arc::new(RustAnalyzer::new()),
            Arc::new(TypeScriptAnalyzer::new()),
            Arc::new(JavaScriptAnalyzer::new()),
            Arc::new(PythonAnalyzer::new()),
            Arc::new(JavaAnalyzer::new()),
            Arc::new(GoAnalyzer::new()),
            Arc::new(CAnalyzer::new()),
            Arc::new(CppAnalyzer::new()),
            Arc::new(CSharpAnalyzer::new()),
            Arc::new(KotlinAnalyzer::new()),
            Arc::new(PhpAnalyzer::new()),
            Arc::new(RubyAnalyzer::new()),
            Arc::new(ScalaAnalyzer::new()),
            Arc::new(SwiftAnalyzer::new()),
        ];
        Self { analyzers }
    }

    /// Empty registry (useful for tests).
    pub fn empty() -> Self {
        Self {
            analyzers: Vec::new(),
        }
    }

    /// Register an additional analyzer (e.g. a custom plugin).
    pub fn register(&mut self, analyzer: Arc<dyn LanguageAnalyzer>) {
        self.analyzers.push(analyzer);
    }

    /// Iterate over all registered analyzers.
    pub fn analyzers(&self) -> &[Arc<dyn LanguageAnalyzer>] {
        &self.analyzers
    }

    /// Get the first analyzer that claims to support `file`, if any.
    pub fn analyzer_for(&self, file: &str) -> Option<Arc<dyn LanguageAnalyzer>> {
        self.analyzers.iter().find(|a| a.supports(file)).cloned()
    }

    /// Analyze a corpus by dispatching each chunk to the matching adapter
    /// by file extension. Chunks with no matching adapter are skipped and
    /// logged at debug level. Results from every adapter are merged.
    pub fn analyze(&self, chunks: &[CodeChunk]) -> StaticAnalysisResult {
        let mut by_adapter: Vec<(Arc<dyn LanguageAnalyzer>, Vec<CodeChunk>)> = self
            .analyzers
            .iter()
            .map(|a| (a.clone(), Vec::new()))
            .collect();

        let mut unrouted = 0usize;
        'outer: for chunk in chunks {
            for slot in by_adapter.iter_mut() {
                if slot.0.supports(&chunk.file) {
                    slot.1.push(chunk.clone());
                    continue 'outer;
                }
            }
            unrouted += 1;
        }

        if unrouted > 0 {
            tracing::debug!("AnalyzerRegistry: {unrouted} chunks had no matching language adapter");
        }

        let mut merged = StaticAnalysisResult::default();
        for (adapter, chunks) in by_adapter {
            if chunks.is_empty() {
                continue;
            }
            tracing::info!(
                "running {} adapter on {} chunks",
                adapter.language(),
                chunks.len()
            );
            let res = adapter.analyze_chunks(&chunks);
            merged.analyzed_files += res.analyzed_files;
            merged.analyzed_chunks += res.analyzed_chunks;
            merged.errors.extend(res.errors);
            merged.graph.merge(res.graph);
        }
        // Cross-chunk linking: collapse duplicate symbols introduced by
        // overlapping chunk windows into single canonical nodes and rewrite
        // edges to use canonical ids.
        merged.graph = crate::core::linker::link(merged.graph);
        merged
    }
}

impl Default for AnalyzerRegistry {
    fn default() -> Self {
        Self::default_registry()
    }
}

/// Why: framework detection is the natural sibling to the language-adapter
/// pipeline — both run after indexing completes and both surface project-level
/// metadata that downstream tooling (the `--explain` LLM prompt, dashboards)
/// needs to tailor advice. Storing each detected framework as a FactStore
/// triple makes the data queryable alongside every other static fact.
/// What: runs [`detect_frameworks`] against `project_root` and upserts one
/// `(<index_id>, "uses_framework", <name>)` fact per detected framework.
/// Returns the detected framework names so callers can echo them into a
/// `ReviewReport`. Errors from `facts.upsert` are logged at warn level and
/// the remaining frameworks are still recorded — a single fact store hiccup
/// shouldn't lose all the others.
/// Test: `record_frameworks_writes_one_fact_per_framework` in `mod tests`.
pub fn record_frameworks(facts: &FactStore, index_id: &str, project_root: &Path) -> Vec<String> {
    let frameworks = detect_frameworks(project_root);
    for name in &frameworks {
        let fact = new_fact(
            index_id.to_string(),
            "uses_framework".to_string(),
            name.clone(),
            index_id.to_string(),
        );
        if let Err(e) = facts.upsert(fact) {
            tracing::warn!("record_frameworks: failed to upsert {index_id} -> {name}: {e:#}");
        }
    }
    frameworks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::KgNodeKind;

    fn chunk(file: &str, content: &str) -> CodeChunk {
        CodeChunk {
            id: format!("{file}:1:5"),
            file: file.into(),
            start_line: 1,
            end_line: 5,
            content: content.into(),
            function_name: None,
            score: 0.0,
            compact_snippet: None,
            match_reason: String::new(),
        }
    }

    #[test]
    fn default_registry_constructs() {
        let r = AnalyzerRegistry::default_registry();
        let langs: Vec<&str> = r.analyzers().iter().map(|a| a.language()).collect();
        for required in [
            "rust",
            "typescript",
            "javascript",
            "python",
            "java",
            "go",
            "c",
            "cpp",
            "csharp",
            "kotlin",
            "php",
            "ruby",
            "scala",
            "swift",
        ] {
            assert!(langs.contains(&required), "missing {required} in {langs:?}");
        }
        assert_eq!(r.analyzers().len(), 14, "expected all 14 adapters");
    }

    #[test]
    fn analyzer_for_dispatches_by_extension() {
        let r = AnalyzerRegistry::default_registry();
        assert_eq!(
            r.analyzer_for("foo.rs").map(|a| a.language().to_string()),
            Some("rust".into())
        );
        assert_eq!(
            r.analyzer_for("foo.ts").map(|a| a.language().to_string()),
            Some("typescript".into())
        );
        assert!(r.analyzer_for("README.md").is_none());
    }

    #[test]
    fn record_frameworks_writes_one_fact_per_framework() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a project with detectable frameworks.
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{ "dependencies": { "next": "14", "react": "18" } }"#,
        )
        .unwrap();
        let facts_db = tempfile::tempdir().unwrap();
        let facts = FactStore::open(&facts_db.path().join("facts.redb")).unwrap();
        let found = record_frameworks(&facts, "my-index", tmp.path());
        assert!(found.contains(&"Next.js".to_string()));
        assert!(found.contains(&"React".to_string()));
        let triples = facts
            .query(Some("my-index"), Some("uses_framework"), None)
            .unwrap();
        assert_eq!(triples.len(), 2, "got {triples:?}");
    }

    #[test]
    fn analyze_routes_chunks_per_language() {
        let r = AnalyzerRegistry::default_registry();
        let chunks = vec![
            chunk("a.rs", "fn rust_fn() {}\n"),
            chunk("b.ts", "function ts_fn() { return 1; }\n"),
        ];
        let res = r.analyze(&chunks);
        let names: Vec<&str> = res
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, KgNodeKind::Function))
            .map(|n| n.name.as_str())
            .collect();
        assert!(names.contains(&"rust_fn"));
        assert!(names.contains(&"ts_fn"));
    }
}
