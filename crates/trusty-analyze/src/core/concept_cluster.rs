//! Concept clustering: k-means over chunk embeddings.
//!
//! Why: groups semantically related chunks into concept clusters, surfacing
//! "what themes does this codebase contain?" without requiring a full KG.
//!
//! What: standard Lloyd's algorithm k-means with k-means++ seeded
//! initialization over embedding vectors. Callers supply embeddings; this
//! module does the math. A companion `bow_embedding` helper provides
//! TF-like hashed bag-of-words vectors so callers without a neural embedder
//! can still cluster.
//!
//! Test: `stable_clusters_on_synthetic_data` verifies that two visually
//! separated point clouds cluster correctly; additional tests cover
//! normalization, k-clamping, and a self-analysis smoke test over this
//! crate's own source.

#![allow(clippy::needless_range_loop)]

use ndarray::{Array1, Array2, ArrayView1};

/// One concept cluster: a label (the centroid's nearest chunk name) and
/// the chunk IDs assigned to it.
#[derive(Debug, Clone)]
pub struct ConceptCluster {
    /// Sequential cluster index (0..k).
    pub id: usize,
    /// Human-readable label: the id of the member chunk closest to the centroid.
    pub label: String,
    /// IDs of all chunks in this cluster.
    pub members: Vec<String>,
    /// Average L2 distance from members to centroid (lower = tighter cluster).
    pub cohesion: f32,
}

/// Result of a clustering run.
#[derive(Debug, Clone)]
pub struct ClusterResult {
    /// One entry per discovered cluster.
    pub clusters: Vec<ConceptCluster>,
    /// How many Lloyd iterations ran before convergence.
    pub iterations: usize,
}

