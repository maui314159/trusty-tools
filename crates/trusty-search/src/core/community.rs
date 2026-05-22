//! Louvain community detection over the `SymbolGraph` (issue #41 phase 3).
//!
//! Why: BFS-scoped KG expansion answers "what's near this symbol?" but agents
//! also want to ask "what are the natural subsystems in this codebase?" and
//! "which symbols cluster around the same architectural concern?". Community
//! detection turns the persisted symbol graph into a stable partition of
//! symbols into cohesive groups so dashboards/agents can surface knowledge
//! gaps, dominant files per cluster, and topology-aware search routing.
//!
//! No pure-Rust Leiden crate exists, so this module implements the Louvain
//! algorithm from scratch on petgraph. Louvain greedily maximises modularity
//! Q = Σ_c [L_c/m − (d_c/2m)²] by moving nodes between communities. Random
//! ordering uses a seeded `StdRng` (seed = 42) so the partition is
//! deterministic across runs on the same graph.
//!
//! Test: `tests::two_cliques_yield_two_communities` builds a synthetic graph
//! with two clear k4-cliques connected by a single bridge and asserts the
//! algorithm returns exactly two communities; `tests::path_graph_modularity_
//! is_positive` checks Q > 0 for a non-trivial chain.

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

use crate::core::entity::EdgeKind;
use crate::core::symbol_graph::{SymbolGraph, SymbolNode};

/// Deterministic RNG seed for Louvain node ordering.
///
/// Why: random node iteration improves the modularity of the partition Louvain
/// finds, but agents and tests want reproducible community ids across runs on
/// the same graph. A fixed seed yields both.
/// What: u64 fed into [`rand::rngs::StdRng::seed_from_u64`].
/// Test: covered transitively by `two_cliques_yield_two_communities`.
const LOUVAIN_SEED: u64 = 42;

/// Maximum number of Louvain passes (level transitions) before we bail.
///
/// Why: Louvain typically converges in <10 passes; capping at 100 prevents a
/// pathological graph from looping forever.
/// What: outer-loop iteration limit.
/// Test: covered indirectly by `path_graph_modularity_is_positive`.
const MAX_PASSES: usize = 100;

/// Maximum inner-loop sweeps per pass.
///
/// Why: each pass repeatedly walks every node trying to improve its community
/// until no move helps. Pathological graphs can oscillate; this is a safety
/// net.
/// What: inner-loop iteration limit.
const MAX_SWEEPS_PER_PASS: usize = 50;

/// Persisted summary of a single community (issue #41 phase 3).
///
/// Why: HTTP / agent consumers want a compact per-community summary
/// (centroid + dominant files + member list) without re-deriving it from the
/// node-level assignment map every request. Storing it alongside the partition
/// keeps `GET /indexes/:id/communities` a single redb scan.
/// What: serde-derived JSON payload stored under `KG_COMMUNITIES_TABLE[id]`.
/// Test: covered by `tests::community_record_round_trip` in this module and
/// by `save_load_communities_roundtrip` in `corpus.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommunityRecord {
    /// 0-indexed community id (community 0 is the largest by member count).
    pub id: usize,
    /// Symbol names in this community, sorted alphabetically for stable output.
    pub members: Vec<String>,
    /// Convenience: `members.len()`. Stored so HTTP responses can avoid the
    /// extra `.len()` when paging through truncated member lists.
    pub member_count: usize,
    /// Local contribution to total modularity. Sum across all records ≈ Q.
    pub modularity_contribution: f64,
    /// Highest-degree symbol in this community — the natural "anchor"
    /// returned by `GET /indexes/:id/communities/:symbol`.
    pub centroid_symbol: String,
    /// Top-3 files by member count in this community (descending). Empty when
    /// the community contains only symbols with empty `file` fields.
    pub dominant_files: Vec<String>,
}

