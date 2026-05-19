//! Vector store trait and usearch HNSW implementation.
//!
//! Why: Most queries hit the vector index; making it pluggable lets us mock it in
//! tests and swap implementations without touching retrieval code.
//! What: `VectorStore` async trait + `UsearchStore` backed by an
//! `Arc<RwLock<usearch::Index>>` persisted to disk after each mutation.
//! Test: `upsert` then `search` returns the inserted id at rank 0 with score
//! at least 0.99 for an identical query vector; `remove` then `search` no
//! longer returns the removed id; reopening the store from the same path
//! retrieves previously inserted vectors.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct VectorHit {
    pub drawer_id: Uuid,
    pub score: f32,
}

/// Result summary returned by `UsearchStore::compact_orphans`.
///
/// Why: CLI / MCP callers need a structured report (not just a count) so they
/// can render progress like "checked 644 vectors, removed 541 orphans (84%)"
/// without re-deriving totals from the store.
/// What: Plain data: total tracked vector ids inspected, count removed as
/// orphans, and the index size before/after compaction (for divergence
/// reporting when the session key_map is incomplete after a cold reload).
/// Test: `compact_orphans_removes_only_missing_ids` exercises the values.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompactionResult {
    pub total_checked: usize,
    pub orphans_removed: usize,
    pub index_size_before: usize,
    pub index_size_after: usize,
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    async fn upsert(&self, id: Uuid, embedding: Vec<f32>) -> Result<()>;
    async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<VectorHit>>;
    async fn remove(&self, id: Uuid) -> Result<()>;
}

/// Initial capacity reserved when creating a new index. usearch grows the
/// index dynamically when `add` exceeds capacity, but reserving a reasonable
/// chunk up front avoids many tiny reallocations during palace warm-up.
const DEFAULT_INITIAL_CAPACITY: usize = 1024;

/// Sidecar filename appended to the usearch index path, holding the
/// `u64 -> Uuid` key map as JSON so cold reloads recover the full UUID for
/// every vector (rather than the zero-padded fallback).
const KEY_MAP_SIDECAR: &str = ".keymap.json";

/// Build the sidecar path next to the usearch index file.
fn key_map_sidecar_path(index_path: &std::path::Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_owned();
    s.push(KEY_MAP_SIDECAR);
    PathBuf::from(s)
}

/// Load `key_map` from disk if present; return empty on any read/parse error.
///
/// Why: A best-effort hydrate keeps cold reloads safe. If the sidecar is
/// missing or corrupt, we degrade to the pre-fix behavior (empty map) instead
/// of refusing to open the palace.
/// What: Reads the JSON sidecar as `Vec<(u64, Uuid)>` and collects into a map.
fn load_key_map_sidecar(index_path: &std::path::Path) -> HashMap<u64, Uuid> {
    let sidecar = key_map_sidecar_path(index_path);
    let Ok(bytes) = std::fs::read(&sidecar) else {
        return HashMap::new();
    };
    let Ok(entries) = serde_json::from_slice::<Vec<(u64, Uuid)>>(&bytes) else {
        tracing::warn!(?sidecar, "key_map sidecar parse failed; starting empty");
        return HashMap::new();
    };
    entries.into_iter().collect()
}

/// Persist `key_map` to disk next to the usearch index. Best-effort; logs on
/// failure so an unwritable sidecar doesn't fail the upsert / remove call.
fn save_key_map_sidecar(index_path: &std::path::Path, key_map: &HashMap<u64, Uuid>) {
    let sidecar = key_map_sidecar_path(index_path);
    let entries: Vec<(u64, Uuid)> = key_map.iter().map(|(k, v)| (*k, *v)).collect();
    match serde_json::to_vec(&entries) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&sidecar, bytes) {
                tracing::warn!(?sidecar, "key_map sidecar write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("key_map sidecar serialize failed: {e}"),
    }
}

/// Convert a UUID into a u64 key suitable for usearch.
///
/// Why: usearch keys are u64 but our drawer ids are UUIDs. Taking the first 8
/// bytes (little-endian) of the UUID is collision-resistant enough for a
/// per-palace index (UUID v4 entropy in those bytes is 64 bits).
/// What: Returns `u64::from_le_bytes(uuid.as_bytes()[..8])`.
/// Test: Round-trip via `key_to_uuid` preserves the first 8 bytes.
fn uuid_to_key(id: Uuid) -> u64 {
    let bytes = id.as_bytes();
    let mut head = [0u8; 8];
    head.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(head)
}

