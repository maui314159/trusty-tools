//! Persistent chat-session store backed by SQLite (reusing the KG's WAL DB).
//!
//! Why: The web UI's chat panel wants to resume prior conversations after a
//! refresh / restart. Storing the history as a JSON blob keyed by a UUID
//! session id keeps the schema simple and lets us layer richer metadata
//! (title, last-message preview) later without migrations.
//! What: `ChatSessionStore` opens the same SQLite file used by the KG (one
//! database per palace) and adds a `chat_sessions` table on first use. The
//! `history` column is a JSON-encoded `Vec<ChatMessage>` so wire format and
//! storage format stay aligned.
//! Test: `create_then_get_session_round_trips`, `list_sessions_returns_meta`,
//! `delete_session_removes_row`, `upsert_session_overwrites_history`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::Uuid;

/// A single message in a chat session.
///
/// Why: Mirrors `trusty_common::ChatMessage` shape (role + content) without
/// taking a dep on it from the core crate — `serde_json` handles round-trip
/// translation at the API boundary.
/// What: Plain struct, JSON-serialised in the SQLite `history` blob.
/// Test: Exercised by every session round-trip test.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Wire-shape summary used by `list_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

/// Full session with history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub history: Vec<ChatMessage>,
}

/// Connection-pooled store for per-palace chat sessions.
pub struct ChatSessionStore {
    pool: Pool<SqliteConnectionManager>,
}