/// Run k-means clustering on the provided embeddings.
///
/// Why: the public entry point used by both the HTTP service and tests.
/// Encapsulates initialization, Lloyd iteration, and labelling so callers
/// only deal in `(chunk_id, vector)` pairs.
///
/// What: k-means++ initialization seeded by `seed`, then Lloyd's algorithm
/// up to `max_iter` iterations or until assignments stabilize. `k` is
/// clamped to `embeddings.len()` if too large. Returns clusters sorted by
/// member count descending so the dominant concepts surface first.
///
/// Test: `stable_clusters_on_synthetic_data` covers convergence on two
/// well-separated point clouds; `cluster_clamps_k_to_input_size` covers
/// the clamping branch; `self_analysis_clusters` smoke-tests over this
/// crate's own source.
///
/// # Panics
/// Panics if `embeddings` is empty or vectors have different lengths.
pub fn cluster(
    embeddings: &[(String, Vec<f32>)],
    k: usize,
    max_iter: usize,
    seed: u64,
) -> ClusterResult {
    assert!(
        !embeddings.is_empty(),
        "cluster requires at least one embedding"
    );
    let dim = embeddings[0].1.len();
    assert!(dim > 0, "embeddings must have non-zero dimension");
    for (id, v) in embeddings {
        assert_eq!(
            v.len(),
            dim,
            "embedding for {id} has dim {} != {dim}",
            v.len()
        );
    }

    let n = embeddings.len();
    let k = k.max(1).min(n);

    // Build an (n, dim) data matrix.
    let mut data = Array2::<f32>::zeros((n, dim));
    for (i, (_, v)) in embeddings.iter().enumerate() {
        for (j, &x) in v.iter().enumerate() {
            data[[i, j]] = x;
        }
    }

    // k-means++ initialization.
    let mut rng = Lcg::new(seed);
    let centroids = kmeans_pp_init(&data, k, &mut rng);
    let mut centroids = centroids;

    let mut assignments = vec![0usize; n];
    let mut iterations = 0usize;
    for it in 0..max_iter.max(1) {
        iterations = it + 1;
        let mut changed = false;
        for i in 0..n {
            let row = data.row(i);
            let mut best = 0usize;
            let mut best_dist = f32::INFINITY;
            for c in 0..k {
                let centroid = centroids.row(c);
                let d = l2_sq(&row, &centroid);
                if d < best_dist {
                    best_dist = d;
                    best = c;
                }
            }
            if assignments[i] != best {
                assignments[i] = best;
                changed = true;
            }
        }

        // Recompute centroids as mean of assigned points.
        let mut new_centroids = Array2::<f32>::zeros((k, dim));
        let mut counts = vec![0u32; k];
        for i in 0..n {
            let c = assignments[i];
            counts[c] += 1;
            let mut row = new_centroids.row_mut(c);
            row += &data.row(i);
        }
        for c in 0..k {
            if counts[c] > 0 {
                let mut row = new_centroids.row_mut(c);
                row.mapv_inplace(|v| v / counts[c] as f32);
            } else {
                // Empty cluster: keep the old centroid to avoid NaN drift.
                new_centroids.row_mut(c).assign(&centroids.row(c));
            }
        }
        centroids = new_centroids;

        if !changed {
            break;
        }
    }

    // Build clusters with labels (closest member to centroid) and cohesion.
    let mut clusters: Vec<ConceptCluster> = (0..k)
        .map(|c| ConceptCluster {
            id: c,
            label: String::new(),
            members: Vec::new(),
            cohesion: 0.0,
        })
        .collect();

    for (i, (id, _)) in embeddings.iter().enumerate() {
        clusters[assignments[i]].members.push(id.clone());
    }

    for c in 0..k {
        let centroid = centroids.row(c);
        let mut closest_idx: Option<usize> = None;
        let mut closest_dist = f32::INFINITY;
        let mut sum_dist = 0.0f32;
        let mut count = 0u32;
        for i in 0..n {
            if assignments[i] != c {
                continue;
            }
            let d = l2_sq(&data.row(i), &centroid).sqrt();
            sum_dist += d;
            count += 1;
            if d < closest_dist {
                closest_dist = d;
                closest_idx = Some(i);
            }
        }
        clusters[c].cohesion = if count > 0 {
            sum_dist / count as f32
        } else {
            0.0
        };
        clusters[c].label = match closest_idx {
            Some(i) => embeddings[i].0.clone(),
            None => format!("cluster_{c}"),
        };
    }

    // Drop empty clusters and reassign sequential ids; sort by size desc.
    clusters.retain(|c| !c.members.is_empty());
    clusters.sort_by_key(|c| std::cmp::Reverse(c.members.len()));
    for (new_id, c) in clusters.iter_mut().enumerate() {
        c.id = new_id;
    }

    ClusterResult {
        clusters,
        iterations,
    }
}

/// Produce a simple TF-like hashed bag-of-words embedding for a text string.
///
/// Why: lets callers cluster without a neural embedder by deriving a
/// deterministic vector from chunk content. Quality is modest but the
/// pipeline always works.
/// What: lowercase, tokenize on non-alphanumeric boundaries, hash each
/// token into one of `dim` buckets, increment, then L2-normalize.
/// Test: `bow_embedding_is_normalized` and `bow_embedding_different_texts_differ`.
pub fn bow_embedding(text: &str, dim: usize) -> Vec<f32> {
    let dim = dim.max(1);
    let mut v = vec![0.0f32; dim];
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            for c in ch.to_lowercase() {
                current.push(c);
            }
        } else if !current.is_empty() {
            bucket_inc(&mut v, &current, dim);
            current.clear();
        }
    }
    if !current.is_empty() {
        bucket_inc(&mut v, &current, dim);
    }

    // L2-normalize.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Cluster chunk contents using BoW embeddings and emit `RawEntity` records
