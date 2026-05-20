//! Hit/miss tracking for recall optimization.
//!
//! Why: Without feedback, importance scores are static. RecallLog closes the
//! loop — frequently recalled drawers are demonstrably more useful.
//! What: SQLite-backed event log with hit_count, miss_rate, top_drawers queries.
//! NLP: Query normalization via stop-word removal + FNV-1a hash. Zero inference.
//! Test: record then hit_count, miss_rate 1.0 when all miss, top_drawers ordering.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

// ── Query normalization (NLP — no inference) ─────────────────────────────────

/// English stop words removed during query normalization.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "in", "on", "at", "to", "of", "for", "with",
    "by", "from", "and", "or", "but", "not", "it", "this", "that", "be", "as", "do", "did", "has",
    "have", "had",
];

/// Normalize a query: lowercase, strip punctuation, collapse whitespace,
/// remove stop words.
///
/// Why: Two semantically equivalent queries should hash to the same bucket.
/// What: Pure string transformation — no embeddings, no inference.
/// Test: "The quick Brown Fox!" → "quick brown fox"
pub fn normalize_query(text: &str) -> String {
    let lower = text.to_lowercase();
    let no_punct: String = lower
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let words: Vec<&str> = no_punct
        .split_whitespace()
        .filter(|w| !STOP_WORDS.contains(w))
        .collect();
    words.join(" ")
}

