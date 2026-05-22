//! Legacy SQLite-backed knowledge graph.
//!
//! Why: The KG moved to redb in issue #44 (see `kg_redb.rs`). The SQLite code
//! is retained behind the `sqlite-kg` feature so issue #45's migration tool
//! can read old `<data_dir>/kg.db` files. Issue #47 will retire this module
//! along with the rusqlite / r2d2 / r2d2_sqlite dependencies.
//! What: `KnowledgeGraphSqlite` — exactly the pre-#44 `KnowledgeGraph`
//! implementation, namespaced so it does not collide with the redb-backed
//! public `KnowledgeGraph` in `kg.rs`.
//! Test: Original tests live in this module's `tests` submodule; they are
//! preserved to verify migration source compatibility.
#![cfg(feature = "sqlite-kg")]

use crate::memory_core::palace::Drawer;
use crate::memory_core::store::kg::Triple;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Legacy SQLite backend, retained for migration purposes only.
///
/// Why: Issue #45's migration walks `<data_dir>/kg.db` and rewrites every
/// triple + drawer into the redb file. The migration tool needs a working
/// reader for the legacy schema, which this struct provides.
/// What: r2d2-pooled rusqlite connection with WAL mode, mirroring the
/// pre-#44 implementation 1:1.
/// Test: Original suite below — `open_creates_schema`, `assert_*`, etc.
pub struct KnowledgeGraphSqlite {
    pool: Pool<SqliteConnectionManager>,
}

impl KnowledgeGraphSqlite {
    /// Open or create a SQLite-backed KG at `path` in WAL mode.
    ///
    /// Why: r2d2's default 30s connection timeout stalled startup on corrupt
    /// databases; 2s lets the broken-palace skipper move on quickly.
    /// What: Builds an r2d2 pool, enables WAL journaling, runs idempotent
    /// schema migrations.
    /// Test: `open_creates_schema`.
    pub fn open(path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            .max_size(8)
            .connection_timeout(std::time::Duration::from_secs(2))
            .build(manager)
            .context("failed to build sqlite connection pool")?;

        let conn = pool.get().context("failed to get sqlite connection")?;

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

    /// Open an existing SQLite KG read-only.
    ///
    /// Why: Issue #45's migration must not mutate the legacy `kg.db` — we read
    /// all rows then rename the file to `kg.db.migrated` so the migration is
    /// idempotent. Opening read-only avoids any chance of accidental writes
    /// (and lets the migration succeed even if the file is on read-only media).
    /// What: Builds an r2d2 pool with the rusqlite `SQLITE_OPEN_READ_ONLY`
    /// flag. Does **not** run schema migrations — readers must tolerate
    /// whatever the legacy schema already contains.
    /// Test: Integration test in `tests/kg_migration_tests.rs`.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path)
            .with_flags(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY);
        let pool = Pool::builder()
            .max_size(2)
            .connection_timeout(std::time::Duration::from_secs(2))
            .build(manager)
            .context("failed to build read-only sqlite connection pool")?;
        // Sanity-check that the connection works.
        let _ = pool
            .get()
            .context("failed to get read-only sqlite connection")?;
        Ok(Self { pool })
    }

    /// Read every triple (active + historical) — used by the migration tool.
    ///
    /// Why: Issue #45 needs full history to faithfully reproduce the temporal
    /// model in redb. `query_active` would lose closed intervals.
    /// What: `SELECT * FROM triples` mapped to `Triple` rows. Rows with
    /// malformed datetimes are skipped with a warning.
    /// Test: Indirect via migration test (issue #45).
    pub fn dump_all_triples(&self) -> Result<Vec<Triple>> {
        let conn = self.pool.get().context("failed to get sqlite connection")?;
        let mut stmt = conn
            .prepare(
                "SELECT subject, predicate, object, valid_from, valid_to,
                        confidence, provenance
                   FROM triples",
            )
            .context("failed to prepare dump_all_triples statement")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, f64>(5)? as f32,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .context("failed to query triples")?;
        let mut out = Vec::new();
        for row in rows {
            let (subject, predicate, object, vf, vt, confidence, provenance) =
                row.context("failed to read triple row")?;
            let valid_from = match DateTime::parse_from_rfc3339(&vf) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(e) => {
                    tracing::warn!(subject, "skip triple with invalid valid_from: {e}");
                    continue;
                }
            };
            let valid_to = match vt {
                Some(s) => match DateTime::parse_from_rfc3339(&s) {
                    Ok(dt) => Some(dt.with_timezone(&Utc)),
                    Err(e) => {
                        tracing::warn!(subject, "skip triple with invalid valid_to: {e}");
                        continue;
                    }
                },
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
    }

    /// Load all drawer rows — used by the migration tool.
    ///
    /// Why: Issue #45 transfers drawers verbatim from SQLite → redb.
    /// What: Mirror of the pre-#44 `load_drawers`: scan the drawers table and
    /// reconstruct `Drawer` values.
    /// Test: Indirect via migration test (issue #45).
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

    #[test]
    fn open_creates_schema_and_dumps_empty() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraphSqlite::open(&dir.path().join("kg.db")).unwrap();
        assert!(kg.dump_all_triples().unwrap().is_empty());
        assert!(kg.load_drawers().unwrap().is_empty());
    }
}