/// (`EntityType::ConceptCluster`) per cluster for knowledge-graph integration.
///
/// Why: the HTTP/MCP surface needs not only cluster assignments but also
/// graph-native entities so downstream KG consumers (search ranking, fact
/// store) can reference clusters by stable id.
/// What: builds a 256-dim BoW vector for each `(chunk_id, content)` pair, runs
/// `cluster()` with seed=42 and `max_iter=100`, then for each cluster emits a
/// `RawEntity::new(EntityType::ConceptCluster, label, (0,0), file, 0)`. The
/// id is the deterministic SHA-256 of `(ConceptCluster, label, file)` from
/// `RawEntity::new`, so re-running on the same input produces stable ids.
/// Test: `cluster_and_emit_entities_emits_one_per_cluster` exercises a small
/// two-cluster corpus and verifies each emitted entity has the expected
/// `entity_type`, non-empty `text`, and stable `id`.
pub fn cluster_and_emit_entities(
    contents: &[(&str, &str)],
    k: usize,
    file: &str,
) -> Vec<crate::types::RawEntity> {
    use crate::types::{EntityType, RawEntity};

    if contents.is_empty() {
        return Vec::new();
    }
    const BOW_DIM: usize = 256;
    let embeddings: Vec<(String, Vec<f32>)> = contents
        .iter()
        .map(|(id, content)| ((*id).to_string(), bow_embedding(content, BOW_DIM)))
        .collect();
    let result = cluster(&embeddings, k, 100, 42);
    result
        .clusters
        .into_iter()
        .map(|c| RawEntity::new(EntityType::ConceptCluster, c.label, (0, 0), file, 0))
        .collect()
}

fn bucket_inc(v: &mut [f32], token: &str, dim: usize) {
    let hash = token
        .bytes()
        .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
    let bucket = (hash % dim as u64) as usize;
    v[bucket] += 1.0;
}

fn l2_sq(a: &ArrayView1<f32>, b: &ArrayView1<f32>) -> f32 {
    let mut s = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = x - y;
        s += d * d;
    }
    s
}

/// k-means++ seeded initialization. Returns a (k, dim) centroid matrix.
fn kmeans_pp_init(data: &Array2<f32>, k: usize, rng: &mut Lcg) -> Array2<f32> {
    let n = data.nrows();
    let dim = data.ncols();
    let mut centroids = Array2::<f32>::zeros((k, dim));

    // Pick the first centroid uniformly at random.
    let first = (rng.next_u64() as usize) % n;
    centroids.row_mut(0).assign(&data.row(first));

    // Each subsequent centroid is chosen with probability proportional to
    // squared distance from the nearest existing centroid.
    let mut dists = Array1::<f32>::from_elem(n, f32::INFINITY);
    for c in 1..k {
        // Update dists against most recently added centroid (c-1).
        for i in 0..n {
            let d = l2_sq(&data.row(i), &centroids.row(c - 1));
            if d < dists[i] {
                dists[i] = d;
            }
        }
        let total: f32 = dists.iter().sum();
        if total <= 0.0 {
            // Degenerate: all points coincide with chosen centroids.
            // Fill remaining centroids with the same point.
            for cc in c..k {
                centroids.row_mut(cc).assign(&data.row(first));
            }
            return centroids;
        }
        // Weighted sample.
        let target = (rng.next_f32() * total).min(total);
        let mut acc = 0.0f32;
        let mut chosen = n - 1;
        for i in 0..n {
            acc += dists[i];
            if acc >= target {
                chosen = i;
                break;
            }
        }
        centroids.row_mut(c).assign(&data.row(chosen));
    }

    centroids
}

