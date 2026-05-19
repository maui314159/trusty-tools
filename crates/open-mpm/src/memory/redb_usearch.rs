//! Concrete `MemoryStore` backed by redb (metadata) + usearch (HNSW vectors).
//!
//! Why: We want an embeddable, single-process store with durable persistence
//! and fast approximate nearest-neighbor search, without running a separate
//! database server. redb gives us transactional k/v storage for payloads and
//! id<->label mappings; usearch gives us a state-of-the-art HNSW index. This
//! module glues them together behind the `MemoryStore` trait so callers never
//! see either engine directly.
//! What: `RedbUsearchStore::open` opens/creates a store directory containing a
//! `store.redb` file and per-segment `.usearch` files. Inserts allocate an
//! auto-incrementing u64 label per segment (tracked in redb), add the vector
//! to the segment's usearch index under that label, and store the payload
//! keyed by `"{prefix}:{id}"`. Searches translate labels back to ids via redb
//! and fetch payloads. Deletes tombstone the vector in usearch and drop all
//! redb rows for the id.
//! Test: See `tests` module below — round-trip insert+search, segment
//! isolation, get-by-id, and persistence after reopen.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use tokio::sync::Mutex;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use super::store::{MemoryResult, MemoryStore, Segment};

// --- redb table definitions ---------------------------------------------

/// Key format: `"{prefix}:{id}"`; value: JSON-serialized payload string.
const PAYLOAD_TABLE: TableDefinition<&str, &str> = TableDefinition::new("payloads");

/// Segment-scoped bidirectional label<->id maps. Labels are u64 (usearch
/// requirement); ids are arbitrary caller strings (e.g., UUIDs, file paths).
const MEM_LABEL_TO_ID: TableDefinition<u64, &str> = TableDefinition::new("mem_label_to_id");
const MEM_ID_TO_LABEL: TableDefinition<&str, u64> = TableDefinition::new("mem_id_to_label");
const CODE_LABEL_TO_ID: TableDefinition<u64, &str> = TableDefinition::new("code_label_to_id");
const CODE_ID_TO_LABEL: TableDefinition<&str, u64> = TableDefinition::new("code_id_to_label");
const CTX_LABEL_TO_ID: TableDefinition<u64, &str> = TableDefinition::new("ctx_label_to_id");
const CTX_ID_TO_LABEL: TableDefinition<&str, u64> = TableDefinition::new("ctx_id_to_label");
const BRIEF_LABEL_TO_ID: TableDefinition<u64, &str> = TableDefinition::new("brief_label_to_id");
const BRIEF_ID_TO_LABEL: TableDefinition<&str, u64> = TableDefinition::new("brief_id_to_label");
const HIST_LABEL_TO_ID: TableDefinition<u64, &str> = TableDefinition::new("hist_label_to_id");
const HIST_ID_TO_LABEL: TableDefinition<&str, u64> = TableDefinition::new("hist_id_to_label");

/// Auto-incrementing u64 counters, keyed by segment prefix.
const COUNTER_TABLE: TableDefinition<&str, u64> = TableDefinition::new("counters");

/// Default initial capacity reservation for a freshly-created usearch index.
/// Why: usearch panics/errors on `add` before `reserve` has been called with
/// capacity >= size+1. We bump this in `ensure_capacity` as the index grows.
const INITIAL_RESERVE: usize = 64;

/// Growth factor when the usearch index fills up.
const GROWTH_FACTOR: usize = 2;

// --- store --------------------------------------------------------------

/// redb + usearch backed memory store. Clone-friendly via internal `Arc`s.
///
/// Why: A single instance is meant to be shared across async tasks; the
/// inner `Arc<Mutex<Index>>` handles concurrent access to the non-reentrant
/// usearch indexes, while redb has its own transactional concurrency.
/// What: Holds one `Database` (multi-table) and two `Index` handles, plus
/// the on-disk paths so we can call `save` after mutations.
/// Test: Construction verified in `tests::roundtrip_insert_and_search`.
pub struct RedbUsearchStore {
    db: Arc<Database>,
    mem_index: Arc<Mutex<Option<Index>>>,
    code_index: Arc<Mutex<Option<Index>>>,
    ctx_index: Arc<Mutex<Option<Index>>>,
    brief_index: Arc<Mutex<Option<Index>>>,
    hist_index: Arc<Mutex<Option<Index>>>,
    mem_index_path: PathBuf,
    code_index_path: PathBuf,
    ctx_index_path: PathBuf,
    brief_index_path: PathBuf,
    hist_index_path: PathBuf,
    /// Vector dimension (same for every segment in this store). Stored so
    /// `warm_segment` can rebuild an evicted index without callers re-passing
    /// the dimension that was supplied at `open()` time.
    vector_dim: usize,
}

