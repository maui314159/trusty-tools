//! Integration tests for the petgraph-backed adjacency layer on
//! `KnowledgeGraph` (issue #48).
//!
//! Why: The in-memory `StableGraph` cache that backs `neighbors` and
//! `shortest_path` must stay in lock-step with the redb-backed store —
//! hydrate from disk on open, gain edges on `assert`, lose edges on
//! `retract`, keep nodes around through retract for stable indices. These
//! tests exercise the public `KnowledgeGraph` surface so they catch
//! regressions where the cache silently diverges from the persisted state.
//! What: Builds a `KnowledgeGraph` in a temp dir, drives the public
//! `assert` / `retract` API, then asserts on `neighbors` / `shortest_path`
//! results and (for hydration) the cumulative count of returned edges.
//! Test: `cargo test -p trusty-common --features memory-core --test
//! kg_graph_tests`.

#![cfg(feature = "memory-core")]

use chrono::Utc;
use tempfile::tempdir;
use trusty_common::memory_core::store::kg::{KgEdge, KnowledgeGraph, Triple};

/// Why: Centralise triple construction so each test only restates the
/// `(subject, predicate, object)` triple under exercise.
/// What: Builds an active triple with `valid_from = now`, confidence 1.0,
/// and no provenance.
/// Test: Indirect — used by every test in this file.
fn t(subject: &str, predicate: &str, object: &str) -> Triple {
    Triple {
        subject: subject.into(),
        predicate: predicate.into(),
        object: object.into(),
        valid_from: Utc::now(),
        valid_to: None,
        confidence: 1.0,
        provenance: None,
    }
}

/// Why: After dropping a `KnowledgeGraph` handle and re-opening the same
/// palace, the adjacency cache must be rebuilt from the persisted active
/// triples — otherwise process restarts (or palace reopens) would yield
/// an empty graph until the next assertion.
/// What: Seeds five triples through `assert`, reopens the palace, and
/// asserts the neighbour counts add up to five distinct active edges.
#[tokio::test]
async fn hydration_populates_graph() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("kg.db");
    {
        let kg = KnowledgeGraph::open(&path).unwrap();
        for (s, p, o) in [
            ("alice", "knows", "bob"),
            ("bob", "knows", "carol"),
            ("carol", "knows", "dave"),
            ("alice", "manages", "eve"),
            ("eve", "reports_to", "alice"),
        ] {
            kg.assert(t(s, p, o)).await.unwrap();
        }
    }

    // Reopen — the cache was dropped with the prior handle; this open
    // must hydrate from redb.
    let kg = KnowledgeGraph::open(&path).unwrap();

    // Each undirected neighbour query reports both outgoing and incoming
    // edges, so sum of degrees should equal 2 * edge_count.
    let total_degree: usize = ["alice", "bob", "carol", "dave", "eve"]
        .iter()
        .map(|n| kg.neighbors(n).unwrap().len())
        .sum();
    assert_eq!(total_degree, 2 * 5, "expected 5 edges hydrated");
}

/// Why: `assert` must update the in-memory cache *after* the redb commit;
/// the post-commit state must include the new edge so the next
/// `neighbors` call sees it without a reopen.
/// What: Asserts a single triple and checks both endpoints see each other
/// as neighbours.
#[tokio::test]
async fn assert_adds_edge() {
    let dir = tempdir().unwrap();
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
    kg.assert(t("alice", "knows", "bob")).await.unwrap();

    let alice_neighbors: Vec<(String, KgEdge)> = kg.neighbors("alice").unwrap();
    assert_eq!(alice_neighbors.len(), 1, "alice has one outgoing edge");
    assert_eq!(alice_neighbors[0].0, "bob");
    assert_eq!(alice_neighbors[0].1.predicate, "knows");

    let bob_neighbors = kg.neighbors("bob").unwrap();
    assert_eq!(bob_neighbors.len(), 1, "bob has one incoming edge");
    assert_eq!(bob_neighbors[0].0, "alice");
}

/// Why: `retract` closes the active interval at `(subject, predicate)`;
/// the cache must drop the edge but keep both nodes so other edges on
/// either endpoint (and stable NodeIndex values) survive.
/// What: Asserts an edge, retracts it, then checks that `neighbors`
/// returns no edge for the subject — but a *new* edge to the same node
/// still works (proving the node was preserved).
#[tokio::test]
async fn retract_removes_edge() {
    let dir = tempdir().unwrap();
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
    kg.assert(t("alice", "knows", "bob")).await.unwrap();
    assert_eq!(kg.neighbors("alice").unwrap().len(), 1);

    let closed = kg.retract("alice", "knows").await.unwrap();
    assert_eq!(closed, 1);
    assert!(
        kg.neighbors("alice").unwrap().is_empty(),
        "retract should remove the edge from alice"
    );
    assert!(
        kg.neighbors("bob").unwrap().is_empty(),
        "retract should remove the edge from bob too"
    );

    // Re-asserting the same edge re-uses the preserved nodes — this is
    // the load-bearing claim for "do NOT remove nodes" in the spec.
    kg.assert(t("alice", "likes", "bob")).await.unwrap();
    let after = kg.neighbors("alice").unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, "bob");
    assert_eq!(after[0].1.predicate, "likes");
}

/// Why: `neighbors` is the single-hop traversal primitive; it must report
/// both outgoing and incoming edges so callers can reason without
/// repeating the query in the opposite direction.
/// What: Builds an A→B→C chain and asserts that B sees both A and C as
/// neighbours.
#[tokio::test]
async fn neighbors_returns_connected() {
    let dir = tempdir().unwrap();
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
    kg.assert(t("a", "links", "b")).await.unwrap();
    kg.assert(t("b", "links", "c")).await.unwrap();

    let mut b_neighbors: Vec<String> = kg
        .neighbors("b")
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    b_neighbors.sort();
    assert_eq!(b_neighbors, vec!["a".to_string(), "c".to_string()]);

    // A only has the outgoing edge to B.
    let a_neighbors = kg.neighbors("a").unwrap();
    assert_eq!(a_neighbors.len(), 1);
    assert_eq!(a_neighbors[0].0, "b");

    // Unknown entity yields empty.
    assert!(kg.neighbors("nope").unwrap().is_empty());
}

/// Why: `shortest_path` underpins multi-hop reasoning (issues #7, #10);
/// the directed-edge Dijkstra must return entity names in the correct
/// order from start to goal.
/// What: Builds A→B→C and asserts the shortest path from A to C is
/// exactly `[A, B, C]`; verifies same-node and unreachable cases too.
#[tokio::test]
async fn shortest_path_finds_route() {
    let dir = tempdir().unwrap();
    let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
    kg.assert(t("A", "step", "B")).await.unwrap();
    kg.assert(t("B", "step", "C")).await.unwrap();

    let path = kg.shortest_path("A", "C").unwrap();
    assert_eq!(
        path,
        Some(vec!["A".to_string(), "B".to_string(), "C".to_string()])
    );

    // Same-node shortest path is the trivial single-node path.
    let self_path = kg.shortest_path("A", "A").unwrap();
    assert_eq!(self_path, Some(vec!["A".to_string()]));

    // Unknown endpoint returns None (no panic).
    assert_eq!(kg.shortest_path("A", "missing").unwrap(), None);
    assert_eq!(kg.shortest_path("missing", "C").unwrap(), None);

    // Disconnected graph: add an isolated node X via a self-irrelevant
    // edge, then assert no path from A to X exists.
    kg.assert(t("X", "knows", "Y")).await.unwrap();
    assert_eq!(kg.shortest_path("A", "X").unwrap(), None);
}