/// Reverse of `uuid_to_key`. Last 8 bytes of the resulting UUID are zero —
/// search results are matched back to drawers via the first-8-byte prefix.
///
/// Why: We only ever round-trip keys we wrote ourselves; callers must reconcile
/// the returned UUID with their own drawer table by prefix or by storing the
/// `(key -> drawer_id)` mapping at upsert time.
/// What: Builds a 16-byte UUID with `key.to_le_bytes()` in the first 8 bytes
/// and zeros in the last 8.
/// Test: `key_to_uuid(uuid_to_key(id))` matches `id` on the first 8 bytes.
fn key_to_uuid(key: u64) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&key.to_le_bytes());
    Uuid::from_bytes(bytes)
}

/// usearch HNSW-backed store.
///
/// Why: usearch gives us high-quality HNSW with disk persistence and a tiny C++
/// dependency. We wrap the index in `Arc<RwLock<_>>` so many concurrent reads
/// (search) never block each other; only mutations take the write lock.
/// What: Holds an `Arc<RwLock<usearch::Index>>` plus the on-disk path and
/// vector dimensionality. `upsert` / `remove` persist the index after every
/// mutation; `search` returns hits with score = `1.0 - distance` (cosine).
/// Test: See module-level tests covering insert+search, remove, and reload.
pub struct UsearchStore {
    index: Arc<RwLock<Index>>,
    path: PathBuf,
    #[allow(dead_code)]
    dim: usize,
    /// Maps usearch u64 key -> original full Uuid, for lossless round-trip.
    ///
    /// Why: `key_to_uuid` only recovers the first 8 bytes; without this map
    /// search results carry zero-padded UUIDs and dedup against the in-memory
    /// drawer table by full UUID equality silently fails (same drawer can
    /// appear in both L1 and L2 results).
    /// What: Populated on every `upsert`, evicted on `remove`. Empty after a
    /// cold reload from disk (TODO: rebuild from index iteration).
    /// Test: `upsert_then_l1_l2_no_duplicate` asserts `search` returns the
    /// original full UUID rather than the zero-padded fallback.
    key_map: Arc<RwLock<HashMap<u64, Uuid>>>,
}