impl RedbUsearchStore {
    /// Open or create the store rooted at `store_dir`.
    ///
    /// Why: A single entrypoint that handles first-run (create files) and
    /// reopen (load existing files) so callers don't have to probe state.
    /// What: Creates the directory if missing, opens `store.redb`, and
    /// opens-or-creates two usearch indexes (`mem.usearch`, `code.usearch`)
    /// with the requested `vector_dim` and cosine similarity metric.
    /// Test: `tests::persists_across_reopen` covers the reopen path.
    pub fn open(store_dir: &Path, vector_dim: usize) -> Result<Self> {
        std::fs::create_dir_all(store_dir)
            .with_context(|| format!("creating store dir {}", store_dir.display()))?;

        let db_path = store_dir.join("store.redb");
        let db = Database::create(&db_path)
            .with_context(|| format!("opening redb at {}", db_path.display()))?;

        // Touch all tables so that subsequent read transactions don't error
        // out with TableDoesNotExist on a freshly-created database.
        {
            let write_txn = db.begin_write()?;
            {
                let _ = write_txn.open_table(PAYLOAD_TABLE)?;
                let _ = write_txn.open_table(MEM_LABEL_TO_ID)?;
                let _ = write_txn.open_table(MEM_ID_TO_LABEL)?;
                let _ = write_txn.open_table(CODE_LABEL_TO_ID)?;
                let _ = write_txn.open_table(CODE_ID_TO_LABEL)?;
                let _ = write_txn.open_table(CTX_LABEL_TO_ID)?;
                let _ = write_txn.open_table(CTX_ID_TO_LABEL)?;
                let _ = write_txn.open_table(BRIEF_LABEL_TO_ID)?;
                let _ = write_txn.open_table(BRIEF_ID_TO_LABEL)?;
                let _ = write_txn.open_table(HIST_LABEL_TO_ID)?;
                let _ = write_txn.open_table(HIST_ID_TO_LABEL)?;
                let _ = write_txn.open_table(COUNTER_TABLE)?;
            }
            write_txn.commit()?;
        }

        let mem_index_path = store_dir.join("mem.usearch");
        let code_index_path = store_dir.join("code.usearch");
        let ctx_index_path = store_dir.join("ctx.usearch");
        let brief_index_path = store_dir.join("brief.usearch");
        let hist_index_path = store_dir.join("hist.usearch");

        let mem_index = open_or_create_index(&mem_index_path, vector_dim)?;
        let code_index = open_or_create_index(&code_index_path, vector_dim)?;
        let ctx_index = open_or_create_index(&ctx_index_path, vector_dim)?;
        let brief_index = open_or_create_index(&brief_index_path, vector_dim)?;
        let hist_index = open_or_create_index(&hist_index_path, vector_dim)?;

        Ok(Self {
            db: Arc::new(db),
            mem_index: Arc::new(Mutex::new(Some(mem_index))),
            code_index: Arc::new(Mutex::new(Some(code_index))),
            ctx_index: Arc::new(Mutex::new(Some(ctx_index))),
            brief_index: Arc::new(Mutex::new(Some(brief_index))),
            hist_index: Arc::new(Mutex::new(Some(hist_index))),
            mem_index_path,
            code_index_path,
            ctx_index_path,
            brief_index_path,
            hist_index_path,
            vector_dim,
        })
    }

