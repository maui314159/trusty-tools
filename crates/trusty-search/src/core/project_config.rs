//! Per-project configuration parsed from `<cwd>/.trusty-search.yaml`.
//!
//! Why: running `trusty-search index` from a repo root whose real source lives
//! in a subdirectory (`app/`), or that contains large non-code trees outside
//! `.gitignore` (`data/`, `docs/`), forces the user to repeat
//! `trusty-search index app --name myproject` every time and gives no way to
//! commit those settings so teammates (and daemon restarts) pick them up
//! automatically. A committed `.trusty-search.yaml` dotfile fixes that: it
//! supplies defaults for the index `name`, the sub`path` to index, and extra
//! `exclude` patterns. CLI flags always win over the file.
//!
//! What: [`ProjectConfig`] is a thin all-optional struct. [`ProjectConfig::load`]
//! reads `.trusty-search.yaml` from a directory, returning `Ok(None)` when the
//! file is simply absent (the common case) and `Err` only when the file exists
//! but is malformed — so callers can cleanly distinguish "no config, use
//! defaults" from "config present but broken, fail loudly".
//!
//! This is intentionally separate from [`super::repo_config::RepoConfig`]
//! (`trusty-search.yaml`, no leading dot), which declares *multiple* named
//! index slices for polyrepos. `.trusty-search.yaml` is the single-index
//! convenience config for the common one-project-one-index case.
//!
//! Test: see the `#[cfg(test)]` block — `test_load_absent`,
//! `test_load_name_only`, `test_load_full`, `test_load_malformed`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Dotfile name auto-detected in the current working directory by
/// `trusty-search index`.
pub const PROJECT_CONFIG_FILENAME: &str = ".trusty-search.yaml";

/// Per-project `index` defaults loaded from `.trusty-search.yaml`.
///
/// Why: every field is optional so a partial config (e.g. just `name:`) is
/// valid — missing fields fall back to the built-in `trusty-search index`
/// defaults, and any field can still be overridden by a CLI flag.
/// What: `name` overrides the directory-basename index name; `path` selects a
/// subdirectory to index (resolved relative to the config file's directory);
/// `exclude` supplies extra glob patterns layered on top of `.gitignore` and
/// the built-in skip list.
/// Test: round-tripped and field-checked in this module's `#[cfg(test)]` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectConfig {
    /// Index name. Overrides the directory-basename default when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Subdirectory to index, relative to the config file's directory.
    /// Absent → index the config file's directory itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,

    /// Extra glob exclude patterns layered on top of `.gitignore` and the
    /// built-in skip list. Absent → no extra excludes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
}

impl ProjectConfig {
    /// Load `<dir>/.trusty-search.yaml`.
    ///
    /// Why: the `index` CLI handler needs to distinguish three states cleanly:
    /// file absent (`Ok(None)` → use defaults), file present and valid
    /// (`Ok(Some(_))` → merge values), and file present but malformed
    /// (`Err(_)` → abort with a clear message rather than silently ignoring a
    /// typo'd config).
    /// What: `stat` → `read_to_string` → `serde_yml::from_str`. Missing file is
    /// not an error. Read and parse failures are surfaced as `anyhow::Error`
    /// with the offending path included for context.
    /// Test: `test_load_absent`, `test_load_name_only`, `test_load_full`,
    /// `test_load_malformed`.
    pub fn load(dir: &Path) -> anyhow::Result<Option<Self>> {
        let path = dir.join(PROJECT_CONFIG_FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        let cfg: Self = serde_yml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
        Ok(Some(cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Absent file is the common case and must be a non-error `None` so the
    /// caller falls back to built-in defaults.
    #[test]
    fn test_load_absent() {
        let tmp = tempdir().unwrap();
        let res = ProjectConfig::load(tmp.path()).unwrap();
        assert!(res.is_none(), "missing config file must return Ok(None)");
    }

    /// A config with only `name:` is valid; `path` and `exclude` stay `None`.
    #[test]
    fn test_load_name_only() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join(PROJECT_CONFIG_FILENAME), "name: foo\n").unwrap();
        let cfg = ProjectConfig::load(tmp.path())
            .unwrap()
            .expect("config present");
        assert_eq!(cfg.name.as_deref(), Some("foo"));
        assert!(cfg.path.is_none());
        assert!(cfg.exclude.is_none());
    }

    /// All three fields populated parse into the expected values.
    #[test]
    fn test_load_full() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join(PROJECT_CONFIG_FILENAME),
            r#"
name: cto
path: app
exclude:
  - data/
  - docs/
  - "*.db"
"#,
        )
        .unwrap();
        let cfg = ProjectConfig::load(tmp.path())
            .unwrap()
            .expect("config present");
        assert_eq!(cfg.name.as_deref(), Some("cto"));
        assert_eq!(cfg.path, Some(PathBuf::from("app")));
        assert_eq!(
            cfg.exclude,
            Some(vec![
                "data/".to_string(),
                "docs/".to_string(),
                "*.db".to_string(),
            ])
        );
    }

    /// Malformed YAML must return `Err`, never panic and never silently
    /// degrade to `None`.
    #[test]
    fn test_load_malformed() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join(PROJECT_CONFIG_FILENAME),
            "name: [unclosed\n  : :",
        )
        .unwrap();
        let res = ProjectConfig::load(tmp.path());
        assert!(res.is_err(), "malformed yaml must return Err, not panic");
    }
}
