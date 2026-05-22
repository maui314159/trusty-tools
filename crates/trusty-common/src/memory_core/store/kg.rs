//! Temporal knowledge graph backed by SQLite (WAL mode).
//!
//! Why: Some facts are relational and time-bounded ("Alice worked at Acme from
//! 2020 to 2023"). Vector search alone can't represent that; a triple store with
//! `valid_from`/`valid_to` columns can.
//! What: `Triple` record + `KnowledgeGraph` (rusqlite + r2d2 pool) implementation.
//! Test: Asserting (s,p,o) twice should close the first interval and open a
//! new one; `query_active` returns only the latest.

use crate::memory_core::palace::Drawer;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Confidence in [0.0, 1.0] from the asserter.
    pub confidence: f32,
    /// Free-form provenance string (drawer id, source URL, agent name, ...).
    pub provenance: Option<String>,
}

/// Schema (created on `open` if missing):
///
/// ```sql
/// CREATE TABLE IF NOT EXISTS entities (
///     id          TEXT PRIMARY KEY,
///     name        TEXT NOT NULL,
///     entity_type TEXT NOT NULL,
///     properties  TEXT  -- JSON
/// );
///
/// CREATE TABLE IF NOT EXISTS triples (
///     id          INTEGER PRIMARY KEY AUTOINCREMENT,
///     subject     TEXT NOT NULL,
///     predicate   TEXT NOT NULL,
///     object      TEXT NOT NULL,
///     valid_from  TEXT NOT NULL,  -- ISO-8601
///     valid_to    TEXT,           -- NULL = currently active
///     confidence  REAL NOT NULL,
///     provenance  TEXT
/// );
///
/// CREATE INDEX IF NOT EXISTS idx_triples_subj_active
///     ON triples(subject) WHERE valid_to IS NULL;
/// ```
pub struct KnowledgeGraph {
    pool: Pool<SqliteConnectionManager>,
}

impl KnowledgeGraph {
    /// Open (or create) a SQLite database at `path` in WAL mode.
    ///
    /// Why: WAL mode allows concurrent readers + a single writer, matching our
    /// many-readers / few-writers workload.
    /// What: Builds an r2d2 pool, sets `journal_mode=WAL`, runs migrations.
    /// Test: `open(temp)` succeeds and creates schema; second `open` on same
    /// path also succeeds (idempotent migrations).
    pub fn open(path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path);
        // Why: r2d2's default `connection_timeout` is 30s. When a palace's
        // `kg.db` is unopenable (corrupt, stale WAL sidecars, permissions),
        // `Pool::builder().build()` blocks for the full 30s before returning
        // an error — multiply that by N broken palaces at daemon startup and
        // the HTTP server takes tens of minutes to bind (issue: trusty-memory
        // stuck-startup on stale WAL). A 2-second timeout fails fast: healthy
        // palaces open in milliseconds and never come near the ceiling, while
        // broken palaces bail quickly so per-palace skipping in
        // `load_palaces_from_disk` can move on to the next one.
        let pool = Pool::builder()
            .max_size(8)
            .connection_timeout(std::time::Duration::from_secs(2))
            .build(manager)
            .context("failed to build sqlite connection pool")?;

        let conn = pool.get().context("failed to get sqlite connection")?;

        // Enable WAL mode. `pragma_update` doesn't return rows, so use query_row.
        conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .context("failed to enable WAL journal mode")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS entities (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                properties  TEXT
            );

            CREATE TABLE IF NOT EXISTS triples (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                subject     TEXT NOT NULL,
                predicate   TEXT NOT NULL,
                object      TEXT NOT NULL,
                valid_from  TEXT NOT NULL,
                valid_to    TEXT,
                confidence  REAL NOT NULL DEFAULT 1.0,
                provenance  TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_triples_subj_active
                ON triples(subject) WHERE valid_to IS NULL;

            CREATE TABLE IF NOT EXISTS drawers (
                id          TEXT PRIMARY KEY,
                room_id     TEXT NOT NULL,
                content     TEXT NOT NULL,
                importance  REAL NOT NULL DEFAULT 0.5,
                tags        TEXT NOT NULL DEFAULT '[]',
                source_file TEXT,
                created_at  TEXT NOT NULL
            );",
        )
        .context("failed to run schema migrations")?;