    /// Resolve a segment to its (label_to_id, id_to_label) table pair.
    /// Enumerate every record stored in `segment` along with its vector and
    /// payload.
    ///
    /// Why: Cross-machine memory export needs the full record (id + payload +
    /// embedding) so the receiving machine can re-insert without recomputing
    /// embeddings. The trait's search/get aren't enough — we need to walk
    /// every row in the segment regardless of similarity.
    /// What: Iterates the segment's `id_to_label` table, looks up each
    /// payload from the shared `PAYLOAD_TABLE`, and pulls the embedding from
    /// usearch via `Index::get`. Skips rows where any piece is missing
    /// (orphaned labels, in-flight tombstones).
    /// Test: Exercised indirectly by `memory_export_produces_jsonl_with_machine_id`.
    pub async fn list_segment(
        &self,
        segment: Segment,
    ) -> Result<Vec<(String, Vec<f32>, serde_json::Value)>> {
        let (_, id_to_label_def) = Self::label_tables(segment);
        let read_txn = self.db.begin_read()?;
        let id_to_label = read_txn.open_table(id_to_label_def)?;
        let payloads = read_txn.open_table(PAYLOAD_TABLE)?;

        let guard = self.ensure_loaded(segment).await?;
        let index = guard.as_ref().expect("ensure_loaded guarantees Some");
        let dim = index.dimensions();

        let mut out: Vec<(String, Vec<f32>, serde_json::Value)> = Vec::new();
        for entry in id_to_label.iter()? {
            let (id_v, label_v) = entry?;
            let id = id_v.value().to_string();
            let label = label_v.value();
            let key = format!("{}:{}", segment.prefix(), &id);
            let Some(payload_raw) = payloads.get(key.as_str())? else {
                continue;
            };
            let payload: serde_json::Value = serde_json::from_str(payload_raw.value())
                .context("deserializing stored payload JSON")?;
            // Pull the vector from usearch. `get` returns an Option<Vec<f32>>;
            // a missing vector means the record is mid-tombstone — skip.
            let mut buf = vec![0.0f32; dim];
            let n = index
                .get(label, &mut buf)
                .map_err(|e| anyhow!("usearch get: {e}"))?;
            if n == 0 {
                continue;
            }
            out.push((id, buf, payload));
        }
        Ok(out)
    }

    fn label_tables(
        segment: Segment,
    ) -> (
        TableDefinition<'static, u64, &'static str>,
        TableDefinition<'static, &'static str, u64>,
    ) {
        match segment {
            Segment::AgentMemory => (MEM_LABEL_TO_ID, MEM_ID_TO_LABEL),
            Segment::CodeIndex => (CODE_LABEL_TO_ID, CODE_ID_TO_LABEL),
            Segment::Context => (CTX_LABEL_TO_ID, CTX_ID_TO_LABEL),
            Segment::Brief => (BRIEF_LABEL_TO_ID, BRIEF_ID_TO_LABEL),
            Segment::History => (HIST_LABEL_TO_ID, HIST_ID_TO_LABEL),
        }
    }

    /// Return the in-memory index handle + on-disk path for a segment.
    fn index_for(&self, segment: Segment) -> (Arc<Mutex<Option<Index>>>, PathBuf) {
        match segment {
            Segment::AgentMemory => (self.mem_index.clone(), self.mem_index_path.clone()),
            Segment::CodeIndex => (self.code_index.clone(), self.code_index_path.clone()),
            Segment::Context => (self.ctx_index.clone(), self.ctx_index_path.clone()),
            Segment::Brief => (self.brief_index.clone(), self.brief_index_path.clone()),
            Segment::History => (self.hist_index.clone(), self.hist_index_path.clone()),
        }
    }

    /// Acquire the segment's index, lazily reloading from disk if it was
    /// evicted by a prior `evict_segment` call.
    ///
    /// Why: Eviction (#372) drops the in-memory HNSW to free RAM after
    /// inactivity; the very next op (insert/search/delete) must transparently
    /// reload it. Centralising the reload here keeps every mutation/read path
    /// identical and makes the warm-up boundary impossible to forget.
    /// What: Locks the segment mutex; if the inner option is `None`, calls
    /// `open_or_create_index` against the on-disk file, logs a single
    /// `info!("search index warmed up", segment=?...)` line, and stores the
    /// rebuilt index back into the option. Returns the locked guard ready
    /// to use. The mutex stays held by the caller (returned guard).
    /// Test: `evict_then_warm_returns_same_results`.
    async fn ensure_loaded<'a>(
        &'a self,
        segment: Segment,
    ) -> Result<tokio::sync::MutexGuard<'a, Option<Index>>> {
        let (index_arc, path) = self.index_for(segment);
        // We can't return a guard borrowed from `index_arc` (a clone of the
        // Arc) because the Arc itself is dropped at end of this fn. Switch to
        // borrowing the field directly via match.
        let _ = index_arc;
        let _ = path;
        // Borrow the field reference directly so the returned guard's
        // lifetime is tied to `&self`.
        let (mutex_ref, path_owned) = match segment {
            Segment::AgentMemory => (&self.mem_index, self.mem_index_path.clone()),
            Segment::CodeIndex => (&self.code_index, self.code_index_path.clone()),
            Segment::Context => (&self.ctx_index, self.ctx_index_path.clone()),
            Segment::Brief => (&self.brief_index, self.brief_index_path.clone()),
            Segment::History => (&self.hist_index, self.hist_index_path.clone()),
        };
        let mut guard = mutex_ref.lock().await;
        if guard.is_none() {
            let rebuilt = open_or_create_index(&path_owned, self.vector_dim)?;
            *guard = Some(rebuilt);
            tracing::info!(segment = ?segment, "search index warmed up");
        }
        Ok(guard)
    }
}

