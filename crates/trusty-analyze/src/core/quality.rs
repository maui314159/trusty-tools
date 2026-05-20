//! Aggregate quality metrics over a chunk corpus.
//!
//! Why: the `/indexes/:id/quality` endpoint and the `analyze_quality` MCP tool
//! both want a one-shot summary number — average cyclomatic complexity, %
//! grade-A chunks, total smell count. Pulling the math into a pure function
//! keeps the HTTP/MCP layers thin.

use crate::types::complexity::ComplexityGrade;
use crate::types::CodeChunk;
use serde::Serialize;

use crate::core::complexity::compute_complexity;

/// One-shot quality summary over a chunk corpus.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct QualityReport {
    pub avg_cyclomatic: f32,
    pub pct_grade_a: f32,
    pub smell_count: usize,
    pub chunk_count: usize,
}

/// Sort `chunks` by descending cyclomatic complexity and return the top N.
/// Chunks without a `complexity` field are scored on demand from `content`.
pub fn complexity_hotspots(chunks: &[CodeChunk], top_n: usize) -> Vec<CodeChunk> {
    let mut scored: Vec<(u32, CodeChunk)> = chunks
        .iter()
        .map(|c| (cyclomatic_of(c), c.clone()))
        .collect();
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    scored.truncate(top_n);
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Return chunks that have at least one detected smell. Re-runs `detect_smells`
/// for chunks lacking pre-computed `complexity` so the caller doesn't have to
/// require trusty-search to populate the field.
pub fn smelly_chunks(chunks: &[CodeChunk]) -> Vec<CodeChunk> {
    chunks
        .iter()
        .filter(|c| !smells_of(c).is_empty())
        .cloned()
        .collect()
}

/// Compute the aggregate `QualityReport`.
pub fn aggregate_quality(chunks: &[CodeChunk]) -> QualityReport {
    let chunk_count = chunks.len();
    let (sum_cyclo, grade_a, smell_count) =
        chunks.iter().fold((0u64, 0usize, 0usize), |(s, a, sm), c| {
            let metrics_cyclo = cyclomatic_of(c);
            let grade = grade_of(c);
            let smell_total = smells_of(c).len();
            let a_inc = if grade == ComplexityGrade::A { 1 } else { 0 };
            (s + metrics_cyclo as u64, a + a_inc, sm + smell_total)
        });
    let avg_cyclomatic = if chunk_count == 0 {
        0.0_f32
    } else {
        sum_cyclo as f32 / chunk_count as f32
    };
    let pct_grade_a = if chunk_count == 0 {
        0.0_f32
    } else {
        grade_a as f32 / chunk_count as f32
    };
    QualityReport {
        avg_cyclomatic,
        pct_grade_a,
        smell_count,
        chunk_count,
    }
}

fn cyclomatic_of(c: &CodeChunk) -> u32 {
    compute_complexity(&c.content).cyclomatic
}

fn grade_of(c: &CodeChunk) -> ComplexityGrade {
    compute_complexity(&c.content).grade
}

fn smells_of(c: &CodeChunk) -> Vec<crate::types::complexity::CodeSmell> {
    crate::core::complexity::detect_smells(&c.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, content: &str) -> CodeChunk {
        CodeChunk {
            id: id.into(),
            file: "f.rs".into(),
            start_line: 1,
            end_line: 10,
            content: content.into(),
            function_name: None,
            score: 0.0,
            compact_snippet: None,
            match_reason: "test".into(),
        }
    }

    #[test]
    fn empty_corpus_returns_zeros() {
        let r = aggregate_quality(&[]);
        assert_eq!(r.chunk_count, 0);
        assert_eq!(r.avg_cyclomatic, 0.0);
        assert_eq!(r.pct_grade_a, 0.0);
        assert_eq!(r.smell_count, 0);
    }

    #[test]
    fn hotspots_sort_by_cyclomatic_descending() {
        // build content with varying decision-point counts.
        let simple = chunk("s", "/// doc\nfn s() {}\n");
        let mut messy_src = String::from("/// doc\nfn m(a: u32) {\n");
        for _ in 0..10 {
            messy_src.push_str("    if a == 1 { return; }\n");
        }
        messy_src.push_str("}\n");
        let messy = chunk("m", &messy_src);
        let hot = complexity_hotspots(&[simple.clone(), messy.clone()], 2);
        assert_eq!(hot.len(), 2);
        assert_eq!(hot[0].id, "m"); // messy first
    }

    #[test]
    fn smelly_chunks_filters_clean_ones() {
        let clean = chunk("c", "/// doc\nfn f() {}\n");
        let mut long = String::from("/// doc\nfn big() {\n");
        for _ in 0..60 {
            long.push_str("    let _ = 1;\n");
        }
        long.push_str("}\n");
        let big = chunk("b", &long);
        let out = smelly_chunks(&[clean, big]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "b");
    }
}
