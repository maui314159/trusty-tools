//! SQLite-backed payload sidecar for external integrations.
//!
//! Why: `TrustyBackedMemoryStore` in open-mpm maps caller-supplied string ids
//! onto trusty's `Uuid` keyspace and attaches an arbitrary JSON payload to each
//! entry. The vector data already persists to the usearch index on disk, but
//! the string-id ↔ uuid ↔ JSON mapping was process-local — losing it on
//! restart blocked switching `TrustyBackedMemoryStore` to the production
//! default (issue #52). This module provides the missing durable sidecar so
//! payloads survive a process restart without forcing every embedding adapter
//! to roll its own SQLite layer.
//! What: `PayloadStore` opens a single SQLite database at a caller-supplied
//! path and exposes `upsert` / `get` / `delete` / `lookup_id_for_uuid` /
//! `load_all` over a `(segment, id, uuid, payload_json)` table. Rows are
//! partitioned by `segment` so a single store can back multiple namespaces
//! (open-mpm's `Segment::AgentMemory`, `CodeIndex`, etc.). Errors flow through
//! the typed `PayloadStoreError` so callers can distinguish I/O from JSON from
//! schema problems.
//! Test: This module's `tests` exercise the full CRUD path plus a reopen
//! round-trip (the load-all method must return every row written by a prior
//! process).

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use serde_json::Value;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Errors raised by `PayloadStore`.
///
/// Why: Callers may want to fall back gracefully on a missing payload but
/// surface a hard I/O failure — distinguishing the two requires a typed error.
/// What: Wraps the three error sources (pool build, SQL, JSON) so each can be
/// inspected without `downcast`. `NotFound` is a value not an error path —
/// missing rows surface as `Ok(None)` instead.
/// Test: Covered indirectly by the round-trip test and the missing-row test.
#[derive(Debug, Error)]
pub enum PayloadStoreError {
    #[error("payload store pool error at {path}: {source}")]
    Pool {
        path: PathBuf,
        #[source]
        source: r2d2::Error,
    },
    #[error("payload store sqlite error at {path}: {source}")]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("payload store json error: {source}")]
    Json {
        #[source]
        source: serde_json::Error,
    },
}

impl PayloadStoreError {
    fn sqlite(path: &Path, source: rusqlite::Error) -> Self {
        Self::Sqlite {
            path: path.to_path_buf(),
            source,
        }
    }
}

type Result<T> = std::result::Result<T, PayloadStoreError>;

/// One persisted payload row.
///
/// Why: `load_all` needs a single struct shape so callers can hydrate their
/// in-memory sidecar in one pass.
/// What: Pairs the original caller id, the deterministic uuid the vector store
/// keys by, and the JSON payload.
/// Test: `roundtrip_persists_across_reopen` reads the row back through this
/// type.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadRow {
    pub segment: String,
    pub id: String,
    pub uuid: Uuid,
    pub payload: Value,
}