/// FNV-1a 64-bit hash — deterministic, zero dependencies.
///
/// Why: We need a stable u64 key for query grouping without an extra crate.
/// What: Standard FNV-1a algorithm over UTF-8 bytes.
/// Test: same input yields same hash; different inputs differ.
pub fn fnv1a_hash(text: &str) -> u64 {
    const OFFSET: u64 = 14695981039346656037;
    const PRIME: u64 = 1099511628211;
    let mut hash = OFFSET;
    for byte in text.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Hash a query after normalization.
///
/// Why: Group semantically equivalent queries under one stable key.
/// What: normalize_query then fnv1a_hash.
/// Test: "The cat" and "a cat" both hash to fnv1a_hash("cat").
pub fn query_hash(text: &str) -> u64 {
    fnv1a_hash(&normalize_query(text))
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallEvent {
    pub palace_id: String,
    pub query_hash: u64,
    pub layer: u8,
    /// None = miss (query returned 0 results)
    pub drawer_id: Option<Uuid>,
    pub score: f32,
    pub occurred_at: DateTime<Utc>,
}

// ── RecallLog ────────────────────────────────────────────────────────────────

pub struct RecallLog {
    pool: Pool<SqliteConnectionManager>,
}

impl RecallLog {
    /// Open (or create) a recall log at `path`.
    ///
    /// Why: Sharing the palace directory keeps everything in one place.
    /// What: WAL-mode SQLite pool + schema migration.
    /// Test: open on tempdir then record + query roundtrips.
    pub fn open(path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            .max_size(4)
            .build(manager)
            .context("failed to build recall log pool")?;

        let conn = pool.get().context("failed to get connection")?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
            .context("failed to set WAL mode")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS recall_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                palace_id   TEXT    NOT NULL,
                query_hash  INTEGER NOT NULL,
                layer       INTEGER NOT NULL,
                drawer_id   TEXT,
                score       REAL    NOT NULL,
                occurred_at TEXT    NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_recall_drawer
                ON recall_events(drawer_id) WHERE drawer_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_recall_query
                ON recall_events(query_hash, occurred_at);",
        )
        .context("failed to create recall_events schema")?;

        Ok(Self { pool })
    }

    /// Record a recall event.
    ///
    /// Why: Persist hit/miss feedback to drive importance updates and gap detection.
    /// What: spawn_blocking insert into recall_events.
    /// Test: record then hit_count returns 1.
    pub async fn record(&self, event: RecallEvent) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = pool.get().context("failed to get connection")?;
            conn.execute(
                "INSERT INTO recall_events
                    (palace_id, query_hash, layer, drawer_id, score, occurred_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    event.palace_id,
                    event.query_hash as i64,
                    event.layer,
                    event.drawer_id.map(|id| id.to_string()),
                    event.score,
                    event.occurred_at.to_rfc3339(),
                ],
            )
            .context("failed to insert recall event")?;
            Ok(())
        })
        .await
        .context("record task join error")??;
        Ok(())
    }

    /// Total hit count for a specific drawer.
    ///
    /// Why: Frequently recalled drawers should bubble up in importance.
    /// What: COUNT(*) WHERE drawer_id = ?.
    /// Test: record twice for same drawer → hit_count == 2.
    pub async fn hit_count(&self, drawer_id: Uuid) -> Result<u64> {
        let pool = self.pool.clone();
        let id_str = drawer_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let conn = pool.get()?;
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM recall_events WHERE drawer_id = ?1",
                rusqlite::params![id_str],
                |r| r.get(0),
            )?;
            Ok(count as u64)
        })
        .await
        .context("hit_count task join error")?
    }

    /// Fraction of distinct queries in last `window_days` that returned 0 results.
    ///
    /// Why: High miss rate signals knowledge gaps in the palace.
    /// What: distinct miss queries / distinct total queries within window.
    /// Test: only-miss events → 1.0; only-hit events → 0.0.
    pub async fn miss_rate(&self, palace_id: &str, window_days: u32) -> Result<f32> {
        let pool = self.pool.clone();
        let palace_id = palace_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<f32> {
            let conn = pool.get()?;
            let since = (Utc::now() - chrono::Duration::days(window_days as i64)).to_rfc3339();
            let total: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT query_hash) FROM recall_events
                 WHERE palace_id = ?1 AND occurred_at >= ?2",
                rusqlite::params![palace_id, since],
                |r| r.get(0),
            )?;
            if total == 0 {
                return Ok(0.0);
            }
            let misses: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT query_hash) FROM recall_events
                 WHERE palace_id = ?1 AND occurred_at >= ?2 AND drawer_id IS NULL",
                rusqlite::params![palace_id, since],
                |r| r.get(0),
            )?;
            Ok(misses as f32 / total as f32)
        })
        .await
        .context("miss_rate task join error")?
    }

    /// Top drawers by hit count.
    ///
    /// Why: Identify the most-valuable drawers to promote in L1.
    /// What: GROUP BY drawer_id ORDER BY hits DESC LIMIT ?.
    /// Test: drawer with 3 hits ranks above drawer with 1 hit.
    pub async fn top_drawers(&self, palace_id: &str, limit: usize) -> Result<Vec<(Uuid, u64)>> {
        let pool = self.pool.clone();
        let palace_id = palace_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<(Uuid, u64)>> {
            let conn = pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT drawer_id, COUNT(*) as hits FROM recall_events
                 WHERE palace_id = ?1 AND drawer_id IS NOT NULL
                 GROUP BY drawer_id ORDER BY hits DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![palace_id, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id_str, count) = row?;
                if let Ok(id) = Uuid::parse_str(&id_str) {
                    out.push((id, count as u64));
                }
            }
            Ok(out)
        })
        .await
        .context("top_drawers task join error")?
    }

    /// Most-missed query hashes (queries that returned 0 results).
    ///
    /// Why: Surfaces knowledge gaps so users can fill them.
    /// What: GROUP BY query_hash WHERE drawer_id IS NULL ORDER BY count DESC.
    /// Test: query missed 3 times ranks above query missed 1 time.
    pub async fn missed_queries(&self, palace_id: &str, limit: usize) -> Result<Vec<(u64, u64)>> {
        let pool = self.pool.clone();
        let palace_id = palace_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<(u64, u64)>> {
            let conn = pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT query_hash, COUNT(*) as misses FROM recall_events
                 WHERE palace_id = ?1 AND drawer_id IS NULL
                 GROUP BY query_hash ORDER BY misses DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![palace_id, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (hash, count) = row?;
                out.push((hash as u64, count as u64));
            }
            Ok(out)
        })
        .await
        .context("missed_queries task join error")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn normalize_removes_stop_words() {
        assert_eq!(normalize_query("The quick Brown Fox!"), "quick brown fox");
    }

    #[test]
    fn normalize_strips_punctuation() {
        assert_eq!(normalize_query("  Rust  is  fast!  "), "rust fast");
    }

    #[test]
    fn fnv1a_is_deterministic() {
        assert_eq!(fnv1a_hash("hello"), fnv1a_hash("hello"));
        assert_ne!(fnv1a_hash("hello"), fnv1a_hash("world"));
    }

    #[tokio::test]
    async fn record_then_hit_count() {
        let dir = tempdir().unwrap();
        let log = RecallLog::open(&dir.path().join("recall.db")).unwrap();
        let id = Uuid::new_v4();
        log.record(RecallEvent {
            palace_id: "test".into(),
            query_hash: query_hash("vector store"),
            layer: 2,
            drawer_id: Some(id),
            score: 0.9,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();
        log.record(RecallEvent {
            palace_id: "test".into(),
            query_hash: query_hash("vector store"),
            layer: 2,
            drawer_id: Some(id),
            score: 0.85,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();
        assert_eq!(log.hit_count(id).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn miss_rate_all_miss() {
        let dir = tempdir().unwrap();
        let log = RecallLog::open(&dir.path().join("recall.db")).unwrap();
        log.record(RecallEvent {
            palace_id: "test".into(),
            query_hash: query_hash("unknown topic"),
            layer: 3,
            drawer_id: None,
            score: 0.0,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();
        let rate = log.miss_rate("test", 7).await.unwrap();
        assert!((rate - 1.0).abs() < 1e-4, "expected 1.0 got {rate}");
    }

    #[tokio::test]
    async fn miss_rate_all_hit() {
        let dir = tempdir().unwrap();
        let log = RecallLog::open(&dir.path().join("recall.db")).unwrap();
        log.record(RecallEvent {
            palace_id: "test".into(),
            query_hash: query_hash("rust"),
            layer: 2,
            drawer_id: Some(Uuid::new_v4()),
            score: 0.9,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();
        let rate = log.miss_rate("test", 7).await.unwrap();
        assert!((rate - 0.0).abs() < 1e-4, "expected 0.0 got {rate}");
    }

    #[tokio::test]
    async fn top_drawers_sorted_by_hits() {
        let dir = tempdir().unwrap();
        let log = RecallLog::open(&dir.path().join("recall.db")).unwrap();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        for _ in 0..3 {
            log.record(RecallEvent {
                palace_id: "test".into(),
                query_hash: 1,
                layer: 2,
                drawer_id: Some(id_a),
                score: 0.9,
                occurred_at: Utc::now(),
            })
            .await
            .unwrap();
        }
        log.record(RecallEvent {
            palace_id: "test".into(),
            query_hash: 2,
            layer: 2,
            drawer_id: Some(id_b),
            score: 0.8,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();
        let top = log.top_drawers("test", 5).await.unwrap();
        assert_eq!(top[0].0, id_a);
        assert_eq!(top[0].1, 3);
        assert_eq!(top[1].0, id_b);
    }

    #[tokio::test]
    async fn missed_queries_returns_most_missed_first() {
        let dir = tempdir().unwrap();
        let log = RecallLog::open(&dir.path().join("recall.db")).unwrap();
        let h1 = query_hash("obscure topic");
        let h2 = query_hash("another gap");
        for _ in 0..3 {
            log.record(RecallEvent {
                palace_id: "test".into(),
                query_hash: h1,
                layer: 3,
                drawer_id: None,
                score: 0.0,
                occurred_at: Utc::now(),
            })
            .await
            .unwrap();
        }
        log.record(RecallEvent {
            palace_id: "test".into(),
            query_hash: h2,
            layer: 3,
            drawer_id: None,
            score: 0.0,
            occurred_at: Utc::now(),
        })
        .await
        .unwrap();
        let missed = log.missed_queries("test", 5).await.unwrap();
        assert_eq!(missed[0].0, h1);
        assert_eq!(missed[0].1, 3);
    }
}
