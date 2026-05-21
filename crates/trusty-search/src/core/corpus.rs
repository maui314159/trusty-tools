//! redb-backed durable chunk corpus (issue #28).
//!
//! Why: prior to this module the chunk corpus was persisted as a single
//! `chunks.json` file rewritten in full after every committed batch. On a
//! 200k-chunk corpus that JSON blob is ~400 MB; serializing it on every batch
//! commit (a reindex emits one commit per 128 files) caused the
//! memory-explosion documented in `PersistState` and forced a full re-read of
//! the entire file into a `HashMap` on every daemon restart. redb gives us:
//!   * crash-safe, atomic per-batch commits (no half-written file window),
//!   * O(batch) incremental writes instead of O(corpus) full rewrites,
//!   * the option to stream chunks back at startup without holding two copies
//!     (the JSON `Vec<RawChunk>` plus the live `HashMap`) in RAM at once.
//!
//! What: [`CorpusStore`] wraps a `redb::Database` with two tables — one keyed
//! by `chunk_id` holding the serialized [`RawChunk`], one keyed by file path
//! holding the serialized per-file [`RawEntity`] list. Values are serialized
//! with `serde_json` (already a workspace dependency; no new crate, and the
//! human-readable form keeps `redb` dumps debuggable).
//!
//! Test: see the `tests` submodule — `roundtrip` writes chunks + entities and
//! reads them back into a fresh store; `missing_db_is_empty` covers the
//! first-run / post-upgrade fallback; `delete_removes_chunk` covers eviction.

use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::core::chunker::RawChunk;
use crate::core::entity::RawEntity;

/// redb table holding the serialized chunk corpus, keyed by `chunk_id`.
///
/// Why: `chunk_id` (`"{path}:{start}:{end}"`) is the corpus's natural primary
/// key — it is collision-safe and is exactly what the in-memory `HashMap` is
/// keyed by, so a redb row maps 1:1 onto a `HashMap` entry.
/// What: `&str → &[u8]` where the value is `serde_json`-encoded [`RawChunk`].
const CHUNKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("chunks");

/// redb table holding the per-file entity lists, keyed by file path.
///
/// Why: `entities` are needed to rebuild the symbol graph on warm-boot and are
/// derived per file, so the file path is the natural key.
/// What: `&str → &[u8]` where the value is `serde_json`-encoded
/// `Vec<RawEntity>`.
const ENTITIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("entities");

/// Durable, redb-backed store for an index's chunk corpus + entity lists.
///
/// Why: see module docs — replaces the full-rewrite `chunks.json` snapshot
/// with an embedded transactional KV store so per-batch commits are O(batch)
/// and crash-safe.
/// What: owns a `redb::Database`; exposes batched upsert, full enumeration,
/// per-id/per-file deletion, and a count. Every mutating call is its own redb
/// write transaction, so a crash between calls never leaves a torn corpus.
/// Test: covered by the `tests` submodule.
pub struct CorpusStore {
    db: Database,
}