/// Construct or reload a usearch index file at `path`.
///
/// Why: Factored out so `open` stays readable and both segments share logic.
/// What: Builds `IndexOptions` with cosine + F32, creates the in-memory index,
/// reserves an initial capacity, then — if a serialized file exists — loads
/// the persisted state over top.
/// Test: `tests::persists_across_reopen` drives both branches.
fn open_or_create_index(path: &Path, vector_dim: usize) -> Result<Index> {
    let options = IndexOptions {
        dimensions: vector_dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        ..Default::default()
    };
    let index = Index::new(&options)
        .map_err(|e| anyhow!("creating usearch index at {}: {e}", path.display()))?;
    index
        .reserve(INITIAL_RESERVE)
        .map_err(|e| anyhow!("reserving initial usearch capacity: {e}"))?;

    if path.exists() {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("non-UTF8 usearch path: {}", path.display()))?;
        index
            .load(path_str)
            .map_err(|e| anyhow!("loading usearch index from {}: {e}", path.display()))?;
    }
    Ok(index)
}

/// Grow the usearch index if it has no headroom left for one more vector.
///
/// Why: usearch returns an error when `add` would exceed `capacity`; we
/// expand geometrically to amortize reallocation cost.
/// What: If `size + 1 > capacity`, reserves `max(size * GROWTH_FACTOR, INITIAL_RESERVE)`.
/// Test: Implicit in repeated inserts in `tests::roundtrip_insert_and_search`.
fn ensure_capacity(index: &Index) -> Result<()> {
    let size = index.size();
    let capacity = index.capacity();
    if size + 1 > capacity {
        let new_cap = std::cmp::max(size * GROWTH_FACTOR, INITIAL_RESERVE);
        index
            .reserve(new_cap)
            .map_err(|e| anyhow!("reserving usearch capacity {new_cap}: {e}"))?;
    }
    Ok(())
}

/// Persist a usearch index to its on-disk path.
fn save_index(index: &Index, path: &Path) -> Result<()> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF8 usearch path: {}", path.display()))?;
    index
        .save(path_str)
        .map_err(|e| anyhow!("saving usearch index to {}: {e}", path.display()))?;
    Ok(())
}

/// Allocate the next auto-incrementing label for `segment`. Caller owns the
/// write transaction so the allocation and usage commit atomically.
fn next_label(write_txn: &redb::WriteTransaction, segment: Segment) -> Result<u64> {
    let mut counters = write_txn.open_table(COUNTER_TABLE)?;
    let prefix = segment.prefix();
    let current = counters.get(prefix)?.map(|v| v.value()).unwrap_or(0);
    let next = current + 1;
    counters.insert(prefix, next)?;
    Ok(next)
}

