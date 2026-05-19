//! Global skills cache — scans multiple source directories and maintains a
//! content-addressed cache with a persistent JSON index.
//!
//! Why: Skills live in multiple places (project, user-local, skillset-mcp).
//! Scanning all sources on every startup is slow and defeats caching. This
//! module keeps `~/.open-mpm/skills/index.json` (metadata) and
//! `~/.open-mpm/skills/cache/<hash>` (content) so repeated runs avoid
//! redundant disk reads and skills from any source are instantly available.
//! What: `GlobalSkillsCache` holds cache/index paths; `refresh` scans all
//! source directories and updates both. `get_content` serves content from
//! the cache or falls back to a direct disk read. `discovery_paths` defines
//! the canonical source priority order.
//! Test: Construct with a temp home, write a `.md` file in one of the source
//! dirs, call `refresh`, assert the index contains the skill and `get_content`
//! returns its body.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;

/// Categorizes where a skill file originated.
///
/// Why: Downstream consumers may want to filter or display skills by origin
/// (e.g., prefer project-local over global), and serde round-trips cleanly
/// through the JSON index without losing that provenance.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// `config/skills/` inside the current project.
    Project,
    /// `~/.open-mpm/skills/files/`
    UserLocal,
    /// `~/Projects/skillset-mcp`
    SkillsetMcp,
}

/// Indexed metadata record for one skill file.
///
/// Why: Lets callers query the index for name/tags/hash without reading the
/// file content; the hash also drives cache lookups and change detection.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SkillMeta {
    /// Bare skill name (filename stem or frontmatter `name` field).
    pub name: String,
    /// Tags extracted from YAML frontmatter (may be empty).
    pub tags: Vec<String>,
    /// Where the skill was found.
    pub source: SkillSource,
    /// Absolute path to the `.md` file on disk.
    pub path: PathBuf,
    /// SHA-256 hex digest of the raw file content.
    pub content_hash: String,
    /// `mtime` as Unix seconds at index time (for change detection).
    pub last_modified: u64,
}

/// Global skills index + content cache stored under `~/.open-mpm/skills/`.
///
/// Why: Multiple source directories need to be merged and cached once so that
/// the per-invocation startup cost is a single JSON read rather than a full
/// directory scan. Atomic writes (tmp + rename) keep the index consistent
/// even if the process crashes mid-update.
pub struct GlobalSkillsCache {
    /// `~/.open-mpm/skills/cache/` — stores `<hash>` files.
    cache_dir: PathBuf,
    /// `~/.open-mpm/skills/index.json`
    index_path: PathBuf,
}