impl CorpusStore {
    /// Open (creating if absent) the redb database at `path`.
    ///
    /// Why: the daemon resolves one `index.redb` per index under its data dir;
    /// opening here is the single entry point so table-creation and the
    /// create-if-missing semantics live in one place.
    /// What: opens the database, then runs a no-op write transaction that
    /// `open_table`s both tables so they exist before any reader runs (redb
    /// requires a table to have been created in a committed write txn before
    /// it can be opened read-only).
    /// Test: `roundtrip` and `missing_db_is_empty` both exercise `open`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent of {}", path.display()))?;
        }
        let db = Database::create(path)
            .with_context(|| format!("open redb corpus at {}", path.display()))?;
        // Materialize both tables in a committed write txn so later read-only
        // transactions can `open_table` them even on a brand-new database.
        {
            let txn = db.begin_write().context("begin corpus init txn")?;
            {
                txn.open_table(CHUNKS_TABLE).context("init chunks table")?;
                txn.open_table(ENTITIES_TABLE)
                    .context("init entities table")?;
            }
            txn.commit().context("commit corpus init txn")?;
        }
        Ok(Self { db })
    }

    /// Upsert a batch of chunks in a single redb write transaction.
    ///
    /// Why: a batch commit (`commit_parsed_batch`) lands up to a few hundred
    /// chunks at once. One transaction per batch keeps the write amplification
    /// proportional to the batch size, not the whole corpus, and makes the
    /// batch atomic — a crash mid-commit rolls the whole batch back.
    /// What: serializes each [`RawChunk`] with `serde_json` and inserts it
    /// under its `id`. Existing ids are overwritten (upsert semantics).
    /// Test: `roundtrip` writes then reads; `delete_removes_chunk` re-upserts.
    pub fn upsert_chunks(&self, chunks: &[RawChunk]) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().context("begin chunk upsert txn")?;
        {
            let mut table = txn.open_table(CHUNKS_TABLE)?;
            for chunk in chunks {
                let bytes = serde_json::to_vec(chunk)
                    .with_context(|| format!("serialize chunk {}", chunk.id))?;
                table
                    .insert(chunk.id.as_str(), bytes.as_slice())
                    .with_context(|| format!("insert chunk {}", chunk.id))?;
            }
        }
        txn.commit().context("commit chunk upsert txn")?;
        Ok(())
    }

    /// Upsert a batch of per-file entity lists in a single write transaction.
    ///
    /// Why: entity lists are committed alongside chunks; sharing the same
    /// one-txn-per-batch discipline keeps both tables consistent on a crash.
    /// What: serializes each `Vec<RawEntity>` and inserts it under its file
    /// path key.
    /// Test: `roundtrip` exercises this alongside `upsert_chunks`.
    pub fn upsert_entities(&self, entities: &[(String, Vec<RawEntity>)]) -> Result<()> {
        if entities.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().context("begin entity upsert txn")?;
        {
            let mut table = txn.open_table(ENTITIES_TABLE)?;
            for (file, ents) in entities {
                let bytes = serde_json::to_vec(ents)
                    .with_context(|| format!("serialize entities for {file}"))?;
                table
                    .insert(file.as_str(), bytes.as_slice())
                    .with_context(|| format!("insert entities for {file}"))?;
            }
        }
        txn.commit().context("commit entity upsert txn")?;
        Ok(())
    }

    /// Delete a set of chunk ids in one write transaction.
    ///
    /// Why: `remove_file` / `remove_chunk` must evict from the durable store
    /// too, or a restart would resurrect deleted chunks.
    /// What: removes each id from `CHUNKS_TABLE`; unknown ids are a silent
    /// no-op (idempotent delete), matching the in-memory `HashMap::remove`.
    /// Test: `delete_removes_chunk`.
    pub fn delete_chunks(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write().context("begin chunk delete txn")?;
        {
            let mut table = txn.open_table(CHUNKS_TABLE)?;
            for id in ids {
                table
                    .remove(id.as_str())
                    .with_context(|| format!("delete chunk {id}"))?;
            }
        }
        txn.commit().context("commit chunk delete txn")?;
        Ok(())
    }

    /// Delete a per-file entity list. Idempotent.
    ///
    /// Why: `remove_file` drops the file's entities; the durable store must
    /// follow or the symbol graph would rebuild stale symbols on restart.
    /// What: removes the file key from `ENTITIES_TABLE`.
    /// Test: covered indirectly by `delete_removes_chunk` (same txn shape).
    pub fn delete_entities(&self, file: &str) -> Result<()> {
        let txn = self.db.begin_write().context("begin entity delete txn")?;
        {
            let mut table = txn.open_table(ENTITIES_TABLE)?;
            table
                .remove(file)
                .with_context(|| format!("delete entities for {file}"))?;
        }
        txn.commit().context("commit entity delete txn")?;
        Ok(())
    }

    /// Load every chunk in the corpus into a `Vec`.
    ///
    /// Why: the warm-boot path rehydrates the in-memory `HashMap` (and rebuilds
    /// BM25 + the symbol graph) from this. A streaming iterator would avoid the
    /// transient `Vec`, but the caller already needs an owned `RawChunk` per
    /// entry to insert into the map, so the `Vec` is not extra peak RAM beyond
    /// the map itself.
    /// What: opens a read transaction, walks `CHUNKS_TABLE`, and deserializes
    /// each value. A single corrupt row is skipped with a `warn` rather than
    /// failing the whole load — one bad chunk must not brick the daemon.
    /// Test: `roundtrip`.
    pub fn load_all_chunks(&self) -> Result<Vec<RawChunk>> {
        let txn = self.db.begin_read().context("begin chunk read txn")?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter().context("iterate chunks table")? {
            let (key, value) = entry.context("read chunk row")?;
            match serde_json::from_slice::<RawChunk>(value.value()) {
                Ok(chunk) => out.push(chunk),
                Err(e) => {
                    tracing::warn!("corpus: skipping corrupt chunk row '{}' ({e})", key.value())
                }
            }
        }
        Ok(out)
    }

    /// Load every per-file entity list.
    ///
    /// Why: counterpart of [`Self::load_all_chunks`] for the entities table;
    /// the warm-boot path needs both to rebuild the symbol graph.
    /// What: walks `ENTITIES_TABLE`, deserializing each `Vec<RawEntity>`. A
    /// corrupt row is skipped with a `warn`.
    /// Test: `roundtrip`.
    pub fn load_all_entities(&self) -> Result<Vec<(String, Vec<RawEntity>)>> {
        let txn = self.db.begin_read().context("begin entity read txn")?;
        let table = txn.open_table(ENTITIES_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter().context("iterate entities table")? {
            let (key, value) = entry.context("read entity row")?;
            let file = key.value().to_string();
            match serde_json::from_slice::<Vec<RawEntity>>(value.value()) {
                Ok(ents) => out.push((file, ents)),
                Err(e) => {
                    tracing::warn!("corpus: skipping corrupt entity row '{file}' ({e})")
                }
            }
        }
        Ok(out)
    }

    /// Number of chunks currently stored.
    ///
    /// Why: lets the warm-boot path log a count and lets callers cheaply check
    /// "is the durable corpus empty?" (first-run / post-upgrade case) without
    /// materializing every row.
    /// What: returns `CHUNKS_TABLE.len()`.
    /// Test: `roundtrip` asserts the count after upsert.
    pub fn chunk_count(&self) -> Result<usize> {
        let txn = self.db.begin_read().context("begin count txn")?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        Ok(table.len().context("count chunks")? as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::chunker::{ChunkType, RawChunk};

    /// Build a minimal `RawChunk` for tests.
    fn raw(id: &str, content: &str) -> RawChunk {
        RawChunk {
            id: id.to_string(),
            file: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            content: content.to_string(),
            function_name: None,
            language: Some("rust".to_string()),
            chunk_type: ChunkType::Code,
            calls: Vec::new(),
            inherits_from: Vec::new(),
            chunk_depth: 0,
            parent_chunk_id: None,
            child_chunk_ids: Vec::new(),
            nlp_keywords: Vec::new(),
            nlp_code_refs: Vec::new(),
            virtual_terms: Vec::new(),
        }
    }

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();

        let chunks = vec![raw("a:1:1", "fn a() {}"), raw("b:1:1", "fn b() {}")];
        store.upsert_chunks(&chunks).unwrap();
        store
            .upsert_entities(&[("src/lib.rs".to_string(), Vec::new())])
            .unwrap();
        assert_eq!(store.chunk_count().unwrap(), 2);

        // Reopen to simulate a daemon restart.
        drop(store);
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        let mut loaded = store.load_all_chunks().unwrap();
        loaded.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "a:1:1");
        assert_eq!(loaded[0].content, "fn a() {}");

        let entities = store.load_all_entities().unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].0, "src/lib.rs");
    }

    #[test]
    fn missing_db_is_empty() {
        // A brand-new database (post-upgrade / first-run) must open cleanly
        // and report an empty corpus rather than erroring.
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("fresh.redb")).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0);
        assert!(store.load_all_chunks().unwrap().is_empty());
        assert!(store.load_all_entities().unwrap().is_empty());
    }

    #[test]
    fn delete_removes_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        store
            .upsert_chunks(&[raw("a:1:1", "x"), raw("b:1:1", "y")])
            .unwrap();
        store.delete_chunks(&["a:1:1".to_string()]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
        let loaded = store.load_all_chunks().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "b:1:1");
        // Deleting an unknown id is a silent no-op.
        store.delete_chunks(&["nope:0:0".to_string()]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 1);
    }

    #[test]
    fn empty_batches_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let store = CorpusStore::open(&dir.path().join("index.redb")).unwrap();
        store.upsert_chunks(&[]).unwrap();
        store.upsert_entities(&[]).unwrap();
        store.delete_chunks(&[]).unwrap();
        assert_eq!(store.chunk_count().unwrap(), 0);
    }
}