impl ChatSessionStore {
    /// Open (or create) a SQLite database at `path` and ensure the
    /// `chat_sessions` table exists.
    ///
    /// Why: Each palace gets one DB shared with the KG; this constructor is
    /// idempotent so it's safe to call on every cold start.
    /// What: Builds a small r2d2 pool, runs `CREATE TABLE IF NOT EXISTS`.
    /// Test: `create_then_get_session_round_trips` exercises open + insert.
    pub fn open(path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            .max_size(4)
            .build(manager)
            .context("failed to build chat-session sqlite pool")?;

        let conn = pool
            .get()
            .context("failed to get chat-session sqlite connection")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chat_sessions (
                id          TEXT PRIMARY KEY,
                title       TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                history     TEXT NOT NULL
            );",
        )
        .context("failed to create chat_sessions table")?;
        Ok(Self { pool })
    }

    /// Create an empty session and return its id.
    pub fn create_session(&self, title: Option<String>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let conn = self.pool.get().context("get conn")?;
        conn.execute(
            "INSERT INTO chat_sessions (id, title, created_at, updated_at, history)
             VALUES (?1, ?2, ?3, ?3, '[]')",
            rusqlite::params![id, title, now.to_rfc3339()],
        )
        .context("insert chat_sessions row")?;
        Ok(id)
    }

    /// List session metadata (no history) ordered by `updated_at DESC`.
    pub fn list_sessions(&self) -> Result<Vec<ChatSessionMeta>> {
        let conn = self.pool.get().context("get conn")?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, created_at, updated_at, history
                   FROM chat_sessions
                   ORDER BY updated_at DESC",
            )
            .context("prepare list_sessions")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .context("query list_sessions")?;
        let mut out = Vec::new();
        for r in rows {
            let (id, title, c, u, h) = r.context("read session row")?;
            let created_at = DateTime::parse_from_rfc3339(&c)
                .context("parse created_at")?
                .with_timezone(&Utc);
            let updated_at = DateTime::parse_from_rfc3339(&u)
                .context("parse updated_at")?
                .with_timezone(&Utc);
            let history: Vec<ChatMessage> = serde_json::from_str(&h).unwrap_or_default();
            out.push(ChatSessionMeta {
                id,
                title,
                created_at,
                updated_at,
                message_count: history.len(),
            });
        }
        Ok(out)
    }

    /// Fetch one session including its full history.
    pub fn get_session(&self, id: &str) -> Result<Option<ChatSession>> {
        let conn = self.pool.get().context("get conn")?;
        let mut stmt = conn
            .prepare(
                "SELECT id, title, created_at, updated_at, history
                   FROM chat_sessions WHERE id = ?1",
            )
            .context("prepare get_session")?;
        let mut rows = stmt
            .query(rusqlite::params![id])
            .context("query get_session")?;
        let Some(row) = rows.next().context("step get_session")? else {
            return Ok(None);
        };
        let id: String = row.get(0).context("read id")?;
        let title: Option<String> = row.get(1).context("read title")?;
        let c: String = row.get(2).context("read created_at")?;
        let u: String = row.get(3).context("read updated_at")?;
        let h: String = row.get(4).context("read history")?;
        let created_at = DateTime::parse_from_rfc3339(&c)?.with_timezone(&Utc);
        let updated_at = DateTime::parse_from_rfc3339(&u)?.with_timezone(&Utc);
        let history: Vec<ChatMessage> = serde_json::from_str(&h).unwrap_or_default();
        Ok(Some(ChatSession {
            id,
            title,
            created_at,
            updated_at,
            history,
        }))
    }

    /// Insert or update a session's history (and bump `updated_at`).
    pub fn upsert_session(&self, id: &str, history: &[ChatMessage]) -> Result<()> {
        let conn = self.pool.get().context("get conn")?;
        let now = Utc::now().to_rfc3339();
        let body = serde_json::to_string(history).context("serialize history")?;
        conn.execute(
            "INSERT INTO chat_sessions (id, title, created_at, updated_at, history)
             VALUES (?1, NULL, ?2, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
               updated_at = excluded.updated_at,
               history    = excluded.history",
            rusqlite::params![id, now, body],
        )
        .context("upsert chat_sessions row")?;
        Ok(())
    }

    /// Delete a session row. No-op if `id` is unknown.
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.pool.get().context("get conn")?;
        conn.execute(
            "DELETE FROM chat_sessions WHERE id = ?1",
            rusqlite::params![id],
        )
        .context("delete chat_sessions row")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open() -> (tempfile::TempDir, ChatSessionStore) {
        let dir = tempdir().unwrap();
        let store = ChatSessionStore::open(&dir.path().join("sessions.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn create_then_get_session_round_trips() {
        let (_d, store) = open();
        let id = store.create_session(Some("Hello".into())).unwrap();
        let s = store.get_session(&id).unwrap().expect("session exists");
        assert_eq!(s.id, id);
        assert_eq!(s.title.as_deref(), Some("Hello"));
        assert!(s.history.is_empty());
    }

    #[test]
    fn list_sessions_returns_meta() {
        let (_d, store) = open();
        let a = store.create_session(Some("A".into())).unwrap();
        let b = store.create_session(None).unwrap();
        store
            .upsert_session(
                &b,
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
            )
            .unwrap();
        let metas = store.list_sessions().unwrap();
        assert_eq!(metas.len(), 2);
        // Most-recently-updated first; b was upserted after a was created.
        assert_eq!(metas[0].id, b);
        assert_eq!(metas[0].message_count, 1);
        assert!(metas.iter().any(|m| m.id == a));
    }

    #[test]
    fn upsert_session_overwrites_history() {
        let (_d, store) = open();
        let id = store.create_session(None).unwrap();
        store
            .upsert_session(
                &id,
                &[ChatMessage {
                    role: "user".into(),
                    content: "first".into(),
                }],
            )
            .unwrap();
        store
            .upsert_session(
                &id,
                &[
                    ChatMessage {
                        role: "user".into(),
                        content: "first".into(),
                    },
                    ChatMessage {
                        role: "assistant".into(),
                        content: "second".into(),
                    },
                ],
            )
            .unwrap();
        let s = store.get_session(&id).unwrap().unwrap();
        assert_eq!(s.history.len(), 2);
        assert_eq!(s.history[1].content, "second");
    }

    #[test]
    fn delete_session_removes_row() {
        let (_d, store) = open();
        let id = store.create_session(None).unwrap();
        store.delete_session(&id).unwrap();
        assert!(store.get_session(&id).unwrap().is_none());
    }
}
