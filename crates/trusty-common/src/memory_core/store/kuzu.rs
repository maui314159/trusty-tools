//! KuzuDB integration: discover and read from kuzu-memory graph databases.
//!
//! Why: claude-mpm's kuzu-memory MCP plugin stores project facts in KuzuDB
//! graphs. Reading them lets a palace surface that knowledge without
//! re-ingesting it through the embedder.
//! What: `KuzuDatabase` descriptor + `KuzuSource` reader + `discover()` scanner.
//! The `kuzu` cargo feature swaps the file-based stub for real Cypher reads.
//! Test: `cargo test -p trusty-memory-core store::kuzu::` covers discovery and
//! the stub recall path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use tracing::warn;

/// Metadata about a discovered KuzuDB database on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KuzuDatabase {
    /// Absolute path to the database directory.
    pub path: PathBuf,
    /// Logical name (parent directory) used for display + palace mapping.
    pub name: String,
}

/// Read-only accessor for a single KuzuDB database.
///
/// Without the `kuzu` cargo feature this is a stub that discovers databases
/// by file presence and returns empty results from `query`/`recall`. With the
/// feature enabled (`--features kuzu`) the real bindings will be wired in;
/// see `TODO(kuzu)` markers below.
pub struct KuzuSource {
    /// Descriptor (path + name) for the opened database.
    pub db_info: KuzuDatabase,
    // TODO(kuzu): hold `kuzu::Database` + `kuzu::Connection` here when the
    //             real bindings are wired in.
}