/// Tiny linear congruential generator for deterministic, seeded sampling.
///
/// Why: kmeans++ needs randomness; pulling `rand` into trusty-analyzer-core
/// just for two samples is overkill. This LCG is sufficient for centroid
/// initialization and keeps tests deterministic via `seed`.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        // Avoid the degenerate state=0 case.
        let s = if seed == 0 { 0x9E3779B97F4A7C15 } else { seed };
        Self { state: s }
    }

    fn next_u64(&mut self) -> u64 {
        // Numerical Recipes LCG constants.
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn next_f32(&mut self) -> f32 {
        // Take the high 24 bits for a value in [0, 1).
        ((self.next_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_clusters_on_synthetic_data() {
        // Two clearly separated clouds: A-group near (1,0), B-group near (-1,0)
        let mut embeddings = Vec::new();
        for i in 0..5 {
            embeddings.push((format!("a{i}"), vec![1.0 + i as f32 * 0.01, 0.0]));
        }
        for i in 0..5 {
            embeddings.push((format!("b{i}"), vec![-1.0 - i as f32 * 0.01, 0.0]));
        }
        let result = cluster(&embeddings, 2, 100, 42);
        assert_eq!(result.clusters.len(), 2);
        // Each cluster should have exactly 5 members
        let sizes: Vec<_> = result.clusters.iter().map(|c| c.members.len()).collect();
        assert!(
            sizes.iter().all(|&s| s == 5),
            "expected 5 per cluster, got {sizes:?}"
        );
    }

    #[test]
    fn bow_embedding_is_normalized() {
        let v = bow_embedding("fn compute_complexity content rust", 256);
        assert_eq!(v.len(), 256);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding not normalized: norm={norm}"
        );
    }

    #[test]
    fn bow_embedding_different_texts_differ() {
        let a = bow_embedding("fn compute_complexity", 256);
        let b = bow_embedding("struct CodeChunk", 256);
        let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!(
            dot < 0.99,
            "different texts should not be identical: dot={dot}"
        );
    }

    #[test]
    fn cluster_clamps_k_to_input_size() {
        let embeddings: Vec<_> = (0..3)
            .map(|i| (format!("c{i}"), vec![i as f32, 0.0]))
            .collect();
        let result = cluster(&embeddings, 10, 50, 0); // k=10 > 3 inputs
        assert!(result.clusters.len() <= 3, "k should be clamped");
    }

    #[test]
    fn cluster_and_emit_entities_emits_one_per_cluster() {
        use crate::types::EntityType;

        let contents = vec![
            ("a1", "async tokio runtime spawn task"),
            ("a2", "async tokio runtime spawn future"),
            ("a3", "async runtime spawn task await"),
            ("b1", "serde serialize derive json"),
            ("b2", "serde serialize derive json wire"),
            ("b3", "serde derive json wire format"),
        ];
        let entities = cluster_and_emit_entities(&contents, 2, "src/lib.rs");
        assert!(
            !entities.is_empty() && entities.len() <= 2,
            "expected 1..=2 entities, got {}",
            entities.len()
        );
        for e in &entities {
            assert_eq!(e.entity_type, EntityType::ConceptCluster);
            assert!(!e.text.is_empty(), "cluster label should be non-empty");
            assert_eq!(e.file, "src/lib.rs");
            assert!(!e.id.is_empty(), "RawEntity::new must produce an id");
        }
        // Determinism: same inputs yield same ids.
        let again = cluster_and_emit_entities(&contents, 2, "src/lib.rs");
        let ids_a: Vec<_> = entities.iter().map(|e| &e.id).collect();
        let ids_b: Vec<_> = again.iter().map(|e| &e.id).collect();
        assert_eq!(ids_a, ids_b, "entity ids must be stable across runs");
    }

    #[test]
    fn cluster_and_emit_entities_empty_when_no_inputs() {
        let entities = cluster_and_emit_entities(&[], 4, "src/foo.rs");
        assert!(entities.is_empty());
    }

    #[test]
    fn self_analysis_clusters() {
        // Run clustering on the project's own source as an integration smoke test
        use crate::core::test_utils::chunks_from_dir;
        use std::path::Path;
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let chunks = chunks_from_dir(&src, ".rs").expect("read own src");

        let embeddings: Vec<_> = chunks
            .iter()
            .map(|c| (c.id.clone(), bow_embedding(&c.content, 256)))
            .collect();

        let result = cluster(&embeddings, 5, 100, 42);
        assert!(
            !result.clusters.is_empty() && result.clusters.len() <= 5,
            "expected up to 5 clusters, got {}",
            result.clusters.len()
        );
        // Every chunk should be in exactly one cluster
        let total_members: usize = result.clusters.iter().map(|c| c.members.len()).sum();
        assert_eq!(
            total_members,
            chunks.len(),
            "every chunk must be in exactly one cluster"
        );
        println!(
            "self-analysis clusters: {} iters, sizes: {:?}",
            result.iterations,
            result
                .clusters
                .iter()
                .map(|c| c.members.len())
                .collect::<Vec<_>>()
        );
    }
}
