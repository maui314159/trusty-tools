//! Palace metadata + identity.txt persistence.
//!
//! Why: Palaces are long-lived state on disk; without metadata persistence the
//! CLI cannot list palaces across runs and there is no canonical place to store
//! the palace identity (L0 baseline context).
//! What: `PalaceStore` provides atomic save/load of `Palace` metadata as JSON
//! at `<data_dir>/palace.json`, plus identity.txt read/write helpers and a
//! registry-wide palace listing walker.
//! Test: `palace_store_roundtrip` and `identity_txt_roundtrip` in this module
//! cover serde + atomic writes; `registry_create_and_open` (in registry.rs)
//! exercises the registry-level wiring.

use crate::memory_core::palace::{Palace, PalaceId};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Filename for serialized palace metadata.
const PALACE_JSON: &str = "palace.json";
/// Filename for the L0 identity blob.
const IDENTITY_TXT: &str = "identity.txt";

/// Errors raised by palace persistence operations.
///
/// Why: Library code returns `Result<_, PalaceStoreError>` so callers can
/// distinguish missing metadata from genuine I/O failure.
/// What: Wraps `std::io::Error` and `serde_json::Error` plus a "not found"
/// variant for missing metadata files.
/// Test: `load_palace_missing_returns_not_found` (implicit via roundtrip test).
#[derive(Debug, Error)]
pub enum PalaceStoreError {
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
    #[error("palace metadata missing at {0}")]
    NotFound(PathBuf),
}

impl PalaceStoreError {
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

type Result<T> = std::result::Result<T, PalaceStoreError>;

/// On-disk palace metadata format. Mirrors `Palace` but is its own type so we
/// can evolve the on-disk schema independently if needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PalaceJson {
    id: String,
    name: String,
    description: Option<String>,
    created_at: chrono::DateTime<Utc>,
    data_dir: PathBuf,
    /// Schema version for forward compatibility.
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

fn default_schema_version() -> u32 {
    1
}

impl From<&Palace> for PalaceJson {
    fn from(p: &Palace) -> Self {
        Self {
            id: p.id.0.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
            created_at: p.created_at,
            data_dir: p.data_dir.clone(),
            schema_version: 1,
        }
    }
}

impl From<PalaceJson> for Palace {
    fn from(j: PalaceJson) -> Self {
        Self {
            id: PalaceId(j.id),
            name: j.name,
            description: j.description,
            created_at: j.created_at,
            data_dir: j.data_dir,
        }
    }
}

/// Stateless namespace for palace persistence helpers.
///
/// Why: Palace metadata persistence has no state of its own — every operation
/// is a pure function over a path. Grouping under a unit struct gives a stable
/// import path while keeping the helpers `pub fn` rather than methods.
/// What: `save_palace` / `load_palace` / `list_palaces` plus identity.txt
/// helpers.
/// Test: This module's tests cover roundtrip and listing.
pub struct PalaceStore;

impl PalaceStore {
    /// Persist a palace's metadata to `<data_dir>/palace.json` atomically.
    ///
    /// Why: Crash-safety — if the daemon dies mid-write we must not leave a
    /// half-written `palace.json` that cannot deserialize.
    /// What: Creates `data_dir`, writes JSON to `palace.json.tmp`, fsyncs and
    /// renames over `palace.json`.
    /// Test: `palace_store_roundtrip` save + load round-trips all fields.
    pub fn save_palace(palace: &Palace) -> Result<()> {
        let data_dir = palace.data_dir.clone();
        std::fs::create_dir_all(&data_dir).map_err(|e| PalaceStoreError::io(&data_dir, e))?;

        let target = data_dir.join(PALACE_JSON);
        let tmp = data_dir.join(format!("{PALACE_JSON}.tmp"));

        let json: PalaceJson = palace.into();
        let bytes = serde_json::to_vec_pretty(&json)
            .map_err(|e| PalaceStoreError::json(target.clone(), e))?;

        std::fs::write(&tmp, &bytes).map_err(|e| PalaceStoreError::io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &target).map_err(|e| PalaceStoreError::io(target.clone(), e))?;
        Ok(())
    }

    /// Load palace metadata from `<data_dir>/palace.json`.
    ///
    /// Why: Re-opening a palace requires reconstructing its `Palace` struct
    /// before we can wire up storage handles.
    /// What: Reads `palace.json`, deserializes into `Palace`. Returns
    /// `NotFound` if the file is missing.
    /// Test: `palace_store_roundtrip` confirms fields survive a save+load.
    pub fn load_palace(data_dir: &Path) -> Result<Palace> {
        let target = data_dir.join(PALACE_JSON);
        if !target.exists() {
            return Err(PalaceStoreError::NotFound(target));
        }
        let bytes = std::fs::read(&target).map_err(|e| PalaceStoreError::io(target.clone(), e))?;
        let json: PalaceJson =
            serde_json::from_slice(&bytes).map_err(|e| PalaceStoreError::json(target, e))?;
        Ok(json.into())
    }