        Ok(Self { pool })
    }

    /// Assert a fact. If a contradicting active triple exists (same
    /// subject+predicate, `valid_to IS NULL`), close it by setting `valid_to`
    /// to this triple's `valid_from`, then insert this one as the new active
    /// fact.
    ///
    /// Why: Temporal model — facts have intervals. New assertion supersedes
    /// the prior active row instead of overwriting it, preserving history.
    /// What: Runs UPDATE-then-INSERT inside a single connection on a blocking
    /// task so the async reactor isn't blocked by sqlite I/O.
    /// Test: After two asserts of differing objects on same (s,p),
    /// `query_active` returns exactly one row with the latest object.
    pub async fn assert(&self, triple: Triple) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = pool.get().context("failed to get sqlite connection")?;
            let close_ts = triple.valid_from.to_rfc3339();

            // Single atomic transaction: closing the prior active interval and
            // inserting the new active row must either both succeed or both
            // fail, otherwise a crash between the two could leave two active
            // rows for the same (subject, predicate) — violating the invariant
            // `query_active` relies on.
            let tx = conn
                .unchecked_transaction()
                .context("failed to begin assert transaction")?;
            tx.execute(
                "UPDATE triples
                    SET valid_to = ?1
                    WHERE subject = ?2 AND predicate = ?3 AND valid_to IS NULL",
                rusqlite::params![close_ts, triple.subject, triple.predicate],
            )
            .context("failed to close prior active interval")?;

            tx.execute(
                "INSERT INTO triples
                    (subject, predicate, object, valid_from, confidence, provenance)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    triple.subject,
                    triple.predicate,
                    triple.object,
                    triple.valid_from.to_rfc3339(),
                    triple.confidence,
                    triple.provenance,
                ],
            )
            .context("failed to insert new active triple")?;

            tx.commit().context("failed to commit assert transaction")?;
            Ok(())
        })
        .await
        .context("assert spawn_blocking join error")??;
        Ok(())
    }

    /// Return all currently active triples (`valid_to IS NULL`) for a subject.
    ///
    /// Why: Most queries want "what is true *now*" — filtering on
    /// `valid_to IS NULL` uses the partial index `idx_triples_subj_active`.
    /// What: SELECT and map rows to `Triple`, parsing RFC3339 datetimes.
    /// Test: After asserting one triple, `query_active(subject)` returns it;
    /// for unknown subjects returns empty vec.
    pub async fn query_active(&self, subject: &str) -> Result<Vec<Triple>> {
        let pool = self.pool.clone();
        let subject = subject.to_string();
        let triples = tokio::task::spawn_blocking(move || -> Result<Vec<Triple>> {
            let conn = pool.get().context("failed to get sqlite connection")?;
            let mut stmt = conn
                .prepare(
                    "SELECT subject, predicate, object, valid_from, valid_to,
                            confidence, provenance
                       FROM triples
                       WHERE subject = ?1 AND valid_to IS NULL",
                )
                .context("failed to prepare query_active statement")?;

            let rows = stmt
                .query_map(rusqlite::params![subject], |row| {
                    let valid_from: String = row.get(3)?;
                    let valid_to: Option<String> = row.get(4)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        valid_from,
                        valid_to,
                        row.get::<_, f64>(5)? as f32,
                        row.get::<_, Option<String>>(6)?,
                    ))
                })
                .context("failed to query active triples")?;

            let mut out = Vec::new();
            for row in rows {
                let (subject, predicate, object, vf, vt, confidence, provenance) =
                    row.context("failed to read triple row")?;
                let valid_from = DateTime::parse_from_rfc3339(&vf)
                    .context("invalid valid_from datetime")?
                    .with_timezone(&Utc);
                let valid_to = match vt {
                    Some(s) => Some(
                        DateTime::parse_from_rfc3339(&s)
                            .context("invalid valid_to datetime")?
                            .with_timezone(&Utc),
                    ),
                    None => None,
                };
                out.push(Triple {
                    subject,
                    predicate,
                    object,
                    valid_from,
                    valid_to,
                    confidence,
                    provenance,
                });
            }
            Ok(out)
        })
        .await
        .context("query_active spawn_blocking join error")??;
        Ok(triples)
    }

    /// Count currently active triples (where `valid_to IS NULL`).
    ///
    /// Why: The admin dashboard needs a per-palace tally of "live" facts in
    /// the knowledge graph; querying the partial index keeps this O(active)
    /// rather than O(history).
    /// What: Synchronous `SELECT COUNT(*)` against the `triples` table where
    /// `valid_to IS NULL`. Returns 0 on any error rather than aborting the
    /// caller — this is a diagnostic counter, not a load-bearing read.
    /// Test: `count_active_triples_returns_live_only`.
    pub fn count_active_triples(&self) -> usize {
        let conn = match self.pool.get() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("count_active_triples: get conn failed: {e:#}");
                return 0;
            }
        };
        conn.query_row(
            "SELECT COUNT(*) FROM triples WHERE valid_to IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
    }

    /// Run a passive WAL checkpoint.
    ///
    /// Why: SQLite WAL grows unbounded unless checkpointed; PASSIVE mode is
    ///      non-blocking — it checkpoints whatever pages aren't actively read.
    /// What: Executes `PRAGMA wal_checkpoint(PASSIVE)` and returns the
    ///       (wal_pages, checkpointed_pages) tuple for logging.
    /// Test: `wal_checkpoint_returns_pages`.
    pub fn checkpoint(&self) -> Result<(i64, i64)> {
        let conn = self.pool.get().context("failed to get sqlite connection")?;
        let (wal, checkpointed) = conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
            })
            .context("failed to execute wal_checkpoint(PASSIVE)")?;
        Ok((wal, checkpointed))
    }

    /// Persist a drawer's metadata. Called from `PalaceHandle::remember`.
    ///
    /// Why: The HNSW index stores only vectors keyed by UUID prefix — without
    /// the metadata persisted alongside, vector hits map to nothing after a
    /// cold restart and retrieval silently drops every result beyond the L1
    /// snapshot (issue #32).
    /// What: INSERT OR REPLACE on the `drawers` table. Tags are JSON-encoded;
    /// `source_file` is stored as a string path; `created_at` is RFC3339.
    /// Test: `upsert_drawer_then_load_drawers_round_trips`.
    pub fn upsert_drawer(&self, drawer: &Drawer) -> Result<()> {
        let conn = self.pool.get().context("failed to get sqlite connection")?;
        let tags = serde_json::to_string(&drawer.tags).context("serialize drawer tags")?;
        let source_file = drawer
            .source_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        conn.execute(
            "INSERT INTO drawers
                (id, room_id, content, importance, tags, source_file, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
              ON CONFLICT(id) DO UPDATE SET
                room_id     = excluded.room_id,
                content     = excluded.content,
                importance  = excluded.importance,
                tags        = excluded.tags,
                source_file = excluded.source_file,
                created_at  = excluded.created_at",
            rusqlite::params![
                drawer.id.to_string(),
                drawer.room_id.to_string(),
                drawer.content,
                drawer.importance as f64,
                tags,
                source_file,
                drawer.created_at.to_rfc3339(),
            ],
        )
        .context("failed to upsert drawer")?;
        Ok(())
    }

    /// Remove a drawer's metadata by ID. Called from `PalaceHandle::forget`.
    ///
    /// Why: Forgetting must clear both the vector index and the persistent
    /// metadata row, otherwise restart would resurrect the drawer.
    /// What: DELETE FROM drawers WHERE id = ?1. No-op if id is unknown.
    /// Test: `delete_drawer_removes_row`.
    pub fn delete_drawer(&self, id: Uuid) -> Result<()> {
        let conn = self.pool.get().context("failed to get sqlite connection")?;
        conn.execute(
            "DELETE FROM drawers WHERE id = ?1",
            rusqlite::params![id.to_string()],
        )
        .context("failed to delete drawer")?;
        Ok(())
    }

    /// Load just the set of drawer IDs currently in the table.
    ///
    /// Why: Issue #49 compaction only needs to know "is this UUID a live
    /// drawer?", which is a fraction of the work of `load_drawers` (no JSON
    /// parsing, no RFC3339 parsing, no Vec<Drawer> allocation). The CLI
    /// `palace compact` path can be standalone (no PalaceHandle) and uses
    /// this to build the valid-id set in one SQL pass.
    /// What: `SELECT id FROM drawers`, parsing each into a `Uuid`. Rows with
    /// malformed IDs are skipped with a warning rather than aborting.
    /// Test: `load_drawer_ids_matches_load_drawers`.
    pub fn load_drawer_ids(&self) -> Result<std::collections::HashSet<Uuid>> {
        let conn = self.pool.get().context("failed to get sqlite connection")?;
        let mut stmt = conn
            .prepare("SELECT id FROM drawers")
            .context("failed to prepare load_drawer_ids statement")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .context("failed to query drawer ids")?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            let id_s = row.context("failed to read drawer id row")?;
            match Uuid::parse_str(&id_s) {
                Ok(u) => {
                    out.insert(u);
                }
                Err(e) => {
                    tracing::warn!(id = %id_s, "skip drawer with invalid id: {e}");
                }
            }
        }
        Ok(out)
    }

    /// Load all drawer metadata. Called from `PalaceHandle::open`.
    ///
    /// Why: Cold-start retrieval needs the full drawer table to map every
    /// HNSW vector hit back to metadata; without this, only the 15 drawers
    /// in the L1 snapshot are recoverable.
    /// What: SELECT * FROM drawers, parsing tags as JSON, source_file as
    /// optional path, created_at as RFC3339. Rows with malformed data are
    /// skipped with a warning rather than aborting the whole load.
    /// Test: `upsert_drawer_then_load_drawers_round_trips`.
    pub fn load_drawers(&self) -> Result<Vec<Drawer>> {
        let conn = self.pool.get().context("failed to get sqlite connection")?;
        let mut stmt = conn
            .prepare(
                "SELECT id, room_id, content, importance, tags, source_file, created_at
                   FROM drawers",
            )
            .context("failed to prepare load_drawers statement")?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .context("failed to query drawers")?;

        let mut out = Vec::new();
        for row in rows {
            let (id_s, room_id_s, content, importance, tags_s, source_file_s, created_s) =
                row.context("failed to read drawer row")?;
            let id = match Uuid::parse_str(&id_s) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(id = %id_s, "skip drawer with invalid id: {e}");
                    continue;
                }
            };
            let room_id = match Uuid::parse_str(&room_id_s) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(id = %id_s, "skip drawer with invalid room_id: {e}");
                    continue;
                }
            };
            let tags: Vec<String> = serde_json::from_str(&tags_s).unwrap_or_default();
            let source_file: Option<PathBuf> = source_file_s.map(PathBuf::from);
            let created_at = match DateTime::parse_from_rfc3339(&created_s) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(e) => {
                    tracing::warn!(id = %id_s, "skip drawer with invalid created_at: {e}");
                    continue;
                }
            };
            out.push(Drawer {
                id,
                room_id,
                content,
                importance: importance as f32,
                source_file,
                created_at,
                tags,
                last_accessed_at: None,
                access_count: 0,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn open_creates_schema() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        // If open succeeds, schema was created
        let result = kg.query_active("nonexistent").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn assert_then_query_active_returns_fact() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let triple = Triple {
            subject: "alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Acme Corp".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(triple).await.unwrap();
        let active = kg.query_active("alice").await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].object, "Acme Corp");
    }

    #[tokio::test]
    async fn second_assert_closes_prior_interval() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let t1 = Triple {
            subject: "alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Acme Corp".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(t1).await.unwrap();

        let t2 = Triple {
            subject: "alice".to_string(),
            predicate: "works_at".to_string(),
            object: "Beta Inc".to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(t2).await.unwrap();

        let active = kg.query_active("alice").await.unwrap();
        assert_eq!(active.len(), 1, "should have exactly 1 active triple");
        assert_eq!(active[0].object, "Beta Inc");
    }

    #[test]
    fn upsert_drawer_then_load_drawers_round_trips() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let room_id = Uuid::new_v4();
        let mut d = Drawer::new(room_id, "the cold-start drawer");
        d.importance = 0.83;
        d.tags = vec!["alpha".into(), "beta".into()];
        d.source_file = Some(PathBuf::from("/tmp/source.md"));
        kg.upsert_drawer(&d).unwrap();

        let loaded = kg.load_drawers().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, d.id);
        assert_eq!(loaded[0].room_id, room_id);
        assert_eq!(loaded[0].content, "the cold-start drawer");
        assert!((loaded[0].importance - 0.83).abs() < 1e-5);
        assert_eq!(loaded[0].tags, vec!["alpha".to_string(), "beta".into()]);
        assert_eq!(loaded[0].source_file, Some(PathBuf::from("/tmp/source.md")));
    }

    /// Why: Issue #49 — compaction needs a cheap "is this UUID a live drawer?"
    /// check; `load_drawer_ids` returns the set of all stored IDs without the
    /// overhead of materializing full `Drawer` rows.
    /// What: Insert two drawers, call `load_drawer_ids`, and assert the
    /// returned set matches the inserted IDs.
    /// Test: This test itself is the verification.
    #[test]
    fn load_drawer_ids_matches_load_drawers() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let room = Uuid::new_v4();
        let d1 = Drawer::new(room, "one");
        let d2 = Drawer::new(room, "two");
        kg.upsert_drawer(&d1).unwrap();
        kg.upsert_drawer(&d2).unwrap();

        let ids = kg.load_drawer_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&d1.id));
        assert!(ids.contains(&d2.id));
    }

    #[test]
    fn delete_drawer_removes_row() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let d = Drawer::new(Uuid::new_v4(), "to be deleted");
        kg.upsert_drawer(&d).unwrap();
        kg.delete_drawer(d.id).unwrap();
        let loaded = kg.load_drawers().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn upsert_drawer_replaces_existing_row() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        let mut d = Drawer::new(Uuid::new_v4(), "original");
        kg.upsert_drawer(&d).unwrap();
        d.content = "updated".into();
        d.importance = 0.95;
        kg.upsert_drawer(&d).unwrap();
        let loaded = kg.load_drawers().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "updated");
        assert!((loaded[0].importance - 0.95).abs() < 1e-5);
    }

    /// Why: The dashboard's KG triple count must reflect only live facts
    /// (`valid_to IS NULL`); closed intervals are history and must not be
    /// counted.
    /// What: Assert one triple, then supersede it with a contradicting one.
    /// The count should be 1 (the new active row), not 2.
    /// Test: This test itself.
    #[tokio::test]
    async fn count_active_triples_returns_live_only() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        assert_eq!(kg.count_active_triples(), 0);

        kg.assert(Triple {
            subject: "alice".into(),
            predicate: "works_at".into(),
            object: "Acme".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        assert_eq!(kg.count_active_triples(), 1);

        // Superseding triple closes the prior interval — count should stay at 1.
        kg.assert(Triple {
            subject: "alice".into(),
            predicate: "works_at".into(),
            object: "Beta".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        assert_eq!(kg.count_active_triples(), 1);
    }

    /// Why: The Dreamer cycle calls `checkpoint()` to keep the WAL bounded;
    /// the method must return a `(wal_pages, checkpointed_pages)` tuple without
    /// erroring on a freshly-opened database.
    /// What: Open a KG, write a triple to populate the WAL, then run a passive
    /// checkpoint. Both returned values must be non-negative.
    /// Test: This test itself.
    #[tokio::test]
    async fn wal_checkpoint_returns_pages() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::open(&dir.path().join("kg.db")).unwrap();
        kg.assert(Triple {
            subject: "s".into(),
            predicate: "p".into(),
            object: "o".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        })
        .await
        .unwrap();
        let (wal, done) = kg.checkpoint().expect("checkpoint should succeed");
        assert!(wal >= 0, "wal_pages must be non-negative, got {wal}");
        assert!(
            done >= 0,
            "checkpointed_pages must be non-negative, got {done}"
        );
    }

    #[tokio::test]
    async fn wal_mode_enabled() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("kg.db");
        let kg = KnowledgeGraph::open(&db_path).unwrap();
        // Write something to ensure the DB is fully initialized.
        let triple = Triple {
            subject: "s".into(),
            predicate: "p".into(),
            object: "o".into(),
            valid_from: Utc::now(),
            valid_to: None,
            confidence: 1.0,
            provenance: None,
        };
        kg.assert(triple).await.unwrap();
        // Verify the actual journal_mode pragma on a fresh connection so we're
        // not relying on filesystem sidecars (which may be cleaned up between
        // writes in some SQLite builds).
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "journal_mode should be wal, got {mode}"
        );
    }
}