impl UsearchStore {
    /// Open or create a usearch HNSW index at `path` with `dim`-dimensional
    /// f32 vectors and cosine similarity.
    ///
    /// Why: A palace's vector index must survive process restarts; opening
    /// transparently from the same path is the contract the registry relies
    /// on.
    /// What: If `path` exists, build an `Index` matching the on-disk header
    /// and `load` it; otherwise build a fresh `Index` with sensible defaults
    /// and reserve initial capacity. Either way, return a store wrapping it
    /// in `Arc<RwLock<_>>`.
    /// Test: Create store, upsert, drop store, reopen at the same path,
    /// search returns the previously inserted vector.
    pub fn new(path: PathBuf, dim: usize) -> Result<Self> {
        let options = IndexOptions {
            dimensions: dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..Default::default()
        };

        let index = Index::new(&options)
            .map_err(|e| anyhow::anyhow!("failed to create usearch index: {e}"))?;

        if path.exists() {
            let path_str = path
                .to_str()
                .with_context(|| format!("usearch path is not valid UTF-8: {path:?}"))?;
            index.load(path_str).map_err(|e| {
                anyhow::anyhow!("failed to load usearch index from {path_str}: {e}")
            })?;
        } else {
            // Ensure parent directory exists before any future save.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent dir for usearch index: {parent:?}")
                    })?;
                }
            }
            index
                .reserve(DEFAULT_INITIAL_CAPACITY)
                .map_err(|e| anyhow::anyhow!("failed to reserve usearch capacity: {e}"))?;
        }

        // Hydrate key_map from the sidecar file if present; otherwise start
        // empty. The sidecar lives next to the usearch index and is rewritten
        // on every upsert / remove (cheap; small JSON).
        let key_map = load_key_map_sidecar(&path);

        Ok(Self {
            index: Arc::new(RwLock::new(index)),
            path,
            dim,
            key_map: Arc::new(RwLock::new(key_map)),
        })
    }

    /// Number of vectors currently in the index.
    ///
    /// Why: Cold-start diagnostics compare the HNSW size to the drawer table
    /// size to surface orphaned vectors (issue #32).
    /// What: Acquires a read lock and returns `Index::size()`.
    /// Test: Indirectly via `PalaceHandle::open` warnings.
    pub fn index_size(&self) -> usize {
        self.index.read().size()
    }

    /// Reset the HNSW index to an empty state, discarding all vectors and
    /// clearing the in-memory + on-disk key map.
    ///
    /// Why: When the index has accumulated orphans we cannot address (because
    /// usearch doesn't expose enumeration and our session `key_map` only
    /// tracks vectors we wrote ourselves), the cheapest remediation is to
    /// rebuild from the authoritative drawer table. This method clears the
    /// index so the caller can re-upsert from drawers.
    /// What: Replaces the inner `Index` with a fresh one matching the original
    /// options, saves it (overwriting the on-disk index file), and truncates
    /// the key_map sidecar.
    /// Test: Indirectly via the dream compaction rebuild path
    /// (`dream_cycle_compacts_orphaned_vectors` and live palace cleanup).
    pub fn reset(&self) -> Result<()> {
        let options = IndexOptions {
            dimensions: self.dim,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..Default::default()
        };
        let new_index = Index::new(&options)
            .map_err(|e| anyhow::anyhow!("failed to recreate usearch index: {e}"))?;
        new_index
            .reserve(DEFAULT_INITIAL_CAPACITY)
            .map_err(|e| anyhow::anyhow!("failed to reserve usearch capacity: {e}"))?;

        let path_str = self
            .path
            .to_str()
            .with_context(|| format!("usearch path is not valid UTF-8: {:?}", self.path))?;
        new_index
            .save(path_str)
            .map_err(|e| anyhow::anyhow!("failed to save empty usearch index: {e}"))?;

        *self.index.write() = new_index;
        {
            let mut km = self.key_map.write();
            km.clear();
            save_key_map_sidecar(&self.path, &km);
        }
        Ok(())
    }

    /// Snapshot of all drawer ids currently tracked by this store's key map.
    ///
    /// Why: The dream compaction pass needs to enumerate vector entries so it
    /// can detect orphans (vectors with no surviving drawer row) and remove
    /// them. usearch's FFI does not expose a way to iterate all keys, so we
    /// use the parallel `key_map` populated on every `upsert` as the
    /// authoritative session view of "what's in the index".
    /// What: Acquires a read lock on `key_map` and clones the value set.
    /// Returns an empty vec on cold reload (before any upsert in this
    /// session) — see the `key_map` TODO for the long-term fix.
    /// Test: `dream_cycle_compacts_orphaned_vectors` exercises this path.
    pub fn all_ids(&self) -> Vec<Uuid> {
        self.key_map.read().values().copied().collect()
    }

    /// Remove vector entries whose drawer IDs are not in `valid_ids`.
    ///
    /// Why: Issue #49 — over a palace's lifetime, vectors get orphaned by
    /// partial writes, schema migrations, or older bugs that dropped drawer
    /// rows without removing the corresponding HNSW entry. The dream loop has
    /// a compaction pass, but operators need an on-demand fix that runs
    /// without the full async dream machinery (and can be triggered from the
    /// CLI against a palace data dir directly).
    /// What: Snapshots the session `key_map` (the authoritative "what's in
    /// the index" view), removes any key whose drawer UUID is not in
    /// `valid_ids`, and persists the index + sidecar after the batch. Returns
    /// a `CompactionResult` with the inspected count, the orphan count, and
    /// the index size before/after.
    /// Test: `compact_orphans_removes_only_missing_ids`.
    pub fn compact_orphans(&self, valid_ids: &HashSet<Uuid>) -> Result<CompactionResult> {
        let index_size_before = self.index.read().size();

        // Snapshot the (key, uuid) pairs we know about, then drop the read
        // lock before acquiring the write lock below.
        let pairs: Vec<(u64, Uuid)> = {
            let map = self.key_map.read();
            map.iter().map(|(k, v)| (*k, *v)).collect()
        };
        let total_checked = pairs.len();

        let mut orphans_removed: usize = 0;
        {
            let index_guard = self.index.write();
            let mut map_guard = self.key_map.write();
            for (key, drawer_id) in &pairs {
                if valid_ids.contains(drawer_id) {
                    continue;
                }
                match index_guard.remove(*key) {
                    Ok(_) => {
                        map_guard.remove(key);
                        orphans_removed += 1;
                    }
                    Err(e) => {
                        tracing::warn!(?drawer_id, "compact_orphans: usearch remove failed: {e}");
                    }
                }
            }

            // Persist once at the end so we don't pay a save per removal.
            if orphans_removed > 0 {
                let path_str = self
                    .path
                    .to_str()
                    .with_context(|| format!("usearch path is not valid UTF-8: {:?}", self.path))?;
                index_guard
                    .save(path_str)
                    .map_err(|e| anyhow::anyhow!("failed to save usearch index: {e}"))?;
                save_key_map_sidecar(&self.path, &map_guard);
            }
        }

        let index_size_after = self.index.read().size();
        Ok(CompactionResult {
            total_checked,
            orphans_removed,
            index_size_before,
            index_size_after,
        })
    }
}

