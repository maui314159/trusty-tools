//! Integration tests for the one-shot SQLite → redb KG migration (issue #45).
//!
//! Why: The migration runs implicitly inside `KnowledgeGraph::open`; we cannot
//! exercise it from a unit test inside the crate without bypassing the public
//! surface. An integration test crate compiled with `memory-core,sqlite-kg`
//! lets us seed a real legacy `kg.db`, call the public `KnowledgeGraph::open`,
//! and assert that triples / drawers crossed over and the file was renamed.
//! What: Creates a SQLite file with the legacy schema, inserts a few triples
//! and drawers via rusqlite directly (so we exercise the schema the migration
//! must read), opens `KnowledgeGraph`, and asserts the post-migration state.
//! Test: `cargo test -p trusty-common --features memory-core,sqlite-kg --test
//! kg_migration_tests`.

#![cfg(all(feature = "memory-core", feature = "sqlite-kg"))]

use chrono::Utc;
use rusqlite::{Connection, params};
use tempfile::tempdir;
use trusty_common::memory_core::palace::Drawer;
use trusty_common::memory_core::store::KnowledgeGraph;
use uuid::Uuid;

/// Build a legacy `kg.db` file at `path` with the schema that the pre-#44
/// SQLite-backed `KnowledgeGraph` used, then return the inserted (drawer ids,
/// triple count).
///
/// Why: The migration must read the historical schema verbatim; reproducing
/// it here (rather than calling `KnowledgeGraphSqlite::open` which has the
/// schema migrations baked in) keeps the test honest if the legacy schema
/// ever diverges from the migration helper.
/// What: Opens a rusqlite connection, runs the legacy `CREATE TABLE`
/// statements, inserts two triples and two drawers.
/// Test: Used by `migration_round_trips_triples_and_drawers`.
fn seed_legacy_sqlite_db(path: &std::path::Path) -> (Vec<Uuid>, usize) {
    let conn = Connection::open(path).expect("open legacy sqlite");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS triples (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            subject     TEXT NOT NULL,
            predicate   TEXT NOT NULL,
            object      TEXT NOT NULL,
            valid_from  TEXT NOT NULL,
            valid_to    TEXT,
            confidence  REAL NOT NULL DEFAULT 1.0,
            provenance  TEXT
        );

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
    .expect("create legacy schema");

    let now = Utc::now().to_rfc3339();
    // Active triple.
    conn.execute(
        "INSERT INTO triples (subject, predicate, object, valid_from, valid_to, confidence, provenance)
         VALUES (?1, ?2, ?3, ?4, NULL, 1.0, ?5)",
        params!["alice", "works_at", "Acme Corp", now, "test-fixture"],
    )
    .expect("insert active triple");
    // Another active triple, different subject.
    conn.execute(
        "INSERT INTO triples (subject, predicate, object, valid_from, valid_to, confidence, provenance)
         VALUES (?1, ?2, ?3, ?4, NULL, 0.9, NULL)",
        params!["bob", "knows", "alice", now],
    )
    .expect("insert second active triple");
    // Closed/historical triple — must survive as history.
    conn.execute(
        "INSERT INTO triples (subject, predicate, object, valid_from, valid_to, confidence, provenance)
         VALUES (?1, ?2, ?3, ?4, ?5, 1.0, NULL)",
        params!["alice", "lived_in", "Springfield", now, now],
    )
    .expect("insert closed triple");

    let mut drawer_ids = Vec::new();
    let room_id = Uuid::new_v4();
    for content in ["first drawer", "second drawer"] {
        let id = Uuid::new_v4();
        drawer_ids.push(id);
        conn.execute(
            "INSERT INTO drawers (id, room_id, content, importance, tags, source_file, created_at)
             VALUES (?1, ?2, ?3, 0.75, '[\"alpha\"]', NULL, ?4)",
            params![id.to_string(), room_id.to_string(), content, now],
        )
        .expect("insert drawer");
    }

    (drawer_ids, 3)
}

