//! Shared helper that builds a `CodeIndexer`, attempting to restore a
//! previously-persisted HNSW snapshot and chunk corpus from disk.
//!
//! Why (issue #85): both `POST /indexes` and the daemon-startup
//! `restore_indexes` hook need the same logic — construct the indexer, wire
//! the embedder, attempt to load HNSW + chunks, and fall back to an empty
//! index on any failure. Centralising this prevents drift between the two
//! call sites (and the inevitable "the warm-boot path silently runs in
//! BM25-only mode" footgun).
//! What: `build_indexer_with_persisted_state` returns a fully-wired
//! `CodeIndexer`. On a corrupt or missing snapshot it falls back to a fresh
//! empty store + corpus and logs at WARN/INFO so operators can tell which
//! path was taken.
//! Test: covered by integration tests in `tests/integration_tests.rs` that
//! drop a state directory, restart, and assert the corpus is intact.

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::{
    corpus::CorpusStore,
    embed::Embedder,
    indexer::CodeIndexer,
    store::{UsearchStore, VectorStore},
};

use crate::service::persistence;

/// Build a `CodeIndexer` for `index_id`, restoring HNSW + chunks from disk
/// when a snapshot is present.
///
/// Why: see module docs.
/// What: tries `UsearchStore::load_from` first; falls back to a fresh empty
/// store if the load returns `Ok(None)` (no snapshot) or `Err` (corrupt
/// snapshot — logged at WARN). Then attaches the embedder + store, and
/// finally calls `load_chunks_from_disk` to rehydrate the corpus.
/// Test: see module docs.
pub async fn build_indexer_with_persisted_state(
    index_id: &str,
    root_path: PathBuf,
    embedder: &Arc<dyn Embedder>,
) -> CodeIndexer {
    let dim = embedder.dimension();
    let store: Arc<dyn VectorStore> = build_store(index_id, dim).await;
    let mut indexer =
        CodeIndexer::new(index_id, root_path).with_components(Arc::clone(embedder), store);

    // Issue #28: wire the durable redb corpus store before restoring chunks.
    // A failure to open the redb file is non-fatal — we log and run without a
    // corpus store (the index simply behaves as a pre-#28 in-memory daemon and
    // will be re-persisted to JSON via `spawn_incremental_persist`).
    match persistence::corpus_redb_path(index_id) {
        Ok(redb_path) => match CorpusStore::open(&redb_path) {
            Ok(corpus) => indexer.set_corpus_store(Arc::new(corpus)),
            Err(e) => tracing::warn!(
                "warm-boot: could not open redb corpus for '{index_id}' at {} ({e}) — \
                 running without durable corpus store",
                redb_path.display()
            ),
        },
        Err(e) => tracing::warn!("cannot resolve redb corpus path for '{index_id}': {e}"),
    }

    restore_corpus(&mut indexer, index_id).await;
    indexer
}

/// Restore the chunk corpus for `indexer`, preferring the redb store and
/// falling back to the legacy `chunks.json` snapshot (issue #28).
///
/// Why: redb is the new source of truth, but daemons upgraded in place have a
/// populated `chunks.json` and an empty `index.redb`. This function tries redb
/// first; on an empty redb corpus it reads the JSON snapshot and then seeds
/// redb from it so every subsequent restart uses the fast path.
/// What: `load_chunks_from_redb` → (if 0 chunks) `load_chunks_from_disk` +
/// `migrate_corpus_to_redb`. Either restore path rebuilds BM25 + the symbol
/// graph as a side effect.
/// Test: covered by the corpus roundtrip + migration integration tests.
async fn restore_corpus(indexer: &mut CodeIndexer, index_id: &str) {
    // Primary path: redb durable corpus.
    match indexer.load_chunks_from_redb().await {
        Ok(n) if n > 0 => {
            tracing::info!("warm-boot: restored {n} chunks for index '{index_id}' from redb");
            return;
        }
        Ok(_) => {} // empty redb — fall through to the JSON migration branch
        Err(e) => tracing::warn!(
            "warm-boot: redb corpus load failed for '{index_id}' ({e}) — \
             trying legacy chunks.json"
        ),
    }

    // Fallback / migration path: legacy chunks.json snapshot.
    match persistence::chunks_path(index_id) {
        Ok(path) => match indexer.load_chunks_from_disk(&path).await {
            Ok(n) if n > 0 => {
                tracing::info!(
                    "warm-boot: restored {n} chunks for index '{index_id}' from legacy {} — \
                     migrating to redb",
                    path.display()
                );
                // Seed redb so the next restart uses the fast path.
                indexer.migrate_corpus_to_redb().await;
            }
            Ok(_) => {} // empty / missing — genuine first-run case
            Err(e) => tracing::warn!(
                "warm-boot: could not load chunks for '{index_id}' ({e}) — starting empty"
            ),
        },
        Err(e) => tracing::warn!("cannot resolve chunks path for '{index_id}': {e}"),
    }
}

/// Try to load the HNSW snapshot for `index_id`. On any failure (missing,
/// corrupt, dimension mismatch) returns a fresh empty `UsearchStore`.
async fn build_store(index_id: &str, dim: usize) -> Arc<dyn VectorStore> {
    let path = match persistence::hnsw_path(index_id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("cannot resolve hnsw path for '{index_id}': {e}");
            return fresh_store(dim);
        }
    };

    if persistence::has_persisted_hnsw(&path) {
        match UsearchStore::load_from(&path).await {
            Ok(Some(store)) => {
                if store.dim() == dim {
                    tracing::info!(
                        "warm-boot: restored HNSW snapshot for '{}' from {}",
                        index_id,
                        path.display()
                    );
                    return Arc::new(store);
                }
                tracing::warn!(
                    "warm-boot: hnsw snapshot for '{}' has dim {} but embedder is {} — starting fresh",
                    index_id,
                    store.dim(),
                    dim
                );
            }
            Ok(None) => {
                // Sidecar missing/corrupt — fall back to fresh.
                tracing::warn!(
                    "warm-boot: hnsw snapshot at {} could not be loaded — starting fresh",
                    path.display()
                );
            }
            Err(e) => {
                tracing::warn!(
                    "warm-boot: error loading hnsw snapshot at {}: {e} — starting fresh",
                    path.display()
                );
            }
        }
    }
    fresh_store(dim)
}

fn fresh_store(dim: usize) -> Arc<dyn VectorStore> {
    // SAFETY (issue #101): `UsearchStore::new` only fails on OOM during the
    // initial HNSW index allocation. There is no meaningful recovery path —
    // the daemon needs an HNSW lane to function, and an OOM at startup would
    // have already torn the process down. We use `.expect` (not `panic!`) so
    // the failure message is uniform and the intent (infallible-modulo-OOM)
    // is documented for the reader.
    let s = UsearchStore::new(dim).unwrap_or_else(|e| {
        tracing::error!(
            "failed to allocate UsearchStore (dim={dim}): {e} — daemon cannot continue"
        );
        // Re-raise as a panic carrying the underlying error: there is no
        // sensible fallback (BM25-only stores are constructed via a different
        // path, not by replacing this Arc<dyn VectorStore>).
        panic!("usearch alloc failure (OOM during HNSW init, dim={dim}): {e}");
    });
    Arc::new(s) as Arc<dyn VectorStore>
}
