//! L1 essential drawer cache snapshot persistence.
//!
//! Why: The L1 cache (top-15 drawers by importance) is computed from the full
//! drawer table on every palace open. For palaces with thousands of drawers,
//! recomputing this on cold start costs more than necessary; persisting it as
//! a JSON snapshot lets us hydrate the cache instantly and refresh it lazily.
//! What: `L1Cache` provides atomic save/load of `Vec<Drawer>` to
//! `<data_dir>/l1_cache.json` plus an `is_stale` mtime helper.
//! Test: `l1_cache_roundtrip` and `l1_cache_stale` in this module.

use crate::palace::Drawer;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use thiserror::Error;

/// Filename for the L1 cache snapshot.
const L1_CACHE_JSON: &str = "l1_cache.json";

/// Maximum number of drawers stored in the L1 snapshot (mirrors `retrieval::L1_CAP`).
pub const L1_SNAPSHOT_CAP: usize = 15;

/// Errors raised by L1 cache persistence operations.
#[derive(Debug, Error)]
pub enum L1CacheError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("json error at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("system time error: {0}")]
    Time(String),
}

impl L1CacheError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
    fn json(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        Self::Json {
            path: path.into(),
            source,
        }
    }
}

type Result<T> = std::result::Result<T, L1CacheError>;

/// Stateless namespace for L1 cache persistence helpers.
///
/// Why: Like `PalaceStore`, every operation is a pure function over a path.
/// What: `save_l1_cache` / `load_l1_cache` / `is_stale`.
/// Test: This module's tests cover roundtrip and staleness.
pub struct L1Cache;

impl L1Cache {
    /// Persist the top-by-importance drawers to `<data_dir>/l1_cache.json`.
    ///
    /// Why: Atomic JSON snapshot lets the next palace open hydrate L1 in O(1)
    /// disk reads instead of re-sorting the full drawer table.
    /// What: Sorts a clone of `drawers` by importance descending, takes the
    /// first `L1_SNAPSHOT_CAP`, and writes JSON via tmp+rename.
    /// Test: `l1_cache_roundtrip` saves 20 drawers and verifies only the top
    /// 15 (by importance) come back.
    pub fn save_l1_cache(drawers: &[Drawer], data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir).map_err(|e| L1CacheError::io(data_dir, e))?;
        let target = data_dir.join(L1_CACHE_JSON);
        let tmp = data_dir.join(format!("{L1_CACHE_JSON}.tmp"));

        let mut sorted: Vec<Drawer> = drawers.to_vec();
        sorted.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        sorted.truncate(L1_SNAPSHOT_CAP);

        let bytes = serde_json::to_vec_pretty(&sorted)
            .map_err(|e| L1CacheError::json(target.clone(), e))?;
        std::fs::write(&tmp, &bytes).map_err(|e| L1CacheError::io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &target).map_err(|e| L1CacheError::io(target.clone(), e))?;
        Ok(())
    }

    /// Load the L1 cache snapshot, returning an empty vec if missing.
    ///
    /// Why: A first-time palace open should not error just because the L1
    /// snapshot has never been written yet.
    /// What: Reads `<data_dir>/l1_cache.json` if present and deserializes.
    /// Returns `Ok(Vec::new())` on missing.
    /// Test: `l1_cache_roundtrip` covers populated; missing returns empty is
    /// exercised by `load_l1_cache_missing_is_empty`.
    pub fn load_l1_cache(data_dir: &Path) -> Result<Vec<Drawer>> {
        let target = data_dir.join(L1_CACHE_JSON);
        if !target.exists() {
            return Ok(Vec::new());
        }
        let bytes = std::fs::read(&target).map_err(|e| L1CacheError::io(target.clone(), e))?;
        let drawers: Vec<Drawer> =
            serde_json::from_slice(&bytes).map_err(|e| L1CacheError::json(target, e))?;
        Ok(drawers)
    }

    /// Check whether the cached snapshot is older than `max_age_secs`.
    ///
    /// Why: Long-lived daemons may want to lazily refresh the L1 snapshot if
    /// it has gotten too old relative to drawer-table mutations elsewhere.
    /// What: Compares the file's mtime to `SystemTime::now()`. Returns
    /// `Ok(true)` if missing (since "no snapshot" is maximally stale), `Ok(false)`
    /// if the snapshot's age is `<= max_age_secs`.
    /// Test: `l1_cache_stale` writes a snapshot, asserts not stale at 60s and
    /// stale at 0s.
    pub fn is_stale(data_dir: &Path, max_age_secs: u64) -> Result<bool> {
        let target = data_dir.join(L1_CACHE_JSON);
        if !target.exists() {
            return Ok(true);
        }
        let meta = std::fs::metadata(&target).map_err(|e| L1CacheError::io(target.clone(), e))?;
        let modified = meta
            .modified()
            .map_err(|e| L1CacheError::io(target.clone(), e))?;
        let age = SystemTime::now()
            .duration_since(modified)
            .map_err(|e| L1CacheError::Time(e.to_string()))?;
        // Compare in milliseconds so a 0-second tolerance treats any
        // already-on-disk snapshot as stale (its mtime predates `now()` by
        // at least the syscall round-trip).
        let max_age_ms = max_age_secs.saturating_mul(1000);
        Ok(age.as_millis() as u64 > max_age_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn drawer_with_importance(content: &str, importance: f32) -> Drawer {
        let mut d = Drawer::new(Uuid::new_v4(), content);
        d.importance = importance;
        d
    }

    #[test]
    fn l1_cache_roundtrip() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path();

        // Build 20 drawers with importance 0.0, 0.05, 0.10, ... 0.95.
        let drawers: Vec<Drawer> = (0..20)
            .map(|i| drawer_with_importance(&format!("drawer {i}"), i as f32 * 0.05))
            .collect();

        L1Cache::save_l1_cache(&drawers, data_dir).expect("save");
        let loaded = L1Cache::load_l1_cache(data_dir).expect("load");

        // Should be capped at 15, sorted by importance descending.
        assert_eq!(loaded.len(), L1_SNAPSHOT_CAP);
        for window in loaded.windows(2) {
            assert!(
                window[0].importance >= window[1].importance,
                "snapshot must be sorted descending by importance"
            );
        }
        // The top entry should be the 0.95-importance drawer.
        assert!((loaded[0].importance - 0.95).abs() < 1e-6);
    }

    #[test]
    fn load_l1_cache_missing_is_empty() {
        let tmp = tempdir().unwrap();
        let drawers = L1Cache::load_l1_cache(tmp.path()).unwrap();
        assert!(drawers.is_empty());
    }

    #[test]
    fn l1_cache_stale() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path();
        let drawers = vec![drawer_with_importance("only", 0.9)];
        L1Cache::save_l1_cache(&drawers, data_dir).unwrap();

        // Freshly written snapshot must not be stale at 60s tolerance.
        assert!(!L1Cache::is_stale(data_dir, 60).unwrap());
        // With 0s tolerance, anything older than the call itself is stale.
        // Sleep a hair to guarantee mtime < now.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(L1Cache::is_stale(data_dir, 0).unwrap());
    }

    #[test]
    fn is_stale_when_missing() {
        let tmp = tempdir().unwrap();
        // No snapshot ever written -> always stale.
        assert!(L1Cache::is_stale(tmp.path(), 999_999).unwrap());
    }
}