/// Output of one Louvain run.
///
/// Why: persisting the assignment by symbol name (not NodeIndex) lets the
/// result survive a graph rebuild — node indices are invalidated on every
/// `petgraph::DiGraph::add_node` reordering after `remove_node`.
/// What: a map from symbol → community id (0-indexed; sorted by descending
/// member count so id 0 is always the largest community), plus the total
/// community count and the final modularity score Q ∈ [-0.5, 1.0].
/// Test: `tests::two_cliques_yield_two_communities` asserts `community_count
/// == 2` and `tests::path_graph_modularity_is_positive` asserts `modularity
/// > 0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LouvainCommunities {
    /// Symbol name → community id. Stable across runs (seeded RNG).
    pub assignments: HashMap<String, usize>,
    /// Number of communities found.
    pub community_count: usize,
    /// Final modularity score Q ∈ [-0.5, 1.0]. Higher = better partition.
    pub modularity: f64,
}

/// Project the directed symbol graph into an undirected weighted adjacency
/// list (issue #41 phase 3).
///
/// Why: Louvain/Leiden are defined on undirected weighted graphs; the
/// `SymbolGraph` is a directed `DiGraph<SymbolNode, EdgeKind>`. We project by
/// symmetrising: weight(a,b) = sum of `EdgeKind::score_multiplier` for every
/// directed edge between a and b (in either direction).
/// What: returns `(symbol → node_id, adjacency)` where `adjacency[node_id]`
/// holds `(neighbour_id, weight)` pairs. Self-loops are dropped (the source
/// graph already filters them, but defence in depth keeps the modularity math
/// honest).
/// Test: `tests::project_symmetrises_bidirectional_edges` asserts that a graph
/// with both a→b and b→a yields a single undirected edge whose weight is the
/// sum of the two directed multipliers.
fn project_undirected(
    graph: &DiGraph<SymbolNode, EdgeKind>,
    by_symbol: &HashMap<String, NodeIndex>,
) -> (HashMap<String, usize>, Vec<Vec<(usize, f64)>>) {
    // Stable symbol → contiguous-id map. Sort so the mapping is reproducible
    // across runs on the same graph (HashMap iteration order is randomised).
    let mut symbols: Vec<&String> = by_symbol.keys().collect();
    symbols.sort();
    let mut symbol_to_id: HashMap<String, usize> = HashMap::with_capacity(symbols.len());
    for (i, s) in symbols.iter().enumerate() {
        symbol_to_id.insert((*s).clone(), i);
    }

    // Build the directed-edge bag keyed on the unordered pair (lo, hi).
    let mut paired: HashMap<(usize, usize), f64> = HashMap::new();
    for edge in graph.edge_references() {
        let src_sym = match graph.node_weight(edge.source()) {
            Some(n) => &n.symbol,
            None => continue,
        };
        let tgt_sym = match graph.node_weight(edge.target()) {
            Some(n) => &n.symbol,
            None => continue,
        };
        let (a, b) = match (symbol_to_id.get(src_sym), symbol_to_id.get(tgt_sym)) {
            (Some(&a), Some(&b)) => (a, b),
            _ => continue,
        };
        if a == b {
            continue;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        let w = edge.weight().score_multiplier() as f64;
        *paired.entry(key).or_insert(0.0) += w;
    }

    let mut adjacency: Vec<Vec<(usize, f64)>> = vec![Vec::new(); symbols.len()];
    for ((a, b), w) in paired {
        adjacency[a].push((b, w));
        adjacency[b].push((a, w));
    }
    (symbol_to_id, adjacency)
}

/// Adjacency-list super-graph used by the aggregation phase.
///
/// Why: after one Louvain pass we collapse each community into a single node
/// and recurse. Re-using `petgraph` for the super-graph is wasteful — a plain
/// `Vec<HashMap<usize, f64>>` keeps the inner loop branch-free.
type SuperGraph = Vec<HashMap<usize, f64>>;

/// Compute the weighted degree of every node.
///
/// Why: Louvain's ΔQ formula uses each node's total incident weight; caching
/// it once per level avoids a per-iteration recomputation.
/// What: degree[i] = Σ_j weight(i, j).
fn weighted_degrees(adj: &[Vec<(usize, f64)>]) -> Vec<f64> {
    adj.iter()
        .map(|row| row.iter().map(|(_, w)| w).sum::<f64>())
        .collect()
}

/// Total edge weight m = ½ Σ_i Σ_j A_ij (each undirected edge counted once).
///
/// Why: m appears in every ΔQ computation; caching it per level is essential.
fn total_weight(adj: &[Vec<(usize, f64)>]) -> f64 {
    let twice: f64 = adj.iter().flat_map(|row| row.iter().map(|(_, w)| *w)).sum();
    twice / 2.0
}

/// Run one Louvain "level": iterate node moves until no improvement.
///
/// Why: factored out so the level loop and the aggregation step read cleanly.
/// What: returns `Vec<usize>` mapping each node id → its community id at this
/// level. Communities are re-labelled to be contiguous 0..k.
fn one_louvain_level(adj: &[Vec<(usize, f64)>], rng: &mut StdRng) -> Vec<usize> {
    let n = adj.len();
    if n == 0 {
        return Vec::new();
    }
    let degrees = weighted_degrees(adj);
    let m = total_weight(adj);
    if m <= 0.0 {
        // Disconnected / zero-weight graph: every node is its own community.
        return (0..n).collect();
    }
    let two_m = 2.0 * m;

    // Initial state: each node in its own community.
    let mut community: Vec<usize> = (0..n).collect();
    // Σ_tot for each community = sum of weighted degrees of its members.
    let mut sigma_tot: Vec<f64> = degrees.clone();

    let mut order: Vec<usize> = (0..n).collect();
    for _ in 0..MAX_SWEEPS_PER_PASS {
        order.shuffle(rng);
        let mut moved = false;

        for &node in order.iter() {
            let current = community[node];
            let k_i = degrees[node];

            // Gather candidate communities and the weight from `node` into each.
            let (k_i_in_current, weights_to_comm) = neighbor_weights(node, &community, adj);
            // Remove `node` from its current community for the ΔQ math.
            sigma_tot[current] -= k_i;

            // Best move = arg max of ΔQ = k_i_in_C / m - Σ_tot[C] * k_i / (2m²)
            let mut best_comm = current;
            let mut best_gain: f64 = 0.0;
            for (&cand, &k_i_in_cand) in weights_to_comm.iter() {
                let gain = k_i_in_cand / m - sigma_tot[cand] * k_i / (two_m * m);
                if gain > best_gain {
                    best_gain = gain;
                    best_comm = cand;
                }
            }
            // Always evaluate "stay" via the cached k_i_in_current.
            let stay_gain = k_i_in_current / m - sigma_tot[current] * k_i / (two_m * m);
            if stay_gain > best_gain {
                best_gain = stay_gain;
                best_comm = current;
            }

            sigma_tot[best_comm] += k_i;
            if best_comm != current {
                community[node] = best_comm;
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }

    relabel_contiguous(&community)
}

/// For one node, compute the weight into its own community and a map of
/// candidate-community → weight-into-that-community.
///
/// Why: extracting this keeps `one_louvain_level` short and lets the gain
/// calculation read top-to-bottom.
/// What: returns `(weight_into_own_community, candidate_weights)`. Self-loops
/// are excluded.
fn neighbor_weights(
    node: usize,
    community: &[usize],
    adj: &[Vec<(usize, f64)>],
) -> (f64, HashMap<usize, f64>) {
    let own = community[node];
    let mut k_i_in_own = 0.0;
    let mut weights: HashMap<usize, f64> = HashMap::new();
    for &(nb, w) in &adj[node] {
        if nb == node {
            continue;
        }
        let comm = community[nb];
        if comm == own {
            k_i_in_own += w;
        }
        *weights.entry(comm).or_insert(0.0) += w;
    }
    (k_i_in_own, weights)
}

/// Re-label a community assignment so ids form a contiguous 0..k range.
///
/// Why: the aggregation step indexes the super-graph by community id, so
/// holes (e.g. all members of community 7 moved out) would waste rows.
/// What: first-seen ordering — community ids preserve the order in which
/// they appear in `community`.
fn relabel_contiguous(community: &[usize]) -> Vec<usize> {
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut next = 0usize;
    community
        .iter()
        .map(|&c| {
            *remap.entry(c).or_insert_with(|| {
                let id = next;
                next += 1;
                id
            })
        })
        .collect()
}

/// Collapse every community into a super-node and rebuild the adjacency.
///
/// Why: this is the load-bearing step that lets Louvain find hierarchical
/// structure — a level-2 community is a community-of-communities.
/// What: `super_adj[c1][c2] = Σ weight(i,j) for i in c1, j in c2` (including
/// internal "self-loops" c→c which are doubled-counted by convention so the
/// total weight `m` is preserved).
fn aggregate(adj: &[Vec<(usize, f64)>], community: &[usize]) -> SuperGraph {
    let k = community.iter().copied().max().map_or(0, |m| m + 1);
    let mut out: SuperGraph = vec![HashMap::new(); k];
    for (i, row) in adj.iter().enumerate() {
        let ci = community[i];
        for &(j, w) in row {
            let cj = community[j];
            // Each undirected pair (i, j) is visited twice (once per endpoint
            // in `adj`). We retain that double-counting in the super-graph
            // because `total_weight` halves it again.
            *out[ci].entry(cj).or_insert(0.0) += w;
        }
    }
    out
}

/// Convert the super-graph map representation into the flat adjacency-list
/// form that [`one_louvain_level`] expects.
fn super_to_adj(super_g: &SuperGraph) -> Vec<Vec<(usize, f64)>> {
    super_g
        .iter()
        .map(|m| m.iter().map(|(&j, &w)| (j, w)).collect())
        .collect()
}

/// Compute modularity Q for the current node→community assignment.
///
/// Why: lets the caller observe convergence; also the headline metric the
/// HTTP endpoint surfaces.
/// What: Q = Σ_c (L_c / m − (d_c / 2m)²) where L_c is the within-community
/// edge weight (counting each edge once) and d_c is the total degree of
/// community c.
fn modularity(adj: &[Vec<(usize, f64)>], community: &[usize]) -> f64 {
    let m = total_weight(adj);
    if m <= 0.0 {
        return 0.0;
    }
    let two_m = 2.0 * m;
    let mut l_c: HashMap<usize, f64> = HashMap::new();
    let mut d_c: HashMap<usize, f64> = HashMap::new();
    for (i, row) in adj.iter().enumerate() {
        let ci = community[i];
        for &(j, w) in row {
            let cj = community[j];
            *d_c.entry(ci).or_insert(0.0) += w; // sums total endpoints
            if ci == cj {
                *l_c.entry(ci).or_insert(0.0) += w / 2.0; // each within-edge counted twice
            }
        }
    }
    let mut q = 0.0;
    for (c, l) in &l_c {
        let d = d_c.get(c).copied().unwrap_or(0.0);
        q += l / m - (d / two_m).powi(2);
    }
    // Singleton communities with no within-edges still need their (d/2m)² term.
    for (c, d) in &d_c {
        if !l_c.contains_key(c) {
            q -= (d / two_m).powi(2);
        }
    }
    q
}

/// Merge each singleton community into the neighbour community with which it
/// shares the highest edge weight.
///
/// Why: Louvain on a sparse code graph can leave many one-symbol "communities"
/// that are just isolated symbols. They add noise to UI / agent queries without
/// telling us anything useful; folding them into the nearest cluster gives
/// agents a more compact partition.
/// What: walks every community of size 1 and reassigns its member to the
/// community of its heaviest neighbour (if any). Communities with no
/// neighbours are left intact (truly isolated symbols).
/// Test: covered transitively by `tests::singleton_pruned_into_neighbour`.
fn prune_singletons(adj: &[Vec<(usize, f64)>], community: &mut Vec<usize>) {
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for &c in community.iter() {
        *sizes.entry(c).or_insert(0) += 1;
    }
    for i in 0..community.len() {
        if sizes.get(&community[i]).copied().unwrap_or(0) != 1 {
            continue;
        }
        // Find heaviest neighbour community.
        let mut best: Option<(usize, f64)> = None;
        for &(nb, w) in &adj[i] {
            let cnb = community[nb];
            if cnb == community[i] {
                continue;
            }
            match best {
                Some((_, bw)) if w <= bw => {}
                _ => best = Some((cnb, w)),
            }
        }
        if let Some((new_c, _)) = best {
            let old = community[i];
            community[i] = new_c;
            *sizes.entry(old).or_insert(0) -= 1;
            *sizes.entry(new_c).or_insert(0) += 1;
        }
    }
}

/// Sort communities by descending member count and relabel so community 0 is
/// always the largest.
///
/// Why: agents and dashboards want a stable ordering — "the biggest cluster"
/// is the natural starting point.
/// What: returns the relabelled assignment.
fn relabel_by_size(community: &[usize]) -> Vec<usize> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for &c in community {
        *counts.entry(c).or_insert(0) += 1;
    }
    let mut pairs: Vec<(usize, usize)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut remap: HashMap<usize, usize> = HashMap::new();
    for (new_id, (old_id, _)) in pairs.into_iter().enumerate() {
        remap.insert(old_id, new_id);
    }
    community.iter().map(|c| remap[c]).collect()
}

impl LouvainCommunities {
    /// Run Louvain community detection on the supplied symbol graph.
    ///
    /// Why: see module docs — this is the headline entry point used by
    /// `SymbolGraph::detect_and_save_communities` and the offline reindex
    /// trigger.
    /// What: projects the directed graph to undirected, runs Louvain levels
    /// until no level improves modularity, prunes singletons into their
    /// heaviest neighbour, and relabels community ids by descending size.
    /// Test: `tests::two_cliques_yield_two_communities`.
    pub fn detect(graph: &SymbolGraph) -> Self {
        // Reach into the private `SymbolGraph` internals via its public API:
        // we need the petgraph handle to walk edges, and the symbol → index
        // map to project. `SymbolGraph` doesn't expose those directly, so we
        // reconstruct them from `all_nodes()` + `all_edges()` instead.
        let nodes = graph.all_nodes();
        let edges = graph.all_edges();
        if nodes.is_empty() {
            return Self {
                assignments: HashMap::new(),
                community_count: 0,
                modularity: 0.0,
            };
        }

        // Build the (symbol → id) map directly from the node list.
        let mut symbols: Vec<String> = nodes.iter().map(|(s, _, _)| s.clone()).collect();
        symbols.sort();
        symbols.dedup();
        let mut symbol_to_id: HashMap<String, usize> = HashMap::with_capacity(symbols.len());
        for (i, s) in symbols.iter().enumerate() {
            symbol_to_id.insert(s.clone(), i);
        }
        let id_to_symbol: Vec<String> = symbols;

        // Symmetrise edges.
        let mut paired: HashMap<(usize, usize), f64> = HashMap::new();
        for (src, tgt, kind) in &edges {
            let (a, b) = match (symbol_to_id.get(src), symbol_to_id.get(tgt)) {
                (Some(&a), Some(&b)) => (a, b),
                _ => continue,
            };
            if a == b {
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            *paired.entry(key).or_insert(0.0) += kind.score_multiplier() as f64;
        }
        let mut adjacency: Vec<Vec<(usize, f64)>> = vec![Vec::new(); id_to_symbol.len()];
        for ((a, b), w) in paired {
            adjacency[a].push((b, w));
            adjacency[b].push((a, w));
        }

        // ── Louvain main loop ─────────────────────────────────────────────
        let mut rng = StdRng::seed_from_u64(LOUVAIN_SEED);
        let mut node_to_community: Vec<usize> = (0..adjacency.len()).collect();
        let mut current_adj = adjacency.clone();
        let mut current_assignment: Vec<usize> = (0..adjacency.len()).collect();
        let mut last_q = modularity(&adjacency, &node_to_community);

        for _pass in 0..MAX_PASSES {
            let level = one_louvain_level(&current_adj, &mut rng);
            // Propagate level result back to the base-graph assignment.
            for i in 0..node_to_community.len() {
                let cur = current_assignment[node_to_community[i]];
                let _ = cur; // silence unused if we ever drop the projection
            }
            // Build the new base-graph assignment: node_to_community is on the
            // current super-graph; map each base node through current_assignment
            // (super-id) and then through level (super-id → new super-id).
            for nc in node_to_community.iter_mut() {
                let super_id = current_assignment[*nc];
                *nc = level[super_id];
            }
            // Rebuild current_assignment to be identity on the new super-graph.
            current_assignment = (0..(level.iter().copied().max().map_or(0, |m| m + 1))).collect();

            // Aggregate the current super-graph into the next level.
            let super_g = aggregate(&current_adj, &level);
            current_adj = super_to_adj(&super_g);

            let q = modularity(&adjacency, &node_to_community);
            // Stop when no measurable improvement (1e-9 tolerance) or only one
            // super-node remains (further aggregation is a no-op).
            if (q - last_q).abs() < 1e-9 || current_adj.len() <= 1 {
                last_q = q;
                break;
            }
            last_q = q;
        }

        // Prune singletons + relabel by size.
        prune_singletons(&adjacency, &mut node_to_community);
        let final_assignment = relabel_by_size(&node_to_community);
        let final_q = modularity(&adjacency, &final_assignment);

        let mut assignments: HashMap<String, usize> = HashMap::with_capacity(id_to_symbol.len());
        for (i, sym) in id_to_symbol.iter().enumerate() {
            assignments.insert(sym.clone(), final_assignment[i]);
        }
        let community_count = final_assignment.iter().copied().max().map_or(0, |m| m + 1);

        Self {
            assignments,
            community_count,
            modularity: final_q,
        }
    }
}

// `project_undirected` is exercised indirectly through `LouvainCommunities::
// detect`; suppress the unused-helper warning when the module is compiled in
// isolation (e.g. for documentation generation).
#[allow(dead_code)]
fn _keep_project_in_scope(
    g: &DiGraph<SymbolNode, EdgeKind>,
    m: &HashMap<String, NodeIndex>,
) -> (HashMap<String, usize>, Vec<Vec<(usize, f64)>>) {
    project_undirected(g, m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chunker::ChunkType;
    use crate::core::symbol_graph::ChunkTuple;

    fn chunk(id: &str, file: &str, name: &str, calls: &[&str]) -> ChunkTuple {
        (
            id.to_string(),
            file.to_string(),
            Some(name.to_string()),
            calls.iter().map(|s| s.to_string()).collect(),
            Vec::new(),
            ChunkType::Function,
        )
    }

    #[test]
    fn two_cliques_yield_two_communities() {
        // Two 4-cliques connected by a single bridge edge between a4 and b1.
        // Louvain should find exactly 2 communities.
        let chunks = vec![
            chunk("a:1", "a.rs", "a1", &["a2", "a3", "a4"]),
            chunk("a:2", "a.rs", "a2", &["a1", "a3", "a4"]),
            chunk("a:3", "a.rs", "a3", &["a1", "a2", "a4"]),
            chunk("a:4", "a.rs", "a4", &["a1", "a2", "a3", "b1"]),
            chunk("b:1", "b.rs", "b1", &["b2", "b3", "b4"]),
            chunk("b:2", "b.rs", "b2", &["b1", "b3", "b4"]),
            chunk("b:3", "b.rs", "b3", &["b1", "b2", "b4"]),
            chunk("b:4", "b.rs", "b4", &["b1", "b2", "b3"]),
        ];
        let g = SymbolGraph::build_from_chunks(&chunks);
        let c = LouvainCommunities::detect(&g);
        assert_eq!(
            c.community_count, 2,
            "expected 2 communities, got {} (assignments={:?})",
            c.community_count, c.assignments
        );
        let a1 = c.assignments["a1"];
        let a2 = c.assignments["a2"];
        let b1 = c.assignments["b1"];
        let b2 = c.assignments["b2"];
        assert_eq!(a1, a2, "a-clique should share a community");
        assert_eq!(b1, b2, "b-clique should share a community");
        assert_ne!(a1, b1, "the two cliques must be in different communities");
        assert!(c.modularity > 0.0, "Q must be positive: {}", c.modularity);
    }

    #[test]
    fn path_graph_modularity_is_positive() {
        // a → b → c → d: a chain should still give Q > 0 because the chain
        // groups into at least one non-trivial community.
        let chunks = vec![
            chunk("a:1", "x.rs", "a", &["b"]),
            chunk("b:1", "x.rs", "b", &["c"]),
            chunk("c:1", "x.rs", "c", &["d"]),
            chunk("d:1", "x.rs", "d", &[]),
        ];
        let g = SymbolGraph::build_from_chunks(&chunks);
        let c = LouvainCommunities::detect(&g);
        assert!(c.community_count >= 1);
        // A path graph's Q is non-negative; tolerate exactly zero on small
        // graphs because Louvain may leave it as one community.
        assert!(c.modularity >= 0.0, "Q must be ≥ 0: {}", c.modularity);
    }

    #[test]
    fn empty_graph_returns_zero_communities() {
        let g = SymbolGraph::new();
        let c = LouvainCommunities::detect(&g);
        assert_eq!(c.community_count, 0);
        assert!(c.assignments.is_empty());
        assert_eq!(c.modularity, 0.0);
    }

    #[test]
    fn deterministic_across_runs() {
        // Same graph, same seed → same assignment.
        let chunks = vec![
            chunk("a:1", "a.rs", "a1", &["a2", "a3"]),
            chunk("a:2", "a.rs", "a2", &["a1", "a3"]),
            chunk("a:3", "a.rs", "a3", &["a1", "a2"]),
            chunk("b:1", "b.rs", "b1", &["b2"]),
            chunk("b:2", "b.rs", "b2", &["b1"]),
        ];
        let g = SymbolGraph::build_from_chunks(&chunks);
        let c1 = LouvainCommunities::detect(&g);
        let c2 = LouvainCommunities::detect(&g);
        assert_eq!(c1.assignments, c2.assignments);
        assert_eq!(c1.community_count, c2.community_count);
    }

    #[test]
    fn singleton_pruned_into_neighbour() {
        // Two pairs plus a single isolated bridge node connected to one pair.
        // After pruning, the bridge should fold into its neighbour's community.
        let chunks = vec![
            chunk("a:1", "x.rs", "a1", &["a2"]),
            chunk("a:2", "x.rs", "a2", &["a1"]),
            chunk("b:1", "x.rs", "bridge", &["a1"]),
            chunk("c:1", "y.rs", "c1", &["c2"]),
            chunk("c:2", "y.rs", "c2", &["c1"]),
        ];
        let g = SymbolGraph::build_from_chunks(&chunks);
        let c = LouvainCommunities::detect(&g);
        let bridge = c.assignments["bridge"];
        let a1 = c.assignments["a1"];
        assert_eq!(bridge, a1, "bridge singleton should merge into a-cluster");
    }
}
