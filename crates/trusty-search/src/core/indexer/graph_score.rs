//! Graph-expanded retrieval scoring (issue #41 phase 4).
//!
//! Why: Vector similarity finds semantically similar code but misses
//! structurally important nodes (heavily called functions, subsystem entry
//! points). Graph centrality complements vector similarity for retrieval
//! quality. The blended score is `rrf_score + graph_bonus`, where
//! `graph_bonus ∈ [0.0, 0.15]` derives from normalised degree centrality plus
//! a community-centroid status flag.
//!
//! What: Precomputes a per-symbol bonus table from a `SymbolGraph` snapshot
//! and a list of `CommunityRecord`s, then offers cheap point-lookups during
//! search-result ranking. Same-community detection lets callers compute a
//! "community cohesion" metric over the top-k results.
//!
//! Test: see the `tests` module — covers degree-based ordering, centroid
//! boost, and same-community detection.

use std::collections::{HashMap, HashSet};

use crate::core::community::CommunityRecord;
use crate::core::symbol_graph::SymbolGraph;

/// Maximum total graph bonus added on top of the RRF score.
///
/// Why: The bonus must be small enough that a clearly stronger semantic match
/// still outranks a structurally central but semantically weaker chunk. RRF
/// scores at the top of the list are typically in the `[0.02, 0.05]` range
/// (sum of `1 / (k + rank)` terms with `k=60`), so a 0.15 cap leaves room for
/// centrality to break ties without dominating retrieval.
/// What: `0.10 * centrality + 0.05 * is_centroid`, capped at `BONUS_MAX`.
/// Test: `bonus_within_bounds` asserts the cap holds.
pub const BONUS_MAX: f32 = 0.15;

/// Weight of the normalised degree-centrality component of the bonus.
const CENTRALITY_WEIGHT: f32 = 0.10;

/// Weight of the community-centroid component of the bonus.
const CENTROID_WEIGHT: f32 = 0.05;

/// Per-index precomputed graph scoring table.
///
/// Why: Computing degree centrality and centroid status from scratch on every
/// search request would dwarf the search cost itself (an O(N) graph walk for
/// each query). Building the table once after each reindex and caching it
/// behind an `Arc` lets every search request take O(1) point-lookups per
/// result chunk.
/// What: Holds `symbol → normalised_centrality` (`degree / max_degree`),
/// a `HashSet` of centroid symbols (one per community), and a
/// `symbol → community_id` map for same-community queries. All fields are
/// owned (no references) so the struct can be parked in an `Arc` and shared
/// across concurrent search handlers.
/// Test: `tests::high_degree_node_scores_higher` and
/// `tests::centroid_gets_extra_bonus` cover the build+bonus path;
/// `tests::same_community_detection` covers the cohesion lookup.
pub struct GraphScorer {
    /// Normalised degree centrality: `degree / max_degree`. Always in `[0, 1]`.
    centrality: HashMap<String, f32>,
    /// Centroid symbols (the highest-degree member of each community).
    centroids: HashSet<String>,
    /// Symbol → community id (mirrors what `CorpusStore::symbol_community`
    /// would return but stays in memory so the cohesion check is lock-free).
    community_map: HashMap<String, u64>,
}

impl GraphScorer {
    /// Build a scorer from a `SymbolGraph` snapshot and its Louvain community
    /// records.
    ///
    /// Why: The graph and community data are produced by separate pipeline
    /// stages (`build_from_chunks_with_entities` and Louvain detection) and
    /// the scorer ties them together for the search hot path.
    /// What: Computes degree centrality via [`SymbolGraph::degrees`],
    /// normalises by the max degree (defending against a degree-zero graph),
    /// then walks each `CommunityRecord` to pick its centroid (declared by
    /// the community detector) and stamp the symbol → community map.
    /// Test: covered by the tests below.
    pub fn build(graph: &SymbolGraph, communities: &[CommunityRecord]) -> Self {
        let degrees = graph.degrees();
        let max_degree = degrees.values().copied().max().unwrap_or(0) as f32;
        let centrality: HashMap<String, f32> = if max_degree <= 0.0 {
            degrees.keys().map(|s| (s.clone(), 0.0_f32)).collect()
        } else {
            degrees
                .into_iter()
                .map(|(sym, d)| (sym, d as f32 / max_degree))
                .collect()
        };

        let mut centroids: HashSet<String> = HashSet::with_capacity(communities.len());
        let mut community_map: HashMap<String, u64> = HashMap::new();
        for rec in communities {
            if !rec.centroid_symbol.is_empty() {
                centroids.insert(rec.centroid_symbol.clone());
            }
            let cid = rec.id as u64;
            for member in &rec.members {
                community_map.insert(member.clone(), cid);
            }
        }

        Self {
            centrality,
            centroids,
            community_map,
        }
    }