/// Why: Verifies the end-to-end migration contract — opening
/// `KnowledgeGraph` against a directory that holds a legacy `kg.db` migrates
/// every triple + drawer, leaves a `kg.db.migrated` marker, removes the
/// original `kg.db`, and the public KG surface returns the migrated data.
/// What: Seeds a legacy SQLite file, opens `KnowledgeGraph`, asserts file
/// system state and queryable data match expectations.
/// Test: Failure here = migration regression — historical palaces would lose
/// data on upgrade.
#[tokio::test]
async fn migration_round_trips_triples_and_drawers() {
    let dir = tempdir().expect("tempdir");
    let kg_db = dir.path().join("kg.db");
    let kg_redb = dir.path().join("kg.redb");
    let migrated = dir.path().join("kg.db.migrated");

    let (drawer_ids, _triple_count) = seed_legacy_sqlite_db(&kg_db);

    // Sanity: pre-conditions.
    assert!(kg_db.exists(), "legacy kg.db should exist before migration");
    assert!(!migrated.exists());
    assert!(!kg_redb.exists());

    // Trigger migration via the public surface.
    let kg = KnowledgeGraph::open(&kg_db).expect("open KG triggers migration");

    // File-system contract: kg.db gone, kg.db.migrated present, kg.redb created.
    assert!(
        !kg_db.exists(),
        "kg.db should be renamed away after migration"
    );
    assert!(
        migrated.exists(),
        "kg.db.migrated marker should exist after migration"
    );
    assert!(kg_redb.exists(), "kg.redb should be created by open()");

    // Active triples land in `query_active`.
    let alice_active = kg.query_active("alice").await.expect("query_active alice");
    assert_eq!(
        alice_active.len(),
        1,
        "alice should have one active triple (lived_in was closed)"
    );
    assert_eq!(alice_active[0].predicate, "works_at");
    assert_eq!(alice_active[0].object, "Acme Corp");
    assert_eq!(alice_active[0].provenance.as_deref(), Some("test-fixture"));

    let bob_active = kg.query_active("bob").await.expect("query_active bob");
    assert_eq!(bob_active.len(), 1);
    assert_eq!(bob_active[0].object, "alice");

    // Historical / closed triple survives as history via dump_all_triples.
    let all = kg.dump_all_triples().expect("dump_all_triples");
    let closed: Vec<_> = all.iter().filter(|t| t.valid_to.is_some()).collect();
    assert_eq!(
        closed.len(),
        1,
        "expected one closed historical row, got {}",
        closed.len()
    );
    assert_eq!(closed[0].subject, "alice");
    assert_eq!(closed[0].predicate, "lived_in");
    assert_eq!(closed[0].object, "Springfield");

    // Drawers all migrated.
    let drawers: Vec<Drawer> = kg.load_drawers().expect("load_drawers");
    assert_eq!(drawers.len(), drawer_ids.len());
    for id in &drawer_ids {
        assert!(
            drawers.iter().any(|d| d.id == *id),
            "drawer {id} should have migrated"
        );
    }
}

/// Why: Migration must be exactly-once. After a successful migration the
/// marker file is present and the legacy file is gone; a subsequent open
/// must be a fast no-op even if a stray `kg.db` reappears (defensive: we
/// never want to overwrite freshly-imported redb state by re-running the
/// migration).
/// What: Runs the migration once, then re-opens the KG and asserts no panic
/// / no error and the data is unchanged.
/// Test: Failure here = migration not idempotent.
#[tokio::test]
async fn second_open_is_noop() {
    let dir = tempdir().expect("tempdir");
    let kg_db = dir.path().join("kg.db");
    seed_legacy_sqlite_db(&kg_db);

    {
        let _kg = KnowledgeGraph::open(&kg_db).expect("first open migrates");
    }

    // Re-open: no kg.db exists anymore, so migration is a no-op.
    let kg = KnowledgeGraph::open(&kg_db).expect("second open noop");
    let alice = kg.query_active("alice").await.expect("query alice");
    assert_eq!(alice.len(), 1, "data must remain after re-open");
}