#[async_trait]
impl MemoryStore for RedbUsearchStore {
    async fn insert(
        &self,
        segment: Segment,
        id: &str,
        vector: &[f32],
        payload: serde_json::Value,
    ) -> Result<()> {
        // 1. Persist payload + label mapping inside one redb txn.
        let payload_json =
            serde_json::to_string(&payload).context("serializing memory payload to JSON")?;
        let payload_key = format!("{}:{}", segment.prefix(), id);
        let (label_to_id_def, id_to_label_def) = Self::label_tables(segment);

        let label = {
            let write_txn = self.db.begin_write()?;
            let label = {
                // Reuse existing label if this id has been written before,
                // otherwise allocate a new one.
                let existing = {
                    let id_to_label = write_txn.open_table(id_to_label_def)?;
                    id_to_label.get(id)?.map(|v| v.value())
                };
                let label = match existing {
                    Some(l) => l,
                    None => next_label(&write_txn, segment)?,
                };

                {
                    let mut payloads = write_txn.open_table(PAYLOAD_TABLE)?;
                    payloads.insert(payload_key.as_str(), payload_json.as_str())?;
                }
                {
                    let mut label_to_id = write_txn.open_table(label_to_id_def)?;
                    label_to_id.insert(label, id)?;
                }
                {
                    let mut id_to_label = write_txn.open_table(id_to_label_def)?;
                    id_to_label.insert(id, label)?;
                }
                label
            };
            write_txn.commit()?;
            label
        };

        // 2. Add vector to usearch and flush to disk. Mutations happen under
        //    the mutex so concurrent inserts can't race on capacity growth.
        //    `ensure_loaded` transparently re-hydrates an evicted index so the
        //    file watcher path keeps writing through cool-down windows.
        let (_, index_path) = self.index_for(segment);
        let guard = self.ensure_loaded(segment).await?;
        let index = guard.as_ref().expect("ensure_loaded guarantees Some");
        ensure_capacity(index)?;
        // If the label is already present (re-insert of same id), remove
        // first so usearch doesn't end up with stale duplicates.
        if index.contains(label) {
            let _ = index
                .remove(label)
                .map_err(|e| anyhow!("removing stale usearch entry: {e}"))?;
        }
        index
            .add(label, vector)
            .map_err(|e| anyhow!("adding vector to usearch: {e}"))?;
        save_index(index, &index_path)?;

        Ok(())
    }

    async fn search(
        &self,
        segment: Segment,
        query_vec: &[f32],
        top_k: usize,
    ) -> Result<Vec<MemoryResult>> {
        let matches = {
            let guard = self.ensure_loaded(segment).await?;
            let index = guard.as_ref().expect("ensure_loaded guarantees Some");
            if index.size() == 0 {
                return Ok(Vec::new());
            }
            index
                .search(query_vec, top_k)
                .map_err(|e| anyhow!("usearch search: {e}"))?
        };

        let (label_to_id_def, _) = Self::label_tables(segment);
        let read_txn = self.db.begin_read()?;
        let label_to_id = read_txn.open_table(label_to_id_def)?;
        let payloads = read_txn.open_table(PAYLOAD_TABLE)?;

        let mut results = Vec::with_capacity(matches.keys.len());
        for (label, distance) in matches.keys.iter().zip(matches.distances.iter()) {
            let Some(id_val) = label_to_id.get(*label)? else {
                // Tombstoned or otherwise orphaned label — skip.
                continue;
            };
            let id = id_val.value().to_string();
            let key = format!("{}:{}", segment.prefix(), &id);
            let Some(payload_raw) = payloads.get(key.as_str())? else {
                continue;
            };
            let payload: serde_json::Value = serde_json::from_str(payload_raw.value())
                .context("deserializing stored payload JSON")?;
            results.push(MemoryResult {
                id,
                score: 1.0 - *distance,
                payload,
                segment: segment.prefix().to_string(),
            });
        }
        Ok(results)
    }

    async fn get(&self, segment: Segment, id: &str) -> Result<Option<serde_json::Value>> {
        let key = format!("{}:{}", segment.prefix(), id);
        let read_txn = self.db.begin_read()?;
        let payloads = read_txn.open_table(PAYLOAD_TABLE)?;
        let Some(raw) = payloads.get(key.as_str())? else {
            return Ok(None);
        };
        let value: serde_json::Value =
            serde_json::from_str(raw.value()).context("deserializing stored payload JSON")?;
        Ok(Some(value))
    }

    async fn list_segments(&self) -> Result<Vec<Segment>> {
        // Why: Iterate each segment's `id_to_label` redb table and report only
        // those with at least one row. We exclude `CodeIndex` from this scan
        // because callers (e.g., agent-memory tooling) treat the code-vector
        // namespace as a separate concern; querying it directly via search
        // remains supported.
        let candidates = [
            Segment::AgentMemory,
            Segment::Context,
            Segment::Brief,
            Segment::History,
        ];
        let read_txn = self.db.begin_read()?;
        let mut populated = Vec::new();
        for seg in candidates {
            let (_, id_to_label_def) = Self::label_tables(seg);
            let table = read_txn.open_table(id_to_label_def)?;
            if table.len()? > 0 {
                populated.push(seg);
            }
        }
        Ok(populated)
    }