    /// Returns a bonus in `[0.0, BONUS_MAX]` for `symbol`.
    ///
    /// Why: Blended into the post-MMR ranking so chunks tied to structurally
    /// important symbols outrank otherwise-equivalent peripheral chunks.
    /// What: `bonus = 0.10 * centrality + 0.05 * is_centroid`, clamped to
    /// `[0.0, BONUS_MAX]`. Unknown symbols return `0.0`.
    /// Test: `tests::bonus_within_bounds` and `tests::centroid_gets_extra_bonus`.
    pub fn bonus(&self, symbol: &str) -> f32 {
        let c = self.centrality.get(symbol).copied().unwrap_or(0.0);
        let centroid = if self.centroids.contains(symbol) {
            1.0_f32
        } else {
            0.0_f32
        };
        let raw = CENTRALITY_WEIGHT * c + CENTROID_WEIGHT * centroid;
        raw.clamp(0.0, BONUS_MAX)
    }

    /// Returns `true` when `a` and `b` share a community.
    ///
    /// Why: Computing a "community cohesion" metric (the fraction of top-k
    /// results that share the community of the top result) gives clients a
    /// quick signal of whether the answer set is structurally consistent.
    /// What: Both symbols must be known to the scorer; an unknown symbol
    /// returns `false`. A self-comparison returns `true` when the symbol is
    /// known.
    /// Test: `tests::same_community_detection`.
    pub fn same_community(&self, a: &str, b: &str) -> bool {
        match (self.community_map.get(a), self.community_map.get(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }

    /// Look up the community id for a symbol, if any.
    ///
    /// Why: The search handler uses this to compute the cohesion metric
    /// without exposing the internal `HashMap`.
    /// What: Returns `Some(community_id)` for known members, `None` otherwise.
    /// Test: `tests::same_community_detection`.
    pub fn community_of(&self, symbol: &str) -> Option<u64> {
        self.community_map.get(symbol).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chunker::ChunkType;
    use crate::core::symbol_graph::{ChunkTuple, SymbolGraph};

    fn mk_chunk(
        id: &str,
        file: &str,
        name: &str,
        calls: &[&str],
        chunk_type: ChunkType,
    ) -> ChunkTuple {
        (
            id.to_string(),
            file.to_string(),
            Some(name.to_string()),
            calls.iter().map(|s| s.to_string()).collect(),
            Vec::new(),
            chunk_type,
        )
    }

    fn synthetic_record(
        id: usize,
        centroid: &str,
        members: &[&str],
    ) -> CommunityRecord {
        CommunityRecord {
            id,
            members: members.iter().map(|s| s.to_string()).collect(),
            member_count: members.len(),
            modularity_contribution: 0.0,
            centroid_symbol: centroid.to_string(),
            dominant_files: Vec::new(),
        }
    }

    /// Why: Heavily-connected symbols should receive a strictly larger bonus
    /// than isolated ones; this is the load-bearing axiom of the scorer.
    /// What: Builds a star-shaped call graph where `hub` is called by five
    /// callers, while `leaf` has only one call edge, then asserts ordering.
    /// Test: failure here indicates the degree projection or normalisation
    /// regressed.
    #[test]
    fn high_degree_node_scores_higher() {
        let chunks = vec![
            mk_chunk("a.rs:1:5", "a.rs", "hub", &[], ChunkType::Function),
            mk_chunk(
                "a.rs:7:9",
                "a.rs",
                "caller1",
                &["hub"],
                ChunkType::Function,
            ),
            mk_chunk(
                "a.rs:11:13",
                "a.rs",
                "caller2",
                &["hub"],
                ChunkType::Function,
            ),
            mk_chunk(
                "a.rs:15:17",
                "a.rs",
                "caller3",
                &["hub"],
                ChunkType::Function,
            ),
            mk_chunk(
                "a.rs:19:21",
                "a.rs",
                "caller4",
                &["hub"],
                ChunkType::Function,
            ),
            mk_chunk(
                "a.rs:23:25",
                "a.rs",
                "caller5",
                &["hub"],
                ChunkType::Function,
            ),
            mk_chunk("a.rs:27:29", "a.rs", "leaf", &[], ChunkType::Function),
            mk_chunk(
                "a.rs:31:33",
                "a.rs",
                "isolated",
                &["leaf"],
                ChunkType::Function,
            ),
        ];
        let graph = SymbolGraph::build_from_chunks(&chunks);
        let scorer = GraphScorer::build(&graph, &[]);
        let hub_bonus = scorer.bonus("hub");
        let leaf_bonus = scorer.bonus("leaf");
        assert!(
            hub_bonus > leaf_bonus,
            "hub bonus {hub_bonus} should exceed leaf bonus {leaf_bonus}"
        );
        assert!(hub_bonus <= BONUS_MAX);
    }

    /// Why: Community centroids get an extra flat bonus so the natural anchor
    /// of a cluster outranks peripheral cluster members with similar centrality.
    /// What: Two synthetic graphs (otherwise identical degrees) where one
    /// symbol is declared centroid; the centroid bonus must be strictly larger.
    /// Test: regression guard against centroid weighting being silently dropped.
    #[test]
    fn centroid_gets_extra_bonus() {
        // Symmetric graph: a↔b, two edges in total. Both symbols have
        // identical degree (1 in + 1 out across the call edges depending on
        // direction), so the only differentiator is centroid membership.
        let chunks = vec![
            mk_chunk("a.rs:1:5", "a.rs", "a", &["b"], ChunkType::Function),
            mk_chunk("a.rs:7:9", "a.rs", "b", &["a"], ChunkType::Function),
        ];
        let graph = SymbolGraph::build_from_chunks(&chunks);
        let communities = vec![synthetic_record(0, "a", &["a", "b"])];
        let scorer = GraphScorer::build(&graph, &communities);
        let a_bonus = scorer.bonus("a");
        let b_bonus = scorer.bonus("b");
        assert!(
            a_bonus > b_bonus,
            "centroid bonus {a_bonus} should exceed non-centroid {b_bonus}"
        );
    }

    /// Why: The "community cohesion" metric needs a cheap same-community
    /// predicate; the test guards against the community map being incorrectly
    /// keyed (e.g. by centroid instead of every member).
    /// What: Two members of the same community return `true`; cross-community
    /// pairs and unknown symbols return `false`.
    #[test]
    fn same_community_detection() {
        // Graph + arbitrary communities — only the records matter for this
        // test, so the graph itself can be empty.
        let graph = SymbolGraph::new();
        let communities = vec![
            synthetic_record(0, "alpha", &["alpha", "beta", "gamma"]),
            synthetic_record(1, "delta", &["delta", "epsilon"]),
        ];
        let scorer = GraphScorer::build(&graph, &communities);
        assert!(scorer.same_community("alpha", "beta"));
        assert!(scorer.same_community("beta", "gamma"));
        assert!(!scorer.same_community("alpha", "delta"));
        assert!(!scorer.same_community("alpha", "unknown"));
    }

    /// Why: A regression here means the bonus can swamp the RRF score and the
    /// blend stops being a tie-breaker. Pin the cap explicitly.
    #[test]
    fn bonus_within_bounds() {
        // Maximum case: degree-1 (centrality = 1.0) AND centroid.
        let chunks = vec![
            mk_chunk("a.rs:1:5", "a.rs", "hub", &["leaf"], ChunkType::Function),
            mk_chunk("a.rs:7:9", "a.rs", "leaf", &[], ChunkType::Function),
        ];
        let graph = SymbolGraph::build_from_chunks(&chunks);
        let communities = vec![synthetic_record(0, "hub", &["hub", "leaf"])];
        let scorer = GraphScorer::build(&graph, &communities);
        let b = scorer.bonus("hub");
        assert!(b <= BONUS_MAX, "bonus {b} exceeded cap {BONUS_MAX}");
        assert!(b >= 0.0);
    }
}