impl GlobalSkillsCache {
    /// Construct a cache rooted at `~/.open-mpm/skills/`.
    ///
    /// Why: Centralizes home-dir resolution so callers don't scatter it.
    /// What: Returns an error when `$HOME` is unset.
    /// Test: Call in a test with `HOME` set to a tempdir and assert no error.
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        let base = home.join(".open-mpm").join("skills");
        Ok(Self {
            cache_dir: base.join("cache"),
            index_path: base.join("index.json"),
        })
    }

    /// Create cache and index directories if they do not already exist.
    ///
    /// Why: The dirs are created lazily so they don't clutter home on
    /// machines that never use global skills.
    /// What: Creates `~/.open-mpm/skills/cache/` (and parents) via tokio.
    /// Test: Call twice on the same path and assert no error (idempotent).
    pub async fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.cache_dir).await?;
        Ok(())
    }

    /// Load the full index from disk, returning an empty map if absent.
    ///
    /// Why: Callers need the index without failing on first-run when no
    /// index file exists yet.
    /// What: Returns `Ok(HashMap::new())` on missing file; propagates other
    /// IO or parse errors.
    /// Test: Assert empty map returned when index_path does not exist.
    pub async fn load_index(&self) -> Result<HashMap<String, SkillMeta>> {
        match fs::read_to_string(&self.index_path).await {
            Ok(text) => {
                let map: HashMap<String, SkillMeta> = serde_json::from_str(&text)?;
                Ok(map)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically persist the index to disk.
    ///
    /// Why: A partial write followed by a crash would corrupt the index; a
    /// tmp-then-rename is atomic on POSIX filesystems.
    /// What: Writes to `<index_path>.tmp` then renames over the target.
    /// Test: Write an index, drop and reload — assert round-trip equality.
    pub async fn save_index(&self, index: &HashMap<String, SkillMeta>) -> Result<()> {
        self.ensure_dirs().await?;
        let tmp = self.index_path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(index)?;
        fs::write(&tmp, &text).await?;
        fs::rename(&tmp, &self.index_path).await?;
        Ok(())
    }

    /// Scan all source directories and rebuild the skill index.
    ///
    /// Why: Called once at startup to detect new or changed skill files
    /// before the workflow engine queries the index.
    /// What: Iterates `discovery_paths`, reads each `*.md` file, computes
    /// its SHA-256 hash, and upserts a `SkillMeta` entry. Saves the updated
    /// index atomically.
    /// Test: Write a `.md` file, call `refresh`, assert the entry appears
    /// in `load_index()`.
    pub async fn refresh(&self, project_dir: &Path) -> Result<()> {
        self.ensure_dirs().await?;
        let mut index = self.load_index().await.unwrap_or_default();

        for (dir, source) in discovery_paths(project_dir) {
            if !dir.exists() {
                continue;
            }
            let mut entries = match fs::read_dir(&dir).await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %e, "global_cache: cannot read dir");
                    continue;
                }
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let content = match fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "global_cache: skip unreadable");
                        continue;
                    }
                };
                let hash = hex_sha256(&content);
                let mtime = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let tags = extract_tags_from_content(&content);

                // Cache the content keyed by hash (no-op if already cached).
                if let Err(e) = self.cache_content(&hash, &content).await {
                    tracing::warn!(hash = %hash, error = %e, "global_cache: failed to cache content");
                }

                index.insert(
                    name.clone(),
                    SkillMeta {
                        name,
                        tags,
                        source: source.clone(),
                        path,
                        content_hash: hash,
                        last_modified: mtime,
                    },
                );
            }
        }

        self.save_index(&index).await?;
        tracing::info!(count = index.len(), "global skills cache refreshed");
        Ok(())
    }

    /// Return skill content: try the hash-keyed cache first, fall back to disk.
    ///
    /// Why: The cache avoids re-reading large skill files on every lookup;
    /// the disk fallback handles skills whose cache entry was evicted or
    /// whose content changed since the last refresh.
    /// What: Reads `cache_dir/<hash>` if it exists; otherwise reads `meta.path`
    /// directly and re-caches it.
    /// Test: Call after `refresh`; assert content matches the file on disk.
    #[cfg(test)]
    pub async fn get_content(&self, meta: &SkillMeta) -> Result<String> {
        let cache_file = self.cache_dir.join(&meta.content_hash);
        if let Ok(cached) = fs::read_to_string(&cache_file).await {
            return Ok(cached);
        }
        // Cache miss — read from source and re-cache.
        let content = fs::read_to_string(&meta.path).await?;
        if let Err(e) = self.cache_content(&meta.content_hash, &content).await {
            tracing::warn!(error = %e, "global_cache: failed to re-cache content");
        }
        Ok(content)
    }

    /// Write `content` to `cache_dir/<hash>` (no-op if file already exists).
    ///
    /// Why: Avoids redundant writes for unchanged skill files.
    /// What: Checks existence before writing; skips if the file is already there.
    /// Test: Write twice with the same hash; assert file is written once.
    async fn cache_content(&self, hash: &str, content: &str) -> Result<()> {
        let dest = self.cache_dir.join(hash);
        if dest.exists() {
            return Ok(());
        }
        let tmp = self.cache_dir.join(format!("{hash}.tmp"));
        fs::write(&tmp, content).await?;
        fs::rename(&tmp, &dest).await?;
        Ok(())
    }
}