    async fn move_segment(&self, id: &str, from: Segment, to: Segment) -> Result<()> {
        // Why: Reclassify a record (e.g., brief -> history) without losing
        // the original embedding. Reading payload + vector first means we
        // can avoid recomputing the embedding in the destination segment.
        if from == to {
            return Ok(());
        }

        // 1. Fetch payload from source.
        let payload = match self.get(from, id).await? {
            Some(p) => p,
            None => {
                return Err(anyhow!(
                    "move_segment: id {id:?} not found in source segment {:?}",
                    from
                ));
            }
        };

        // 2. Fetch vector from source's usearch index.
        let vector: Vec<f32> = {
            let guard = self.ensure_loaded(from).await?;
            let index = guard.as_ref().expect("ensure_loaded guarantees Some");
            let dim = index.dimensions();
            let (_, id_to_label_def) = Self::label_tables(from);
            let read_txn = self.db.begin_read()?;
            let id_to_label = read_txn.open_table(id_to_label_def)?;
            let label = id_to_label
                .get(id)?
                .map(|v| v.value())
                .ok_or_else(|| anyhow!("move_segment: missing label for id {id:?}"))?;
            let mut buf = vec![0.0f32; dim];
            let n = index
                .get(label, &mut buf)
                .map_err(|e| anyhow!("usearch get during move: {e}"))?;
            if n == 0 {
                return Err(anyhow!(
                    "move_segment: vector for id {id:?} missing in source segment"
                ));
            }
            buf
        };

        // 3. Insert into destination, then delete from source. Best-effort
        //    atomicity — if delete fails after insert, the record is briefly
        //    duplicated rather than lost.
        self.insert(to, id, &vector, payload).await?;
        self.delete(from, id).await?;
        Ok(())
    }

    async fn delete(&self, segment: Segment, id: &str) -> Result<()> {
        let (label_to_id_def, id_to_label_def) = Self::label_tables(segment);
        let key = format!("{}:{}", segment.prefix(), id);

        // 1. Look up label and drop all redb rows in one transaction.
        let label_opt = {
            let write_txn = self.db.begin_write()?;
            let label_opt = {
                let mut id_to_label = write_txn.open_table(id_to_label_def)?;
                let label = id_to_label.get(id)?.map(|v| v.value());
                if label.is_some() {
                    id_to_label.remove(id)?;
                }
                label
            };
            if let Some(label) = label_opt {
                let mut label_to_id = write_txn.open_table(label_to_id_def)?;
                label_to_id.remove(label)?;
            }
            {
                let mut payloads = write_txn.open_table(PAYLOAD_TABLE)?;
                payloads.remove(key.as_str())?;
            }
            write_txn.commit()?;
            label_opt
        };

        // 2. Remove the vector from usearch (tombstone) and persist.
        if let Some(label) = label_opt {
            let (_, index_path) = self.index_for(segment);
            let guard = self.ensure_loaded(segment).await?;
            let index = guard.as_ref().expect("ensure_loaded guarantees Some");
            if index.contains(label) {
                let _ = index
                    .remove(label)
                    .map_err(|e| anyhow!("removing usearch entry: {e}"))?;
            }
            save_index(index, &index_path)?;
        }

        Ok(())
    }

    async fn evict_segment(&self, segment: Segment) -> Result<()> {
        // Why: Drop the in-memory HNSW for `segment` to free RAM after a
        // search-inactivity window. Persistence is unaffected — the on-disk
        // `.usearch` file plus redb metadata stay intact and the next access
        // path (`ensure_loaded`) rehydrates from disk transparently.
        // What: Locks the segment's mutex and replaces `Some(Index)` with
        // `None`. Idempotent: a second call while already evicted is a no-op.
        // Test: `evict_then_warm_returns_same_results`.
        let mutex_ref = match segment {
            Segment::AgentMemory => &self.mem_index,
            Segment::CodeIndex => &self.code_index,
            Segment::Context => &self.ctx_index,
            Segment::Brief => &self.brief_index,
            Segment::History => &self.hist_index,
        };
        let mut guard = mutex_ref.lock().await;
        if guard.is_some() {
            *guard = None;
            tracing::info!(segment = ?segment, "search index evicted from memory");
        }
        Ok(())
    }