/// SQLite-backed sidecar for external string-id ↔ uuid ↔ JSON mappings.
///
/// Why: Provides the durable half of `TrustyBackedMemoryStore`'s in-memory
/// hashmap so adapter restarts don't lose payload data.
/// What: Owns an r2d2 pool over a single SQLite file in WAL mode with one
/// `payloads` table keyed by `(segment, id)`. Methods are all synchronous —
/// the call sites are already off the request-critical path (they wrap their
/// own async vector ops).
/// Test: `roundtrip_persists_across_reopen`, `get_missing_returns_none`,
/// `delete_drops_row`, `lookup_id_for_uuid_round_trips`, `load_all_filters_by_segment`.
pub struct PayloadStore {
    pool: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

impl PayloadStore {
    /// Open or create a payload store at `path`.
    ///
    /// Why: Single entry point so callers don't have to remember the schema
    /// version or WAL mode setting.
    /// What: Creates parent directories if missing, builds an r2d2 pool over a
    /// SQLite file at `path`, enables WAL mode, and idempotently creates the
    /// `payloads` table plus the `idx_payloads_uuid` lookup index.
    /// Test: `roundtrip_persists_across_reopen` opens the same path twice.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| PayloadStoreError::Sqlite {
                    path: path.to_path_buf(),
                    source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
                })?;
            }
        }
        let manager = SqliteConnectionManager::file(path);
        let pool =
            Pool::builder()
                .max_size(8)
                .build(manager)
                .map_err(|e| PayloadStoreError::Pool {
                    path: path.to_path_buf(),
                    source: e,
                })?;

        let conn = pool.get().map_err(|e| PayloadStoreError::Pool {
            path: path.to_path_buf(),
            source: e,
        })?;

        // WAL mode: concurrent readers, single writer. Matches kg.rs.
        conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .map_err(|e| PayloadStoreError::sqlite(path, e))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS payloads (
                segment   TEXT NOT NULL,
                id        TEXT NOT NULL,
                uuid      TEXT NOT NULL,
                payload   TEXT NOT NULL,
                PRIMARY KEY (segment, id)
            );
            CREATE INDEX IF NOT EXISTS idx_payloads_uuid
                ON payloads (segment, uuid);
            "#,
        )
        .map_err(|e| PayloadStoreError::sqlite(path, e))?;

        Ok(Self {
            pool,
            path: path.to_path_buf(),
        })
    }

    /// Insert or replace the row at `(segment, id)`.
    ///
    /// Why: Adapters write payloads on every `insert` call; idempotent upsert
    /// matches the trait semantics and lets retries be safe.
    /// What: `INSERT OR REPLACE` keyed by `(segment, id)`. Serializes `payload`
    /// to JSON. Stores `uuid` as a hyphenated string so it's grep-friendly.
    /// Test: `roundtrip_persists_across_reopen`.
    pub fn upsert(&self, segment: &str, id: &str, uuid: Uuid, payload: &Value) -> Result<()> {
        let payload_json =
            serde_json::to_string(payload).map_err(|e| PayloadStoreError::Json { source: e })?;
        let conn = self.pool.get().map_err(|e| PayloadStoreError::Pool {
            path: self.path.clone(),
            source: e,
        })?;
        conn.execute(
            "INSERT OR REPLACE INTO payloads (segment, id, uuid, payload) VALUES (?1, ?2, ?3, ?4)",
            params![segment, id, uuid.to_string(), payload_json],
        )
        .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        Ok(())
    }

    /// Fetch the payload for `(segment, id)`, if any.
    ///
    /// Why: `MemoryStore::get` expects `Ok(None)` on missing rows; a typed
    /// `Option` keeps callers from having to inspect error variants.
    /// What: Returns the `(uuid, payload)` pair on hit, `Ok(None)` on miss.
    /// Test: `get_missing_returns_none`.
    pub fn get(&self, segment: &str, id: &str) -> Result<Option<(Uuid, Value)>> {
        let conn = self.pool.get().map_err(|e| PayloadStoreError::Pool {
            path: self.path.clone(),
            source: e,
        })?;
        let mut stmt = conn
            .prepare("SELECT uuid, payload FROM payloads WHERE segment = ?1 AND id = ?2")
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        let mut rows = stmt
            .query(params![segment, id])
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        if let Some(row) = rows
            .next()
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?
        {
            let uuid_str: String = row
                .get(0)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            let payload_str: String = row
                .get(1)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            let uuid = Uuid::parse_str(&uuid_str).map_err(|e| PayloadStoreError::Sqlite {
                path: self.path.clone(),
                source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
            })?;
            let payload: Value = serde_json::from_str(&payload_str)
                .map_err(|e| PayloadStoreError::Json { source: e })?;
            Ok(Some((uuid, payload)))
        } else {
            Ok(None)
        }
    }

    /// Reverse-lookup the caller id for a uuid (used to translate vector hits
    /// back to the application-visible id).
    ///
    /// Why: `search` returns uuids from the vector store; the adapter needs to
    /// map each hit back to the original string id without scanning the whole
    /// table.
    /// What: Queries `idx_payloads_uuid` for an exact match.
    /// Test: `lookup_id_for_uuid_round_trips`.
    pub fn lookup_id_for_uuid(&self, segment: &str, uuid: Uuid) -> Result<Option<String>> {
        let conn = self.pool.get().map_err(|e| PayloadStoreError::Pool {
            path: self.path.clone(),
            source: e,
        })?;
        let mut stmt = conn
            .prepare("SELECT id FROM payloads WHERE segment = ?1 AND uuid = ?2")
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        let mut rows = stmt
            .query(params![segment, uuid.to_string()])
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        if let Some(row) = rows
            .next()
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            Ok(Some(id))
        } else {
            Ok(None)
        }
    }

    /// Delete the row at `(segment, id)`. No-op if the row does not exist.
    ///
    /// Why: Mirrors `MemoryStore::delete` which is also idempotent.
    /// What: `DELETE` keyed by `(segment, id)`.
    /// Test: `delete_drops_row`.
    pub fn delete(&self, segment: &str, id: &str) -> Result<()> {
        let conn = self.pool.get().map_err(|e| PayloadStoreError::Pool {
            path: self.path.clone(),
            source: e,
        })?;
        conn.execute(
            "DELETE FROM payloads WHERE segment = ?1 AND id = ?2",
            params![segment, id],
        )
        .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        Ok(())
    }

    /// Load every row, optionally restricted to `segment_filter`.
    ///
    /// Why: On startup, adapters rebuild their in-memory sidecar in one pass;
    /// `load_all` lets them do that without iterating per-id.
    /// What: Returns all rows when `segment_filter` is `None`, or just rows
    /// matching the filter otherwise.
    /// Test: `load_all_filters_by_segment` and `roundtrip_persists_across_reopen`.
    pub fn load_all(&self, segment_filter: Option<&str>) -> Result<Vec<PayloadRow>> {
        let conn = self.pool.get().map_err(|e| PayloadStoreError::Pool {
            path: self.path.clone(),
            source: e,
        })?;
        let (sql, bind_segment): (&str, Option<&str>) = match segment_filter {
            Some(seg) => (
                "SELECT segment, id, uuid, payload FROM payloads WHERE segment = ?1",
                Some(seg),
            ),
            None => ("SELECT segment, id, uuid, payload FROM payloads", None),
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
        let mut rows = match bind_segment {
            Some(seg) => stmt
                .query(params![seg])
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?,
            None => stmt
                .query([])
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?,
        };

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?
        {
            let segment: String = row
                .get(0)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            let id: String = row
                .get(1)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            let uuid_str: String = row
                .get(2)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            let payload_str: String = row
                .get(3)
                .map_err(|e| PayloadStoreError::sqlite(&self.path, e))?;
            let uuid = Uuid::parse_str(&uuid_str).map_err(|e| PayloadStoreError::Sqlite {
                path: self.path.clone(),
                source: rusqlite::Error::ToSqlConversionFailure(Box::new(e)),
            })?;
            let payload: Value = serde_json::from_str(&payload_str)
                .map_err(|e| PayloadStoreError::Json { source: e })?;
            out.push(PayloadRow {
                segment,
                id,
                uuid,
                payload,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_uuid(b: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[0] = b;
        Uuid::from_bytes(bytes)
    }

    #[test]
    fn roundtrip_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("payloads.db");
        let u = fixture_uuid(1);

        {
            let store = PayloadStore::open(&path).unwrap();
            store
                .upsert("seg-a", "rec-1", u, &json!({"hello": "world"}))
                .unwrap();
        }

        // Reopen — payload must survive.
        let store2 = PayloadStore::open(&path).unwrap();
        let got = store2.get("seg-a", "rec-1").unwrap();
        assert_eq!(got, Some((u, json!({"hello": "world"}))));

        let rows = store2.load_all(None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "rec-1");
        assert_eq!(rows[0].uuid, u);
    }

    #[test]
    fn get_missing_returns_none() {
        let dir = tempdir().unwrap();
        let store = PayloadStore::open(&dir.path().join("p.db")).unwrap();
        assert!(store.get("seg-a", "nope").unwrap().is_none());
    }

    #[test]
    fn delete_drops_row() {
        let dir = tempdir().unwrap();
        let store = PayloadStore::open(&dir.path().join("p.db")).unwrap();
        let u = fixture_uuid(2);
        store.upsert("seg-a", "k", u, &json!(42)).unwrap();
        store.delete("seg-a", "k").unwrap();
        assert!(store.get("seg-a", "k").unwrap().is_none());
        // Idempotent — second delete is fine.
        store.delete("seg-a", "k").unwrap();
    }

    #[test]
    fn lookup_id_for_uuid_round_trips() {
        let dir = tempdir().unwrap();
        let store = PayloadStore::open(&dir.path().join("p.db")).unwrap();
        let u = fixture_uuid(7);
        store.upsert("seg-a", "rec-7", u, &json!({"x": 1})).unwrap();
        let got = store.lookup_id_for_uuid("seg-a", u).unwrap();
        assert_eq!(got, Some("rec-7".to_string()));
        // Wrong segment must miss.
        assert!(store.lookup_id_for_uuid("seg-b", u).unwrap().is_none());
    }

    #[test]
    fn load_all_filters_by_segment() {
        let dir = tempdir().unwrap();
        let store = PayloadStore::open(&dir.path().join("p.db")).unwrap();
        store.upsert("a", "1", fixture_uuid(1), &json!(1)).unwrap();
        store.upsert("a", "2", fixture_uuid(2), &json!(2)).unwrap();
        store.upsert("b", "3", fixture_uuid(3), &json!(3)).unwrap();

        let only_a = store.load_all(Some("a")).unwrap();
        assert_eq!(only_a.len(), 2);
        assert!(only_a.iter().all(|r| r.segment == "a"));

        let all = store.load_all(None).unwrap();
        assert_eq!(all.len(), 3);
    }
}
