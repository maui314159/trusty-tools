//! JavaScript adapter — reuses the TypeScript walker with the JS grammar.
//!
//! Why: JavaScript and TypeScript share enough AST structure that the same
//! walker is sufficient; only the grammar differs.
//!
//! What: A thin wrapper that delegates to `typescript::analyze_with_grammar`
//! with the JavaScript grammar selected.
//!
//! Test: `js_analyzer_extracts_function` asserts a `function hello() {}`
//! snippet produces a Function node tagged `language = "javascript"`.

use crate::types::CodeChunk;

use super::typescript::analyze_with_grammar;
use crate::lang::{LanguageAnalyzer, StaticAnalysisResult};

/// tree-sitter-javascript-backed analyzer.
pub struct JavaScriptAnalyzer;

impl JavaScriptAnalyzer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JavaScriptAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageAnalyzer for JavaScriptAnalyzer {
    fn language(&self) -> &str {
        "javascript"
    }

    fn supported_extensions(&self) -> &[&str] {
        &[".js", ".jsx", ".mjs", ".cjs"]
    }

    fn analyze_chunks(&self, chunks: &[CodeChunk]) -> StaticAnalysisResult {
        analyze_with_grammar(chunks, "javascript", false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{KgNode, KgNodeKind};

    fn make_chunk(content: &str) -> CodeChunk {
        CodeChunk {
            id: "f.js:1:5".into(),
            file: "f.js".into(),
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
    fn js_analyzer_extracts_function() {
        let a = JavaScriptAnalyzer::new();
        let c = make_chunk("function hello() { return 1; }\n");
        let r = a.analyze_chunks(&[c]);
        let funcs: Vec<&KgNode> = r
            .graph
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, KgNodeKind::Function))
            .collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "hello");
        assert_eq!(funcs[0].language, "javascript");
    }

    #[test]
    fn supports_js_extensions() {
        let a = JavaScriptAnalyzer::new();
        assert!(a.supports("foo.js"));
        assert!(a.supports("foo.mjs"));
        assert!(!a.supports("foo.ts"));
    }
}