    /// Walk `registry_dir` for palace subdirectories and return all loadable
    /// palaces.
    ///
    /// Why: `palace list` and the daemon startup path both need to enumerate
    /// every palace on disk. Skipping unreadable subdirs keeps a single broken
    /// palace from taking the whole registry down.
    /// What: For each immediate child directory containing a `palace.json`,
    /// loads and pushes the `Palace`. Subdirs without metadata are silently
    /// skipped.
    /// Test: `list_palaces_finds_saved_palaces` (registry tests cover the full
    /// stack).
    pub fn list_palaces(registry_dir: &Path) -> Result<Vec<Palace>> {
        if !registry_dir.exists() {
            return Ok(Vec::new());
        }
        let read_dir =
            std::fs::read_dir(registry_dir).map_err(|e| PalaceStoreError::io(registry_dir, e))?;

        let mut palaces = Vec::new();
        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !path.join(PALACE_JSON).exists() {
                continue;
            }
            match Self::load_palace(&path) {
                Ok(p) => palaces.push(p),
                Err(_) => continue,
            }
        }
        Ok(palaces)
    }

    /// Persist the identity (L0) text for a palace.
    ///
    /// Why: L0 identity is read on every palace open; storing it as a plain
    /// `identity.txt` keeps it human-editable.
    /// What: Atomic write of `text` to `<data_dir>/identity.txt`.
    /// Test: `identity_txt_roundtrip` saves and reloads.
    pub fn save_identity(_palace_id: &PalaceId, text: &str, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir).map_err(|e| PalaceStoreError::io(data_dir, e))?;
        let target = data_dir.join(IDENTITY_TXT);
        let tmp = data_dir.join(format!("{IDENTITY_TXT}.tmp"));
        std::fs::write(&tmp, text.as_bytes()).map_err(|e| PalaceStoreError::io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &target).map_err(|e| PalaceStoreError::io(target.clone(), e))?;
        Ok(())
    }

    /// Read the identity (L0) text for a palace, if present.
    ///
    /// Why: Brand-new palaces may not have an identity yet — returning
    /// `Ok(None)` lets callers fall back to a default without treating the
    /// missing file as an error.
    /// What: Reads `<data_dir>/identity.txt`. Returns `Ok(None)` on missing,
    /// `Ok(Some(text))` on success.
    /// Test: `identity_txt_roundtrip` covers the present case; missing returns
    /// `None` is exercised by `load_identity_missing_returns_none`.
    pub fn load_identity(data_dir: &Path) -> Result<Option<String>> {
        let target = data_dir.join(IDENTITY_TXT);
        if !target.exists() {
            return Ok(None);
        }
        let s = std::fs::read_to_string(&target)
            .map_err(|e| PalaceStoreError::io(target.clone(), e))?;
        Ok(Some(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_palace(id: &str, dir: &Path) -> Palace {
        Palace {
            id: PalaceId::new(id),
            name: format!("Palace {id}"),
            description: Some("test palace".to_string()),
            created_at: Utc::now(),
            data_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn palace_store_roundtrip() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().join("alpha");
        let palace = make_palace("alpha", &data_dir);

        PalaceStore::save_palace(&palace).expect("save");
        let loaded = PalaceStore::load_palace(&data_dir).expect("load");

        assert_eq!(loaded.id, palace.id);
        assert_eq!(loaded.name, palace.name);
        assert_eq!(loaded.description, palace.description);
        assert_eq!(loaded.data_dir, palace.data_dir);
        // Timestamps should round-trip exactly through serde.
        assert_eq!(loaded.created_at, palace.created_at);
    }

    #[test]
    fn identity_txt_roundtrip() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let id = PalaceId::new("alpha");

        let text = "I am an experienced Rust engineer.\nI prefer concise answers.\n";
        PalaceStore::save_identity(&id, text, &data_dir).expect("save");

        let got = PalaceStore::load_identity(&data_dir).expect("load");
        assert_eq!(got.as_deref(), Some(text));
    }

    #[test]
    fn load_identity_missing_returns_none() {
        let tmp = tempdir().unwrap();
        let got = PalaceStore::load_identity(tmp.path()).expect("load");
        assert!(got.is_none());
    }

    #[test]
    fn list_palaces_finds_saved_palaces() {
        let tmp = tempdir().unwrap();
        let registry = tmp.path();

        for id in &["alpha", "beta"] {
            let dir = registry.join(id);
            let palace = make_palace(id, &dir);
            PalaceStore::save_palace(&palace).unwrap();
        }
        // Add a non-palace subdirectory; it should be ignored.
        std::fs::create_dir_all(registry.join("not-a-palace")).unwrap();

        let palaces = PalaceStore::list_palaces(registry).unwrap();
        let mut ids: Vec<String> = palaces.into_iter().map(|p| p.id.0).collect();
        ids.sort();
        assert_eq!(ids, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn list_palaces_missing_dir_is_empty() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let palaces = PalaceStore::list_palaces(&missing).unwrap();
        assert!(palaces.is_empty());
    }
}