    async fn warm_segment(&self, segment: Segment) -> Result<()> {
        // Why: Public counterpart to `evict_segment`. Pre-loads an evicted
        // index so the next search query sees zero warm-up latency. Callers
        // that just want lazy warm-up can omit this — `search` calls
        // `ensure_loaded` itself — but explicit pre-warm is useful at PM
        // startup ("never cold-start under user load").
        // What: Calls `ensure_loaded` and drops the guard.
        // Test: `evict_then_warm_returns_same_results`.
        let _ = self.ensure_loaded(segment).await?;
        Ok(())
    }

    async fn is_segment_warm(&self, segment: Segment) -> Result<bool> {
        // Why: Tests assert eviction actually happened; production callers
        // can use this to skip redundant warm-up work.
        let mutex_ref = match segment {
            Segment::AgentMemory => &self.mem_index,
            Segment::CodeIndex => &self.code_index,
            Segment::Context => &self.ctx_index,
            Segment::Brief => &self.brief_index,
            Segment::History => &self.hist_index,
        };
        Ok(mutex_ref.lock().await.is_some())
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// Produce a simple 4-dim f32 vector from a tag so tests read clearly.
    fn vec4(a: f32, b: f32, c: f32, d: f32) -> Vec<f32> {
        vec![a, b, c, d]
    }

    #[tokio::test]
    async fn roundtrip_insert_and_search() {
        let dir = tempdir().unwrap();
        let store = RedbUsearchStore::open(dir.path(), 4).unwrap();

        // Three clearly-separated vectors.
        store
            .insert(
                Segment::AgentMemory,
                "a",
                &vec4(1.0, 0.0, 0.0, 0.0),
                json!({"tag": "a"}),
            )
            .await
            .unwrap();
        store
            .insert(
                Segment::AgentMemory,
                "b",
                &vec4(0.0, 1.0, 0.0, 0.0),
                json!({"tag": "b"}),
            )
            .await
            .unwrap();
        store
            .insert(
                Segment::AgentMemory,
                "c",
                &vec4(0.0, 0.0, 1.0, 0.0),
                json!({"tag": "c"}),
            )
            .await
            .unwrap();

        // Query close to "b".
        let results = store
            .search(Segment::AgentMemory, &vec4(0.0, 0.95, 0.05, 0.0), 3)
            .await
            .unwrap();

        assert!(!results.is_empty(), "expected at least one hit");
        assert_eq!(results[0].id, "b", "closest hit should be 'b'");
        assert_eq!(results[0].payload["tag"], "b");
        assert_eq!(results[0].segment, "mem");
    }

    #[tokio::test]
    async fn segments_are_isolated() {
        let dir = tempdir().unwrap();
        let store = RedbUsearchStore::open(dir.path(), 4).unwrap();

        // Same ids in both segments with distinguishable payloads.
        store
            .insert(
                Segment::AgentMemory,
                "shared",
                &vec4(1.0, 0.0, 0.0, 0.0),
                json!({"where": "mem"}),
            )
            .await
            .unwrap();
        store
            .insert(
                Segment::CodeIndex,
                "shared",
                &vec4(1.0, 0.0, 0.0, 0.0),
                json!({"where": "code"}),
            )
            .await
            .unwrap();

        let code_hits = store
            .search(Segment::CodeIndex, &vec4(1.0, 0.0, 0.0, 0.0), 5)
            .await
            .unwrap();
        assert_eq!(code_hits.len(), 1);
        assert_eq!(code_hits[0].segment, "code");
        assert_eq!(code_hits[0].payload["where"], "code");

        let mem_hits = store
            .search(Segment::AgentMemory, &vec4(1.0, 0.0, 0.0, 0.0), 5)
            .await
            .unwrap();
        assert_eq!(mem_hits.len(), 1);
        assert_eq!(mem_hits[0].segment, "mem");
        assert_eq!(mem_hits[0].payload["where"], "mem");
    }

    #[tokio::test]
    async fn get_returns_payload_for_known_id() {
        let dir = tempdir().unwrap();
        let store = RedbUsearchStore::open(dir.path(), 4).unwrap();

        store
            .insert(
                Segment::AgentMemory,
                "note-1",
                &vec4(0.1, 0.2, 0.3, 0.4),
                json!({"body": "hello"}),
            )
            .await
            .unwrap();

        let got = store.get(Segment::AgentMemory, "note-1").await.unwrap();
        assert_eq!(got, Some(json!({"body": "hello"})));

        let missing = store.get(Segment::AgentMemory, "nope").await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let store = RedbUsearchStore::open(&path, 4).unwrap();
            store
                .insert(
                    Segment::AgentMemory,
                    "persist",
                    &vec4(0.5, 0.5, 0.5, 0.5),
                    json!({"durable": true}),
                )
                .await
                .unwrap();
        } // store dropped here — files must be flushed

        let store2 = RedbUsearchStore::open(&path, 4).unwrap();
        let got = store2.get(Segment::AgentMemory, "persist").await.unwrap();
        assert_eq!(got, Some(json!({"durable": true})));

        // Vector search should also work against the reopened index.
        let hits = store2
            .search(Segment::AgentMemory, &vec4(0.5, 0.5, 0.5, 0.5), 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "persist");
    }