/// Return the SHA-256 hex digest of `content`.
fn hex_sha256(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Extract tags from YAML frontmatter in the skill file content.
///
/// Why: Duplicating the full frontmatter parser here would create coupling;
/// this minimal extractor is sufficient for tag indexing.
/// What: Looks for a line matching `tags: [...]` inside the first `---` fence.
/// Test: Pass content with `tags: [rust, async]` and assert ["rust","async"].
fn extract_tags_from_content(content: &str) -> Vec<String> {
    let rest = match content.strip_prefix("---") {
        Some(r) => r.strip_prefix('\n').or_else(|| r.strip_prefix("\r\n")),
        None => return Vec::new(),
    };
    let rest = match rest {
        Some(r) => r,
        None => return Vec::new(),
    };
    let fm_end = match rest.find("\n---") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let fm = &rest[..fm_end];
    for line in fm.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("tags:") {
            let val = val.trim();
            if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                return inner
                    .split(',')
                    .map(|t| t.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
        }
    }
    Vec::new()
}

/// Return the ordered list of `(directory, source)` pairs to scan.
///
/// Why: The priority order (project > user-local > skillset-mcp) ensures that
/// project-specific overrides shadow global defaults without manual config.
/// What: Returns up to three paths; callers should iterate in order and use
/// the first match for a given skill name.
/// Test: Assert the returned slice contains `project_dir/.open-mpm/skills` first.
pub fn discovery_paths(project_dir: &Path) -> Vec<(PathBuf, SkillSource)> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        (project_dir.join(".open-mpm/skills"), SkillSource::Project),
        (home.join(".open-mpm/skills/files"), SkillSource::UserLocal),
        (home.join("Projects/skillset-mcp"), SkillSource::SkillsetMcp),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Build a `GlobalSkillsCache` rooted at a tempdir instead of `$HOME`.
    fn cache_in(base: &Path) -> GlobalSkillsCache {
        let skills = base.join(".open-mpm").join("skills");
        GlobalSkillsCache {
            cache_dir: skills.join("cache"),
            index_path: skills.join("index.json"),
        }
    }

    /// Write a minimal skill `.md` file with optional YAML frontmatter.
    async fn write_skill(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).await.unwrap();
        fs::write(dir.join(format!("{name}.md")), body)
            .await
            .unwrap();
    }

    // INTENT: Verify constructor succeeds when HOME is set.
    #[tokio::test]
    async fn test_new_succeeds() {
        // `dirs::home_dir()` reads real HOME; just verify no panic.
        let result = GlobalSkillsCache::new();
        assert!(result.is_ok(), "new() should succeed when HOME is set");
    }

    // INTENT: Verify ensure_dirs is idempotent and creates the cache directory.
    #[tokio::test]
    async fn test_ensure_dirs_idempotent() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());

        cache.ensure_dirs().await.unwrap();
        assert!(cache.cache_dir.exists());

        // Second call is a no-op — should not error.
        cache.ensure_dirs().await.unwrap();
        assert!(cache.cache_dir.exists());
    }

    // INTENT: Verify load_index returns empty map when no index file exists.
    #[tokio::test]
    async fn test_load_index_missing_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());

        let idx = cache.load_index().await.unwrap();
        assert!(idx.is_empty());
    }

    // INTENT: Verify save_index + load_index round-trips data correctly.
    #[tokio::test]
    async fn test_save_load_index_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());

        let mut index = HashMap::new();
        index.insert(
            "test-skill".to_string(),
            SkillMeta {
                name: "test-skill".to_string(),
                tags: vec!["rust".to_string(), "async".to_string()],
                source: SkillSource::Project,
                path: PathBuf::from("/fake/test-skill.md"),
                content_hash: "abc123".to_string(),
                last_modified: 1000,
            },
        );

        cache.save_index(&index).await.unwrap();

        let loaded = cache.load_index().await.unwrap();
        assert_eq!(loaded.len(), 1);
        let meta = loaded.get("test-skill").unwrap();
        assert_eq!(meta.name, "test-skill");
        assert_eq!(meta.tags, vec!["rust", "async"]);
        assert_eq!(meta.source, SkillSource::Project);
        assert_eq!(meta.content_hash, "abc123");
        assert_eq!(meta.last_modified, 1000);
    }

    // INTENT: Verify save_index uses atomic write (tmp file is cleaned up).
    #[tokio::test]
    async fn test_save_index_atomic_no_tmp_residue() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());

        let index = HashMap::new();
        cache.save_index(&index).await.unwrap();

        let tmp_path = cache.index_path.with_extension("json.tmp");
        assert!(!tmp_path.exists(), "tmp file should be renamed away");
        assert!(cache.index_path.exists(), "index.json should exist");
    }

    // INTENT: Verify refresh scans a project skills dir and populates the index.
    #[tokio::test]
    async fn test_refresh_scans_project_skills() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let project = tmp.path().join("project");
        let skills_dir = project.join(".open-mpm").join("skills");

        let body = "# My Skill\nSome content here.";
        write_skill(&skills_dir, "my-skill", body).await;

        cache.refresh(&project).await.unwrap();

        let idx = cache.load_index().await.unwrap();
        assert!(
            idx.contains_key("my-skill"),
            "index should contain my-skill"
        );
        let meta = idx.get("my-skill").unwrap();
        assert_eq!(meta.source, SkillSource::Project);
        assert_eq!(meta.content_hash, hex_sha256(body));
    }

    // INTENT: Verify get_content returns cached content (cache hit path).
    #[tokio::test]
    async fn test_get_content_cache_hit() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let project = tmp.path().join("project");
        let skills_dir = project.join(".open-mpm").join("skills");

        let body = "# Cached Skill\nBody text.";
        write_skill(&skills_dir, "cached", body).await;

        cache.refresh(&project).await.unwrap();

        let idx = cache.load_index().await.unwrap();
        let meta = idx.get("cached").unwrap();

        // Cache file should exist after refresh.
        let content = cache.get_content(meta).await.unwrap();
        assert_eq!(content, body);
    }

    // INTENT: Verify get_content falls back to disk when cache file is missing.
    #[tokio::test]
    async fn test_get_content_cache_miss_falls_back() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        cache.ensure_dirs().await.unwrap();

        let body = "# Fallback Skill\nDisk content.";
        let skill_path = tmp.path().join("standalone.md");
        fs::write(&skill_path, body).await.unwrap();

        let meta = SkillMeta {
            name: "standalone".to_string(),
            tags: vec![],
            source: SkillSource::UserLocal,
            path: skill_path,
            content_hash: hex_sha256(body),
            last_modified: 0,
        };

        // No cache file written — should fall back to meta.path.
        let content = cache.get_content(&meta).await.unwrap();
        assert_eq!(content, body);

        // After fallback, cache file should now exist.
        let cache_file = cache.cache_dir.join(&meta.content_hash);
        assert!(cache_file.exists(), "content should be re-cached on miss");
    }

    // INTENT: Verify extract_tags_from_content parses YAML frontmatter tags.
    #[test]
    fn test_extract_tags_basic() {
        let content = "---\ntags: [rust, async]\n---\n# Skill\nBody.";
        let tags = extract_tags_from_content(content);
        assert_eq!(tags, vec!["rust", "async"]);
    }

    // INTENT: Verify extract_tags handles quoted tag values.
    #[test]
    fn test_extract_tags_quoted() {
        let content = "---\ntags: [\"rust\", 'tokio']\n---\n# Skill";
        let tags = extract_tags_from_content(content);
        assert_eq!(tags, vec!["rust", "tokio"]);
    }

    // INTENT: Verify extract_tags returns empty when no frontmatter present.
    #[test]
    fn test_extract_tags_no_frontmatter() {
        let content = "# Just a heading\nNo frontmatter here.";
        let tags = extract_tags_from_content(content);
        assert!(tags.is_empty());
    }

    // INTENT: Verify extract_tags returns empty when frontmatter has no tags line.
    #[test]
    fn test_extract_tags_frontmatter_without_tags() {
        let content = "---\nname: foo\n---\n# Skill";
        let tags = extract_tags_from_content(content);
        assert!(tags.is_empty());
    }

    // INTENT: Verify discovery_paths returns project dir first (highest priority).
    #[test]
    fn test_discovery_paths_project_first() {
        let project = PathBuf::from("/tmp/myproject");
        let paths = discovery_paths(&project);

        assert!(!paths.is_empty());
        assert_eq!(paths[0].0, project.join(".open-mpm/skills"));
        assert_eq!(paths[0].1, SkillSource::Project);
    }

    // INTENT: Verify discovery_paths contains all three source types in order.
    #[test]
    fn test_discovery_paths_order() {
        let project = PathBuf::from("/tmp/proj");
        let paths = discovery_paths(&project);

        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0].1, SkillSource::Project);
        assert_eq!(paths[1].1, SkillSource::UserLocal);
        assert_eq!(paths[2].1, SkillSource::SkillsetMcp);
    }

    // INTENT: Verify hex_sha256 produces correct deterministic output.
    #[test]
    fn test_hex_sha256_deterministic() {
        let a = hex_sha256("hello");
        let b = hex_sha256("hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // SHA-256 hex = 64 chars

        let c = hex_sha256("world");
        assert_ne!(a, c);
    }

    // INTENT: Verify refresh handles non-md files gracefully (skips them).
    #[tokio::test]
    async fn test_refresh_skips_non_md_files() {
        let tmp = TempDir::new().unwrap();
        let cache = cache_in(tmp.path());
        let project = tmp.path().join("project");
        let skills_dir = project.join(".open-mpm").join("skills");

        fs::create_dir_all(&skills_dir).await.unwrap();
        fs::write(skills_dir.join("readme.txt"), "not a skill")
            .await
            .unwrap();
        write_skill(&skills_dir, "real", "# Real skill").await;

        cache.refresh(&project).await.unwrap();

        let idx = cache.load_index().await.unwrap();
        assert_eq!(idx.len(), 1);
        assert!(idx.contains_key("real"));
    }
}