#[async_trait]
impl VectorStore for UsearchStore {
    async fn upsert(&self, id: Uuid, embedding: Vec<f32>) -> Result<()> {
        let index = self.index.clone();
        let key_map = self.key_map.clone();
        let path = self.path.clone();
        // Index operations are CPU/IO-bound C++ calls; run on a blocking thread
        // so we don't stall the async reactor.
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = index.write();
            let key = uuid_to_key(id);

            // Grow capacity if we're at the limit. usearch's `add` can fail
            // when capacity is exhausted; reserve in chunks rather than
            // doubling on every insert.
            let size = guard.size();
            let capacity = guard.capacity();
            if size + 1 > capacity {
                let new_capacity = (capacity.max(DEFAULT_INITIAL_CAPACITY)).saturating_mul(2);
                guard
                    .reserve(new_capacity)
                    .map_err(|e| anyhow::anyhow!("failed to grow usearch capacity: {e}"))?;
            }

            // `add` updates an existing key's vector in-place when the index
            // was built with `multi=false` (our default).
            guard
                .add(key, &embedding)
                .map_err(|e| anyhow::anyhow!("failed to add vector to usearch: {e}"))?;

            // Record the full UUID so search() can return it losslessly.
            {
                let mut km = key_map.write();
                km.insert(key, id);
                save_key_map_sidecar(&path, &km);
            }

            let path_str = path
                .to_str()
                .with_context(|| format!("usearch path is not valid UTF-8: {path:?}"))?;
            guard
                .save(path_str)
                .map_err(|e| anyhow::anyhow!("failed to save usearch index: {e}"))?;
            Ok(())
        })
        .await
        .context("upsert task panicked")??;
        Ok(())
    }

    async fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<VectorHit>> {
        let index = self.index.clone();
        let key_map = self.key_map.clone();
        let query = query.to_vec();
        tokio::task::spawn_blocking(move || -> Result<Vec<VectorHit>> {
            let guard = index.read();
            let matches = guard
                .search(&query, top_k)
                .map_err(|e| anyhow::anyhow!("usearch search failed: {e}"))?;

            let map_guard = key_map.read();
            let mut hits: Vec<VectorHit> = matches
                .keys
                .into_iter()
                .zip(matches.distances)
                .map(|(key, distance)| {
                    // Prefer the original full UUID we stored at upsert; fall
                    // back to the zero-padded reconstruction for entries we
                    // haven't seen this session (cold reload from disk).
                    let drawer_id = map_guard
                        .get(&key)
                        .copied()
                        .unwrap_or_else(|| key_to_uuid(key));
                    VectorHit {
                        drawer_id,
                        // Cosine distance in usearch is `1 - cosine_similarity`,
                        // so similarity = 1 - distance. Clamp to keep the
                        // floating-point boundary clean for callers that compare
                        // to thresholds like 0.99.
                        score: (1.0_f32 - distance).clamp(0.0, 1.0),
                    }
                })
                .collect();
            drop(map_guard);

            // usearch already returns ascending distance, so descending score
            // is implied — but make it explicit for downstream consumers.
            hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Ok(hits)
        })
        .await
        .context("search task panicked")?
    }

    async fn remove(&self, id: Uuid) -> Result<()> {
        let index = self.index.clone();
        let key_map = self.key_map.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = index.write();
            let key = uuid_to_key(id);
            guard
                .remove(key)
                .map_err(|e| anyhow::anyhow!("failed to remove vector from usearch: {e}"))?;
            {
                let mut km = key_map.write();
                km.remove(&key);
                save_key_map_sidecar(&path, &km);
            }

            let path_str = path
                .to_str()
                .with_context(|| format!("usearch path is not valid UTF-8: {path:?}"))?;
            guard
                .save(path_str)
                .map_err(|e| anyhow::anyhow!("failed to save usearch index: {e}"))?;
            Ok(())
        })
        .await
        .context("remove task panicked")??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn unit_vec(dim: usize, seed: u32) -> Vec<f32> {
        let raw: Vec<f32> = (0..dim).map(|i| ((i as u32 + seed) as f32) + 1.0).collect();
        let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        raw.into_iter().map(|x| x / norm).collect()
    }

    #[tokio::test]
    async fn upsert_then_search_returns_same_vector_at_rank_0() {
        let dir = tempdir().unwrap();
        let store = UsearchStore::new(dir.path().join("test.usearch"), 384).unwrap();
        let id = Uuid::new_v4();
        let v = unit_vec(384, 0);

        store.upsert(id, v.clone()).await.unwrap();
        let hits = store.search(&v, 1).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(uuid_to_key(hits[0].drawer_id), uuid_to_key(id));
        assert!(hits[0].score >= 0.99, "score was {}", hits[0].score);
    }

    #[tokio::test]
    async fn remove_clears_vector() {
        let dir = tempdir().unwrap();
        let store = UsearchStore::new(dir.path().join("test.usearch"), 384).unwrap();
        let id = Uuid::new_v4();
        let v = unit_vec(384, 7);
        store.upsert(id, v.clone()).await.unwrap();
        store.remove(id).await.unwrap();

        let hits = store.search(&v, 5).await.unwrap();
        assert!(
            !hits
                .iter()
                .any(|h| uuid_to_key(h.drawer_id) == uuid_to_key(id)),
            "removed id still present in results"
        );
    }

    #[tokio::test]
    async fn persist_and_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.usearch");
        let id = Uuid::new_v4();
        let v = unit_vec(384, 13);
        {
            let store = UsearchStore::new(path.clone(), 384).unwrap();
            store.upsert(id, v.clone()).await.unwrap();
        }
        let store2 = UsearchStore::new(path, 384).unwrap();
        let hits = store2.search(&v, 1).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(uuid_to_key(hits[0].drawer_id), uuid_to_key(id));
        assert!(hits[0].score >= 0.99, "score was {}", hits[0].score);
    }

    #[test]
    fn uuid_key_round_trip_preserves_prefix() {
        let id = Uuid::new_v4();
        let key = uuid_to_key(id);
        let round = key_to_uuid(key);
        assert_eq!(&id.as_bytes()[..8], &round.as_bytes()[..8]);
    }

    /// Why: Confirm that after an upsert+search cycle, the UUID we recover
    /// from the store equals the full original UUID (last 8 bytes preserved),
    /// so dedup across L1/L2 doesn't silently fail.
    /// What: Upsert a vector under a fresh `Uuid::new_v4`, search for it, and
    /// assert the returned `drawer_id` matches the input bit-for-bit.
    /// Test: This test itself is the verification.
    /// Why: Issue #49 — `compact_orphans` must remove only the vectors whose
    /// drawer UUIDs are absent from the supplied valid set, and must persist
    /// the change so a subsequent reload doesn't resurrect the orphans.
    /// What: Insert three vectors, mark one as valid, run compaction, then
    /// assert (a) total_checked counts all three, (b) two were removed, and
    /// (c) reopening the store from disk shows only the kept vector.
    /// Test: This test itself is the verification.
    #[tokio::test]
    async fn compact_orphans_removes_only_missing_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.usearch");
        let store = UsearchStore::new(path.clone(), 384).unwrap();

        let keep = Uuid::new_v4();
        let drop_a = Uuid::new_v4();
        let drop_b = Uuid::new_v4();
        store.upsert(keep, unit_vec(384, 1)).await.unwrap();
        store.upsert(drop_a, unit_vec(384, 2)).await.unwrap();
        store.upsert(drop_b, unit_vec(384, 3)).await.unwrap();

        let mut valid = HashSet::new();
        valid.insert(keep);
        let res = store.compact_orphans(&valid).unwrap();
        assert_eq!(res.total_checked, 3);
        assert_eq!(res.orphans_removed, 2);
        assert_eq!(res.index_size_before, 3);
        assert_eq!(res.index_size_after, 1);

        // Reopen from disk — the compacted state must survive.
        drop(store);
        let reopened = UsearchStore::new(path, 384).unwrap();
        let ids = reopened.all_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], keep);
    }

    #[tokio::test]
    async fn upsert_then_l1_l2_no_duplicate() {
        let dir = tempdir().unwrap();
        let store = UsearchStore::new(dir.path().join("test.usearch"), 384).unwrap();
        let id = Uuid::new_v4();
        let v = unit_vec(384, 42);

        store.upsert(id, v.clone()).await.unwrap();
        let hits = store.search(&v, 1).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].drawer_id, id,
            "search must return the full original UUID, not a zero-padded fallback"
        );
        // Last 8 bytes must be non-zero (otherwise dedup against the in-memory
        // drawer table would silently fail).
        assert_ne!(
            &hits[0].drawer_id.as_bytes()[8..],
            &[0u8; 8],
            "last 8 bytes were zeroed — the key_map round-trip is broken"
        );
    }
}