impl KuzuSource {
    /// Discover KuzuDB databases under the given root paths.
    ///
    /// Why: kuzu-memory stores graphs at conventional locations
    /// (`~/.claude-mpm/memory/<project>/...`); scanning avoids manual config.
    /// What: For each root, treat each immediate subdirectory as a candidate.
    /// A directory is a kuzu DB if it (or its `kuzu/` subdir) contains
    /// `catalog.kz`, `data.kz`, or kuzu's `lock` file.
    /// Test: `discover_finds_kuzu_dir_with_catalog` and
    /// `discover_finds_nested_kuzu_subdir`.
    pub fn discover(roots: &[PathBuf]) -> Vec<KuzuDatabase> {
        let mut found = Vec::new();
        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if Self::looks_like_kuzu_db(&path) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    found.push(KuzuDatabase {
                        path: path.clone(),
                        name,
                    });
                    continue;
                }
                // Also check one level deeper, e.g. `<root>/<project>/kuzu/`.
                let kuzu_sub = path.join("kuzu");
                if Self::looks_like_kuzu_db(&kuzu_sub) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    found.push(KuzuDatabase {
                        path: kuzu_sub,
                        name,
                    });
                }
            }
        }
        found
    }

    /// Heuristic check that `path` is a kuzu database directory.
    fn looks_like_kuzu_db(path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        path.join("catalog.kz").exists()
            || path.join("data.kz").exists()
            || path.join("lock").exists()
    }

    /// Default discovery roots for kuzu-memory databases.
    ///
    /// Why: Centralizes the conventional locations claude-mpm uses so callers
    /// don't have to repeat them.
    /// What: Returns `~/.claude-mpm/memory`, `~/.open-mpm/memory`, and the
    /// equivalents under the current working directory.
    /// Test: Indirectly covered by integration tests; trivial otherwise.
    pub fn default_roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".claude-mpm").join("memory"));
            roots.push(home.join(".open-mpm").join("memory"));
        }
        if let Ok(cwd) = std::env::current_dir() {
            roots.push(cwd.join(".claude-mpm").join("memory"));
            roots.push(cwd.join(".open-mpm").join("memory"));
        }
        roots
    }

    /// Open a KuzuDB database at `path` in read-only mode.
    ///
    /// Why: Establishes a handle that callers can use for repeat queries
    /// without re-discovering the database each time.
    /// What: Builds a `KuzuDatabase` descriptor; the real kuzu connection is
    /// instantiated here once the `kuzu` feature is wired in.
    /// Test: `open_and_recall_stub_returns_empty`.
    pub fn open(path: PathBuf) -> Result<Self> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let db_info = KuzuDatabase {
            path: path.clone(),
            name,
        };

        #[cfg(feature = "memory-core-kuzu")]
        {
            // TODO(kuzu): replace stub with:
            //   let db = kuzu::Database::new(&path, kuzu::SystemConfig::default())?;
            //   let conn = kuzu::Connection::new(&db)?;
            // and store both on the struct.
            warn!(
                db = %path.display(),
                "kuzu feature enabled but bindings not yet wired; using stub"
            );
        }

        Ok(Self { db_info })
    }

    /// Execute a Cypher query and return rows as JSON maps.
    ///
    /// Why: Cypher is kuzu's native query language; exposing it lets callers
    /// run arbitrary graph queries against a kuzu-memory DB.
    /// What: Stub returns an empty vec with a warning; real impl will delegate
    /// to `kuzu::Connection::query`.
    /// Test: `open_and_recall_stub_returns_empty` exercises the stub path.
    pub fn query(&self, cypher: &str) -> Result<Vec<HashMap<String, Value>>> {
        warn!(
            db = %self.db_info.path.display(),
            query = %cypher,
            "kuzu query stub — no results"
        );
        // TODO(kuzu): execute via kuzu::Connection and serialize rows to
        //             HashMap<String, serde_json::Value>.
        Ok(Vec::new())
    }

    /// Free-text recall from the kuzu graph.
    ///
    /// Why: Surfaces kuzu-memory facts alongside HNSW vector results so
    /// retrieval can blend graph and dense recall.
    /// What: Builds a CONTAINS-style Cypher against `Memory` nodes; stub
    /// returns empty until the real bindings land.
    /// Test: `open_and_recall_stub_returns_empty`.
    pub fn recall(&self, query_text: &str, top_k: usize) -> Result<Vec<String>> {
        let _cypher = format!(
            "MATCH (m:Memory) WHERE m.content CONTAINS '{}' RETURN m.content LIMIT {}",
            query_text.replace('\'', "\\'"),
            top_k
        );
        warn!(
            db = %self.db_info.path.display(),
            query = %query_text,
            "kuzu recall stub — no results"
        );
        // TODO(kuzu): run `_cypher` and collect `m.content` strings.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn discover_finds_kuzu_dir_with_catalog() {
        let dir = tempdir().unwrap();
        let kuzu_db = dir.path().join("my-project");
        std::fs::create_dir_all(&kuzu_db).unwrap();
        std::fs::write(kuzu_db.join("catalog.kz"), b"").unwrap();

        let found = KuzuSource::discover(&[dir.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "my-project");
    }

    #[test]
    fn discover_finds_nested_kuzu_subdir() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("project-x");
        let kuzu_sub = project_dir.join("kuzu");
        std::fs::create_dir_all(&kuzu_sub).unwrap();
        std::fs::write(kuzu_sub.join("data.kz"), b"").unwrap();

        let found = KuzuSource::discover(&[dir.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "project-x");
    }

    #[test]
    fn discover_ignores_non_kuzu_dirs() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("random-dir")).unwrap();

        let found = KuzuSource::discover(&[dir.path().to_path_buf()]);
        assert!(found.is_empty());
    }

    #[test]
    fn discover_skips_missing_root() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let found = KuzuSource::discover(&[missing]);
        assert!(found.is_empty());
    }

    #[test]
    fn open_and_recall_stub_returns_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("catalog.kz"), b"").unwrap();
        let src = KuzuSource::open(dir.path().to_path_buf()).unwrap();
        let results = src.recall("anything", 10).unwrap();
        assert!(results.is_empty());
        let rows = src.query("MATCH (n) RETURN n LIMIT 1").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn default_roots_returns_some_paths() {
        let roots = KuzuSource::default_roots();
        assert!(!roots.is_empty());
    }
}