    #[tokio::test]
    async fn delete_removes_from_both_stores() {
        let dir = tempdir().unwrap();
        let store = RedbUsearchStore::open(dir.path(), 4).unwrap();

        store
            .insert(
                Segment::AgentMemory,
                "tmp",
                &vec4(1.0, 0.0, 0.0, 0.0),
                json!({"x": 1}),
            )
            .await
            .unwrap();

        store.delete(Segment::AgentMemory, "tmp").await.unwrap();

        let got = store.get(Segment::AgentMemory, "tmp").await.unwrap();
        assert!(got.is_none(), "payload should be gone after delete");

        let hits = store
            .search(Segment::AgentMemory, &vec4(1.0, 0.0, 0.0, 0.0), 5)
            .await
            .unwrap();
        assert!(
            hits.iter().all(|h| h.id != "tmp"),
            "deleted id should not appear in search results"
        );
    }

    #[tokio::test]
    async fn list_segments_returns_only_populated() {
        let dir = tempdir().unwrap();
        let store = RedbUsearchStore::open(dir.path(), 4).unwrap();

        // Empty store reports no populated segments.
        let empty = store.list_segments().await.unwrap();
        assert!(empty.is_empty(), "fresh store should have no segments");

        store
            .insert(
                Segment::Context,
                "ctx-1",
                &vec4(1.0, 0.0, 0.0, 0.0),
                json!({"k": "v"}),
            )
            .await
            .unwrap();
        store
            .insert(
                Segment::Brief,
                "brief-1",
                &vec4(0.0, 1.0, 0.0, 0.0),
                json!({"k": "v"}),
            )
            .await
            .unwrap();

        let segments = store.list_segments().await.unwrap();
        assert!(segments.contains(&Segment::Context));
        assert!(segments.contains(&Segment::Brief));
        assert!(
            !segments.contains(&Segment::History),
            "History was never written to"
        );
        assert!(
            !segments.contains(&Segment::AgentMemory),
            "AgentMemory was never written to"
        );
    }

    #[tokio::test]
    async fn move_segment_transfers_and_deletes() {
        let dir = tempdir().unwrap();
        let store = RedbUsearchStore::open(dir.path(), 4).unwrap();

        store
            .insert(
                Segment::AgentMemory,
                "rec-1",
                &vec4(0.25, 0.5, 0.75, 1.0),
                json!({"note": "to-history"}),
            )
            .await
            .unwrap();

        store
            .move_segment("rec-1", Segment::AgentMemory, Segment::History)
            .await
            .unwrap();

        // Now in History.
        let in_history = store.get(Segment::History, "rec-1").await.unwrap();
        assert_eq!(in_history, Some(json!({"note": "to-history"})));

        // Gone from AgentMemory.
        let in_mem = store.get(Segment::AgentMemory, "rec-1").await.unwrap();
        assert!(
            in_mem.is_none(),
            "record should be gone from source segment"
        );

        // Vector also moved — searching History should find it.
        let hits = store
            .search(Segment::History, &vec4(0.25, 0.5, 0.75, 1.0), 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "rec-1");
    }
}
