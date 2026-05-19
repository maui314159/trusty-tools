//! Tag-indexed skill registry with configurable sources (#168).
//!
//! Why: Skills were previously discovered ad-hoc from `config/skills/` with no
//! way for operators to point the harness at per-project or per-user skill
//! overrides, and agents had to do free-form `query` search to find relevant
//! skills (slow + unreliable). This registry mirrors the `AgentRegistry`
//! pattern (#167): scan a priority-ordered list of directories at startup,
//! parse minimal YAML frontmatter, index skills by name AND by tag so
//! `list_skills(tags=[...])` is O(1) per tag and deterministically ranked by
//! tag-overlap score. No LLM or embedding call is on the hot path.
//! What: `SkillMeta` is a thin metadata record (name, description, tags,
//! source_path); `SkillRegistry` owns an ordered name → meta map plus an
//! inverted tag → Vec<name> index. Discovery is failure-tolerant: missing
//! directories and malformed frontmatter log at `warn` and are skipped.
//! Test: See the unit tests at the bottom of this module.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Current schema version for the persisted skill effectiveness index (#197).
///
/// Why: When `SkillMeta` gains new required fields, on-disk indexes written by
/// older versions will fail to deserialize. Bumping this constant and writing
/// it into every saved index lets `merge_index` detect stale files quickly and
/// discard them rather than emitting confusing field-missing errors on every
/// startup.
/// What: A monotonically-increasing `u32` embedded in the `SkillIndex` wrapper
/// that wraps the flat `name -> SkillMeta` map. Increment when a breaking
/// schema change is made.
pub const SKILL_INDEX_SCHEMA_VERSION: u32 = 1;

/// Versioned on-disk envelope for the skill effectiveness index (#197).
///
/// Why: Wrapping the flat `HashMap` in a struct with a `schema_version` field
/// lets future readers detect indexes written by older code and discard them
/// gracefully rather than failing with cryptic serde errors.
/// What: Serialized as a JSON object with `schema_version` (u32) and `skills`
/// (the flat `name -> SkillMeta` map).
/// Test: `save_and_load_index_roundtrip`.
#[derive(Debug, Serialize, Deserialize)]
struct SkillIndex {
    #[serde(default)]
    schema_version: u32,
    skills: HashMap<String, SkillMeta>,
}

/// Hard cap on `.md` files indexed from a single source directory (#184).
///
/// Why: A user-level skills dir like `~/.claude/skills/` can contain hundreds
/// of skills (claude-mpm bundles 700+) which made startup hang for 30+ minutes
/// while every nested directory was read and parsed. A bounded scan trades
/// completeness on huge external libraries for a predictable startup time.
/// What: Once `MAX_SKILLS_PER_SOURCE` skills have been discovered inside one
/// source root, `visit_dir` stops descending. Operators who want a higher cap
/// can either split the directory or contribute a configurable knob later.
pub const MAX_SKILLS_PER_SOURCE: usize = 50;

/// Threshold for "this looks like an external skill repo, not an open-mpm
/// source" detection (#184).
///
/// Why: claude-mpm and similar projects ship hundreds of `.md` files in flat
/// or shallow layouts; loading them silently costs minutes on cold-cache disks.
/// We bail out early with a WARN so operators see why their skills didn't
/// appear and can opt in explicitly via `skill-sources.toml`.
const LARGE_DIR_MD_THRESHOLD: usize = 200;

/// Per-source-root scan timeout (#184).
///
/// Why: Even with the count cap, a pathological filesystem (network mount,
/// symlink loop) could stall startup. A wall-clock budget enforced inside
/// `visit_dir` lets us abandon a misbehaving source and continue with the
/// rest of the registry rather than hang forever.
const PER_SOURCE_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimal frontmatter-parsed description of one skill.
///
/// Why: The full file body is only needed when an agent actually loads the
/// skill. Listing + tag ranking only need name/description/tags, and keeping
/// a lightweight struct keeps the registry cheap to clone / pass through
/// `Arc`.
/// What: Holds the canonical skill name, human description (may be empty),
/// tag list, and absolute path to the `.md` file on disk.
/// Test: `registry_finds_skills_by_tag`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    /// Human-readable description of the skill. May be empty for skills that
    /// omit the `description` frontmatter key, and defaults to an empty string
    /// when deserializing older index.json files that predate this field
    /// (#216).
    #[serde(default)]
    pub description: String,
    pub tags: Vec<String>,
    pub source_path: PathBuf,
    /// Effectiveness score in `[0.0, 1.0]` used as a multiplier on tag-overlap
    /// rankings (#171). Defaults to `0.5` so newly discovered skills start in
    /// the neutral middle and earn rank up or down via `update_effectiveness`.
    ///
    /// Why: Pure tag overlap can't distinguish a stale, broken skill from a
    /// fresh, useful one. An exponentially-smoothed effectiveness score lets
    /// the system learn from outcomes (e.g., did the run succeed?) without
    /// needing a heavy ML pipeline.
    /// What: A single f32 in `[0.0, 1.0]`; the tag-overlap score is multiplied
    /// by this before sorting, so a low-effectiveness skill with many matching
    /// tags can rank below a high-effectiveness skill with fewer tags.
    /// Test: `effectiveness_score_influences_ranking`,
    /// `skill_meta_deserializes_with_defaults`.
    #[serde(default = "default_effectiveness")]
    pub effectiveness_score: f32,
    /// Total times this skill was injected into a phase prompt (#171).
    ///
    /// Why: Operators benefit from observability into which skills are
    /// pulling weight; this is the simplest counter that surfaces it.
    /// What: Monotonically incremented by `update_skill_usage` after each
    /// workflow run.
    /// Test: `skill_meta_deserializes_with_defaults`.
    #[serde(default)]
    pub use_count: u32,
    /// ISO-8601 UTC timestamp of the most recent injection, or `None` (#171).
    ///
    /// Why: Lets cleanup tooling identify cold skills without scanning logs.
    /// What: Stored as a string to avoid leaking `chrono` types through the
    /// public `SkillMeta` API.
    /// Test: `skill_meta_deserializes_with_defaults`.
    #[serde(default)]
    pub last_used: Option<String>,
}

/// Neutral default effectiveness — new skills start in the middle and earn
/// rank up or down based on actual usage outcomes.
fn default_effectiveness() -> f32 {
    0.5
}

/// In-memory, tag-indexed catalog of every discovered skill.
///
/// Why: `list_skills(tags=[...])` is called on every agent's first tool-using
/// turn; an O(1) inverted-tag index beats re-scanning the filesystem or the
/// skill list each call. Priority ordering (first source wins) gives operators
/// a predictable override story: project-local beats user-level beats bundled.
/// What: `skills` preserves discovery order so `list()` is deterministic;
/// `tag_index` is an inverted map `tag → [skill_names]` built during `load`.
/// Test: `registry_tag_overlap_ranking`, `registry_higher_priority_source_wins`.
pub struct SkillRegistry {
    /// Canonical name → metadata (ordered by discovery).
    skills: IndexMap<String, SkillMeta>,
    /// Inverted index: tag → skill names that carry that tag.
    tag_index: HashMap<String, Vec<String>>,
    /// BM25 semantic search index over all discovered skills (#483).
    ///
    /// Why: Tag lookup requires the caller to already know the right tags;
    /// per-turn dynamic injection instead has only the free-text user message.
    /// A BM25 index ranks skills against that message so the harness can pick
    /// the few most relevant skills to inject without an embedding model.
    /// What: Built from the same search paths as the tag index; `None` until
    /// `attach_bm25_index` is called so existing call sites that build the
    /// registry without the index keep working unchanged.
    bm25: Option<super::index::SkillIndex>,
}

impl SkillRegistry {
    /// Scan `search_paths` in priority order and build the registry.
    ///
    /// Why: Mirrors `AgentRegistry::load` — earlier entries shadow later ones
    /// on name conflict so a `.open-mpm/skills/fastapi.md` override wins over
    /// the bundled `config/skills/frameworks/fastapi.md`. Missing directories
    /// are a graceful no-op because operators don't want to pre-create every
    /// layer of the hierarchy just to run the harness.
    /// What: Walks each directory recursively for `*.md` files; parses
    /// frontmatter (`name`, `description`, `tags`); inserts the first
    /// occurrence of each name and ignores later duplicates. Files without a
    /// frontmatter block or without both `name` and `tags` fields log a WARN
    /// and are skipped. Builds `tag_index` once at the end.
    /// Test: `registry_finds_skills_by_tag`, `registry_skips_files_without_frontmatter`,
    /// `registry_higher_priority_source_wins`.
    pub fn load(search_paths: &[PathBuf]) -> Self {
        let mut skills: IndexMap<String, SkillMeta> = IndexMap::new();

        for dir in search_paths {
            if !dir.is_dir() {
                tracing::debug!(path = %dir.display(), "skill search path missing, skipping");
                continue;
            }
            // #184: Skip directories that look like external skill repos
            // (e.g. claude-mpm's ~/.claude/skills/ with 700+ files). Operators
            // who want them must opt in via skill-sources.toml.
            if looks_like_external_skill_dir(dir) {
                tracing::warn!(
                    path = %dir.display(),
                    threshold = LARGE_DIR_MD_THRESHOLD,
                    "Skipping large skill directory (>={LARGE_DIR_MD_THRESHOLD} files): \
                     {} — add to skill-sources.toml to opt in",
                    dir.display()
                );
                continue;
            }
            // #184: Per-source budget enforcement (count cap + wall-clock).
            let started = Instant::now();
            let count_before = skills.len();
            visit_dir(dir, &mut skills, dir, &started);
            let added = skills.len() - count_before;
            if started.elapsed() >= PER_SOURCE_SCAN_TIMEOUT {
                tracing::warn!(
                    path = %dir.display(),
                    added,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "skill scan timeout for {}, skipping remaining files",
                    dir.display()
                );
            }
        }

        let mut tag_index: HashMap<String, Vec<String>> = HashMap::new();
        for (name, meta) in &skills {
            for tag in &meta.tags {
                let key = tag.to_lowercase();
                tag_index.entry(key).or_default().push(name.clone());
            }
        }

        // #483: Build the BM25 search index from the same source directories so
        // `search` can rank skills against a free-text query. Failure-tolerant —
        // a build error leaves `bm25` as `None` and `search` returns empty.
        let bm25 = match super::index::SkillIndex::build(search_paths) {
            Ok(idx) => Some(idx),
            Err(e) => {
                tracing::warn!(error = %e, "skills: BM25 index build failed; search disabled");
                None
            }
        };

        Self {
            skills,
            tag_index,
            bm25,
        }
    }

    /// Return up to `n` skill names ranked by BM25 relevance to `query` (#483).
    ///
    /// Why: Per-turn dynamic skill injection needs to rank ALL discoverable
    /// skills against the current free-text user message, which tag lookup
    /// cannot do. This delegates to the BM25 index built during `load`.
    /// What: Returns an empty vector when the BM25 index is absent (registry
    /// built via `empty`), the query is blank, or no skill matches.
    /// Test: `registry_search_delegates_to_bm25_index`.
    pub fn search(&self, query: &str, n: usize) -> Vec<String> {
        match &self.bm25 {
            Some(idx) => idx.search(query, n),
            None => Vec::new(),
        }
    }

    /// Scan paths from a `SkillSourceRegistry`, falling back to `bundled_dir`
    /// at the end (#172).
    ///
    /// Why: Operator-configured sources should be primary; the bundled
    /// `<config_dir>/skills` tree is the safety net so missing/empty configs
    /// still produce a useful catalog.
    /// What: Calls `sources.resolved_paths()` to get the priority-ordered list
    /// of on-disk dirs, appends `bundled_dir` last, and delegates to `load`.
    /// Skips bundled fallback when it duplicates an already-listed path so the
    /// "first dir wins" rule in `load` doesn't accidentally shadow itself.
    /// Test: Indirect — covered by `SkillSourceRegistry` unit tests plus the
    /// existing `SkillRegistry::load` priority/shadow tests.
    pub fn from_sources(sources: &super::sources::SkillSourceRegistry, bundled_dir: &Path) -> Self {
        let mut paths = sources.resolved_paths();
        if !paths.iter().any(|p| p == bundled_dir) {
            paths.push(bundled_dir.to_path_buf());
        }
        Self::load(&paths)
    }

    /// Build an empty registry (useful for tests and graceful fallbacks).
    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self {
            skills: IndexMap::new(),
            tag_index: HashMap::new(),
            bm25: None,
        }
    }

    /// Number of indexed skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// True when no skills were discovered.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Find skills matching ANY of the given tags, ranked by an effectiveness-
    /// weighted overlap score (#171).
    ///
    /// Why: Pure tag-overlap counts can't distinguish a stale, low-quality
    /// skill from a fresh, high-impact one. Multiplying the raw overlap by
    /// `effectiveness_score` lets a 1-tag-match high-quality skill outrank a
    /// 2-tag-match low-quality skill, so the system improves as outcomes
    /// feed back into the score.
    /// What: Case-insensitive tag lookup; deduplicates skill names across
    /// matching tags; computes `effective_score = tag_count * effectiveness`
    /// and sorts that descending, breaking ties by insertion (discovery)
    /// order for determinism.
    /// Test: `registry_tag_overlap_ranking`,
    /// `effectiveness_score_influences_ranking`.
    pub fn find_by_tags(&self, tags: &[&str]) -> Vec<&SkillMeta> {
        if tags.is_empty() || self.skills.is_empty() {
            return Vec::new();
        }
        // name -> number of tag hits.
        let mut scores: HashMap<&str, usize> = HashMap::new();
        for want in tags {
            let key = want.to_lowercase();
            if let Some(names) = self.tag_index.get(&key) {
                for n in names {
                    *scores.entry(n.as_str()).or_insert(0) += 1;
                }
            }
        }
        if scores.is_empty() {
            return Vec::new();
        }
        // Stable ranking: primary = -effective_score (higher first); secondary
        // = index position in the IndexMap (earlier discovery wins ties).
        // `effective_score = tag_count * effectiveness_score`. Tie-breaker uses
        // total_cmp so NaN never panics (defensive — effectiveness is bounded).
        let mut ordered: Vec<(&str, f32, usize)> = scores
            .into_iter()
            .map(|(name, count)| {
                let idx = self.skills.get_index_of(name).unwrap_or(usize::MAX);
                let eff = self
                    .skills
                    .get(name)
                    .map(|m| m.effectiveness_score)
                    .unwrap_or(default_effectiveness());
                (name, count as f32 * eff, idx)
            })
            .collect();
        ordered.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
        ordered
            .into_iter()
            .filter_map(|(name, _, _)| self.skills.get(name))
            .collect()
    }

    /// Update a skill's effectiveness score with a new outcome observation
    /// using exponential moving average (#171).
    ///
    /// Why: Each successful run is one weak signal — replacing the score on
    /// every observation would be too noisy. EMA with alpha=0.3 smooths
    /// outliers while still tracking trend changes within a few runs.
    /// What: `new = 0.3 * score + 0.7 * old`, with `score` clamped to
    /// `[0.0, 1.0]`. No-op when `skill_name` is unknown.
    /// Test: `update_effectiveness_ema`.
    pub fn update_effectiveness(&mut self, skill_name: &str, score: f32) {
        if let Some(meta) = self.skills.get_mut(skill_name) {
            let clamped = score.clamp(0.0, 1.0);
            meta.effectiveness_score = 0.3 * clamped + 0.7 * meta.effectiveness_score;
        }
    }

    /// Increment `use_count` and refresh `last_used` for `skill_name` (#171).
    ///
    /// Why: Persistence functions need a primitive to record an injection
    /// without callers reaching into private fields.
    /// What: Increments `use_count` (saturating) and sets `last_used` to the
    /// supplied ISO-8601 timestamp. No-op when `skill_name` is unknown.
    /// Test: Indirect via `update_skill_usage` integration.
    pub fn record_use(&mut self, skill_name: &str, timestamp_iso: &str) {
        if let Some(meta) = self.skills.get_mut(skill_name) {
            meta.use_count = meta.use_count.saturating_add(1);
            meta.last_used = Some(timestamp_iso.to_string());
        }
    }

    /// Persist the in-memory skill map to a JSON index on disk (#171).
    ///
    /// Why: Effectiveness/usage learning is only valuable if it survives
    /// process restarts. The index is a versioned wrapper containing a flat
    /// `name -> SkillMeta` map keyed by canonical name so a future load can
    /// merge it back over a fresh scan. The `schema_version` field lets future
    /// schema changes be detected and old files discarded cleanly.
    /// What: Creates parent directories if needed, then writes
    /// `serde_json::to_string_pretty` of the `SkillIndex` wrapper. Errors are
    /// returned for the caller to log/swallow at a higher level.
    /// Test: `save_and_load_index_roundtrip`.
    pub fn save_index(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let map: HashMap<String, SkillMeta> = self
            .skills
            .iter()
            .map(|(name, meta)| (name.clone(), meta.clone()))
            .collect();
        let index = SkillIndex {
            schema_version: SKILL_INDEX_SCHEMA_VERSION,
            skills: map,
        };
        let text = serde_json::to_string_pretty(&index)?;
        // #198: Use state_writer for advisory-locked atomic write so a second
        // open-mpm process can't truncate the index mid-write. The previous
        // tmp+rename was atomic on its own filesystem but had no inter-process
        // coordination — two writers could each rename a different `.tmp`
        // alternately and the persisted state would race.
        crate::state_writer::atomic_write(path, text.as_bytes())?;
        Ok(())
    }

    /// Merge persisted effectiveness/usage fields from a JSON index into the
    /// already-scanned registry (#171, #197).
    ///
    /// Why: The on-disk scan rebuilds `SkillMeta` with defaults; without a
    /// merge step, every restart would wipe the learned effectiveness scores.
    /// What: Reads the JSON at `path` (no-op when absent). If deserialization
    /// fails (stale schema, missing `description` field, etc.) the stale file
    /// is deleted and the registry continues with its freshly-scanned defaults
    /// — no error is propagated to the caller. On success, copies
    /// `effectiveness_score`, `use_count`, and `last_used` from the persisted
    /// map over the matching freshly-scanned entries. Skills present only in
    /// the index but missing on disk are ignored.
    /// Test: `merge_index_restores_effectiveness_after_reload`,
    ///       `merge_index_stale_file_is_deleted_and_noop`.
    pub fn merge_index(&mut self, path: &Path) -> anyhow::Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // Try the versioned wrapper first; fall back to the legacy flat map
        // format for indexes written by versions before #197.
        let map: HashMap<String, SkillMeta> = if let Ok(idx) =
            serde_json::from_str::<SkillIndex>(&text)
        {
            idx.skills
        } else {
            match serde_json::from_str::<HashMap<String, SkillMeta>>(&text) {
                Ok(m) => m,
                Err(e) => {
                    // #197 / #216: Stale/corrupt index — delete it and
                    // proceed with fresh defaults.  Log at DEBUG so that
                    // normal schema migrations (e.g. a field added in a
                    // new release) are silent on startup; only genuinely
                    // unexpected corruption surfaces here.
                    tracing::debug!(
                        error = %e,
                        path = %path.display(),
                        "skills index schema mismatch — deleting stale index and using fresh defaults"
                    );
                    let _ = std::fs::remove_file(path);
                    return Ok(());
                }
            }
        };
        for (name, persisted) in map {
            if let Some(meta) = self.skills.get_mut(&name) {
                meta.effectiveness_score = persisted.effectiveness_score;
                meta.use_count = persisted.use_count;
                meta.last_used = persisted.last_used;
            }
        }
        Ok(())
    }

    /// Return the match score (overlap count) for a specific skill name,
    /// when the registry is queried with `tags`. Used by tool callers that
    /// want to include the score in their response JSON.
    pub fn tag_overlap_score(&self, name: &str, tags: &[&str]) -> usize {
        let Some(meta) = self.skills.get(name) else {
            return 0;
        };
        let have: HashSet<String> = meta.tags.iter().map(|t| t.to_lowercase()).collect();
        tags.iter()
            .filter(|t| have.contains(&t.to_lowercase()))
            .count()
    }

    /// Fetch the full Markdown content of a skill by name.
    ///
    /// Why: The registry only indexes metadata; full content is read on
    /// demand to keep startup fast and memory flat.
    /// What: Returns `None` when the name is unknown or the file can no
    /// longer be read (rare; indicates the file was deleted after startup).
    /// Test: Covered by the existing `load_skill` integration path.
    #[allow(dead_code)] // Wired into the `load_skill` tool in a follow-up PR.
    pub fn get_content(&self, name: &str) -> Option<String> {
        let meta = self.skills.get(name)?;
        match std::fs::read_to_string(&meta.source_path) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    name = %name,
                    path = %meta.source_path.display(),
                    error = %e,
                    "skill: failed to read file at load time"
                );
                None
            }
        }
    }

    /// List every discovered skill in priority/discovery order.
    pub fn list(&self) -> Vec<&SkillMeta> {
        self.skills.values().collect()
    }

    /// Look up a single skill's metadata by exact name.
    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.get(name)
    }
}

/// Detect "this is an external skill repo, skip it" (#184).
///
/// Why: claude-mpm's `~/.claude/skills/` directory has 700+ markdown files
/// organized in subdirectories with no `.toml` manifests — scanning it
/// recursively at every open-mpm startup hangs for tens of minutes. Operators
/// who genuinely want those skills indexed should add the path explicitly to
/// `.open-mpm/skill-sources.toml` so the opt-in is visible.
/// What: Returns `true` when the directory contains `>= LARGE_DIR_MD_THRESHOLD`
/// `.md` files (anywhere in the tree, sampled with an early-exit walk) AND
/// no `*.toml` skill manifests at the top level. The check itself is bounded
/// by both the count threshold and `PER_SOURCE_SCAN_TIMEOUT` so a giant tree
/// can't make the *probe* hang either.
/// Test: `looks_like_external_skill_dir_flags_claude_skills_layout`,
/// `looks_like_external_skill_dir_passes_open_mpm_layout`.
fn looks_like_external_skill_dir(dir: &Path) -> bool {
    // Top-level TOML manifest = "this is an open-mpm-shaped source".
    let has_toml = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("toml")),
        Err(_) => return false,
    };
    if has_toml {
        return false;
    }
    // Otherwise, count `.md` files (with budgets so the probe itself is cheap).
    let started = Instant::now();
    let mut count = 0usize;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if started.elapsed() >= PER_SOURCE_SCAN_TIMEOUT {
            // Probe ran out of time; assume "external" so we don't hang the
            // real scan downstream.
            return true;
        }
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
                count += 1;
                if count >= LARGE_DIR_MD_THRESHOLD {
                    return true;
                }
            }
        }
    }
    false
}

/// Recurse through `dir` inserting every `.md` file as a skill entry.
///
/// Why: Bundled skills live in nested subdirectories (`languages/rust.md`,
/// `frameworks/fastapi.md`, `workflow/tdd.md`); a recursive walk keeps the
/// search-path config flat (one dir per source) while still picking up
/// organized layouts.
/// What: Silently skips unreadable entries; logs WARN on malformed
/// frontmatter. First writer wins on name conflict inside the same source.
fn visit_dir(
    dir: &Path,
    skills: &mut IndexMap<String, SkillMeta>,
    source_root: &Path,
    started: &Instant,
) {
    // #184: Count skills already loaded from THIS source root so we can
    // enforce `MAX_SKILLS_PER_SOURCE` without breaking earlier sources.
    let source_root_owned = source_root.to_path_buf();
    fn count_for_root(skills: &IndexMap<String, SkillMeta>, root: &Path) -> usize {
        let root_str = root.to_string_lossy();
        skills
            .values()
            .filter(|m| {
                m.source_path
                    .to_string_lossy()
                    .starts_with(root_str.as_ref())
            })
            .count()
    }
    if count_for_root(skills, &source_root_owned) >= MAX_SKILLS_PER_SOURCE {
        return;
    }
    if started.elapsed() >= PER_SOURCE_SCAN_TIMEOUT {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "failed to read skill dir");
            return;
        }
    };
    for entry in entries.flatten() {
        // #184: Re-check budgets each iteration so a deep tree can't blow past
        // the cap or timeout silently.
        if count_for_root(skills, &source_root_owned) >= MAX_SKILLS_PER_SOURCE {
            tracing::debug!(
                source = %source_root.display(),
                cap = MAX_SKILLS_PER_SOURCE,
                "skill source hit per-source cap; skipping remaining files"
            );
            return;
        }
        if started.elapsed() >= PER_SOURCE_SCAN_TIMEOUT {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            visit_dir(&path, skills, source_root, started);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        match parse_skill_meta(&path) {
            Ok(meta) => {
                if skills.contains_key(&meta.name) {
                    tracing::debug!(
                        skill = %meta.name,
                        shadowed = %path.display(),
                        "lower-priority skill shadowed by earlier dir"
                    );
                    continue;
                }
                tracing::debug!(
                    skill = %meta.name,
                    source = %path.display(),
                    "discovered skill"
                );
                skills.insert(meta.name.clone(), meta);
            }
            Err(ParseSkillError::MissingFrontmatter) => {
                tracing::warn!(
                    path = %path.display(),
                    "skill file has no YAML frontmatter; skipping"
                );
            }
            Err(ParseSkillError::MissingField(field)) => {
                tracing::warn!(
                    path = %path.display(),
                    field = %field,
                    "skill file missing required frontmatter field; skipping"
                );
            }
            Err(ParseSkillError::Io(e)) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to read skill file; skipping"
                );
            }
        }
    }
}

#[derive(Debug)]
enum ParseSkillError {
    Io(std::io::Error),
    MissingFrontmatter,
    MissingField(&'static str),
}

/// Parse `name`, `description`, `tags` out of a skill file's frontmatter.
///
/// Why: The YAML frontmatter we emit is dead simple (flat keys, one inline
/// list); pulling in a full YAML parser just for three keys is overkill and
/// would widen the dependency tree. A hand-rolled parser keeps the build fast
/// and the dependency graph tight.
/// What: Reads the file, locates the `---` / `---` fence block, extracts the
/// three keys, trims quotes. Returns `MissingField` when `name` or `tags` is
/// absent (those two are required so the registry can index by them).
/// Test: `registry_skips_files_without_frontmatter`.
fn parse_skill_meta(path: &Path) -> Result<SkillMeta, ParseSkillError> {
    let content = std::fs::read_to_string(path).map_err(ParseSkillError::Io)?;
    let fm = extract_frontmatter(&content).ok_or(ParseSkillError::MissingFrontmatter)?;
    let name = extract_value(fm, "name").ok_or(ParseSkillError::MissingField("name"))?;
    let description = extract_value(fm, "description").unwrap_or_default();
    let tags = extract_list(fm, "tags");
    if tags.is_empty() {
        return Err(ParseSkillError::MissingField("tags"));
    }
    Ok(SkillMeta {
        name,
        description,
        tags,
        source_path: path.to_path_buf(),
        effectiveness_score: default_effectiveness(),
        use_count: 0,
        last_used: None,
    })
}

/// Return the text between the opening and closing `---` fences, or `None`.
fn extract_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---")?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let end_rel = rest.find("\n---")?;
    Some(&rest[..end_rel])
}

fn extract_value(fm: &str, key: &str) -> Option<String> {
    for line in fm.lines() {
        let trimmed = line.trim();
        let prefix = format!("{key}:");
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let val = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

fn extract_list(fm: &str, key: &str) -> Vec<String> {
    for line in fm.lines() {
        let trimmed = line.trim();
        let prefix = format!("{key}:");
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let val = rest.trim();
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

/// Compute skill-source search paths in priority order (highest first).
///
/// Why: Centralizes the discovery policy so `main.rs`, `skills list`, and
/// future integrations all agree on where to look. Mirrors
/// `agents::registry::agent_search_paths`. The sibling `trusty-common/skills`
/// directory (when present alongside this repo) is included so cross-project
/// skill libraries authored in trusty-common are visible to open-mpm without
/// duplicating files.
/// What: Returns, in order: `.open-mpm/skills`, `.claude/skills`,
/// `../trusty-common/skills` (sibling repo, if it exists),
/// `~/.open-mpm/skills`, `~/.claude/skills`, `<config_dir>/skills`.
/// Test: `skill_search_paths_order`.
pub fn skill_search_paths(config_dir: &Path) -> Vec<PathBuf> {
    // #184: When `OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY=1` (set by CTRL for its
    // lightweight LLM turns), restrict discovery to the project-local
    // `.open-mpm/skills` directory and the bundled fallback. This skips
    // `~/.claude/skills/` (claude-mpm's 700+-file repo) which previously
    // hung CTRL's startup for 30+ minutes.
    if std::env::var("OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some()
    {
        return vec![PathBuf::from(".open-mpm/skills"), config_dir.join("skills")];
    }
    let mut paths = Vec::new();
    paths.push(PathBuf::from(".open-mpm/skills"));
    paths.push(PathBuf::from(".claude/skills"));
    // Sibling `trusty-common/skills` repo: cross-project shared skill library.
    // Only included when the directory actually exists so users without the
    // sibling checkout don't see warnings. Project-local skills (above) still
    // win on name collisions.
    let trusty_common = PathBuf::from("../trusty-common/skills");
    if trusty_common.is_dir() {
        paths.push(trusty_common);
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home.clone()).join(".open-mpm/skills"));
        paths.push(PathBuf::from(home).join(".claude/skills"));
    }
    paths.push(config_dir.join("skills"));
    paths
}

/// Canonical path of the persisted skill effectiveness index (#171).
///
/// Why: Centralizes the `~/.open-mpm/skills/index.json` location so startup
/// (merge_index) and post-run (save_index) callers agree on the same file.
/// What: Returns `~/.open-mpm/skills/index.json` when `$HOME` is set, else
/// `.open-mpm/skills/index.json` relative to the CWD as a fallback.
/// Test: Indirect via `merge_index_restores_effectiveness_after_reload`.
pub fn skill_index_path() -> PathBuf {
    let base = if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".open-mpm").join("skills")
    } else {
        PathBuf::from(".open-mpm").join("skills")
    };
    base.join("index.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::HOME_LOCK;
    use std::fs;
    use tempfile::TempDir;

    fn write_skill(dir: &Path, name: &str, description: &str, tags: &[&str]) {
        let tags_str = tags
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let content = format!(
            "---\nname: {name}\ndescription: {description}\ntags: [{tags_str}]\n---\n\n# {name}\nbody\n",
        );
        fs::write(dir.join(format!("{name}.md")), content).unwrap();
    }

    #[test]
    fn registry_finds_skills_by_tag() {
        let dir = TempDir::new().unwrap();
        write_skill(
            dir.path(),
            "fastapi",
            "async routes",
            &["python", "fastapi"],
        );
        write_skill(dir.path(), "pytest", "fixtures", &["python", "pytest"]);

        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);

        let hits = reg.find_by_tags(&["python"]);
        assert_eq!(hits.len(), 2);
        let names: Vec<&str> = hits.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"fastapi"));
        assert!(names.contains(&"pytest"));
    }

    #[test]
    fn registry_skips_files_without_frontmatter() {
        let dir = TempDir::new().unwrap();
        // Valid file: indexed.
        write_skill(dir.path(), "ok", "desc", &["tag1"]);
        // No frontmatter: skipped with warn, not a panic.
        fs::write(
            dir.path().join("plain.md"),
            "# Just markdown, no frontmatter\n",
        )
        .unwrap();
        // Frontmatter missing `tags`: skipped with warn.
        fs::write(
            dir.path().join("notag.md"),
            "---\nname: notag\ndescription: missing tags\n---\nbody\n",
        )
        .unwrap();
        // Frontmatter missing `name`: skipped with warn.
        fs::write(
            dir.path().join("noname.md"),
            "---\ndescription: missing name\ntags: [x]\n---\nbody\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(reg.len(), 1, "only 'ok' should be indexed");
        assert!(reg.get("ok").is_some());
    }

    #[test]
    fn registry_higher_priority_source_wins() {
        let high = TempDir::new().unwrap();
        let low = TempDir::new().unwrap();
        // Same name in both dirs; high wins.
        write_skill(high.path(), "shared", "from-high", &["tag-high"]);
        write_skill(low.path(), "shared", "from-low", &["tag-low"]);
        // Unique-to-low skill still appears.
        write_skill(low.path(), "only-low", "low only", &["low-tag"]);

        let reg = SkillRegistry::load(&[high.path().to_path_buf(), low.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);
        let shared = reg.get("shared").expect("shared present");
        assert_eq!(shared.description, "from-high", "high-priority dir wins");
        assert!(reg.get("only-low").is_some());
    }

    #[test]
    fn registry_tag_overlap_ranking() {
        let dir = TempDir::new().unwrap();
        // Triple match for "three"; double for "two"; single for "one".
        write_skill(dir.path(), "three", "d", &["python", "fastapi", "pytest"]);
        write_skill(dir.path(), "two", "d", &["python", "fastapi"]);
        write_skill(dir.path(), "one", "d", &["python"]);

        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let ranked = reg.find_by_tags(&["python", "fastapi", "pytest"]);
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].name, "three", "3-tag match should rank first");
        assert_eq!(ranked[1].name, "two", "2-tag match should rank second");
        assert_eq!(ranked[2].name, "one", "1-tag match should rank last");
    }

    #[test]
    fn registry_find_by_tags_case_insensitive() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "rs", "d", &["Rust", "Async"]);
        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let hits = reg.find_by_tags(&["rust"]);
        assert_eq!(hits.len(), 1);
        let hits = reg.find_by_tags(&["ASYNC"]);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn registry_recursive_scan_picks_up_nested_dirs() {
        let root = TempDir::new().unwrap();
        let nested = root.path().join("languages");
        fs::create_dir_all(&nested).unwrap();
        write_skill(&nested, "rust", "rust idioms", &["rust"]);
        write_skill(root.path(), "top", "top level", &["top"]);

        let reg = SkillRegistry::load(&[root.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("rust").is_some());
        assert!(reg.get("top").is_some());
    }

    #[test]
    fn registry_get_content_returns_file_body() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "x", "d", &["t"]);
        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let content = reg.get_content("x").expect("content present");
        assert!(content.contains("---"));
        assert!(content.contains("# x"));
    }

    #[test]
    fn skill_search_paths_order() {
        // HOME_LOCK serializes with other tests that mutate $HOME process-wide.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        // SAFETY: test-only; we restore HOME at the end.
        unsafe {
            std::env::set_var("HOME", "/tmp/skills-home-test");
        }
        let paths = skill_search_paths(Path::new("/opt/open-mpm/config"));
        assert_eq!(paths[0], PathBuf::from(".open-mpm/skills"));
        assert_eq!(paths[1], PathBuf::from(".claude/skills"));
        // The `../trusty-common/skills` sibling path is conditionally inserted
        // based on whether the directory exists at test time. Skip past it if
        // present so the remaining assertions stay stable across environments.
        let trusty_common = PathBuf::from("../trusty-common/skills");
        let mut idx = 2;
        if paths.get(idx) == Some(&trusty_common) {
            idx += 1;
        }
        assert_eq!(
            paths[idx],
            PathBuf::from("/tmp/skills-home-test/.open-mpm/skills")
        );
        assert_eq!(
            paths[idx + 1],
            PathBuf::from("/tmp/skills-home-test/.claude/skills")
        );
        assert_eq!(paths[idx + 2], PathBuf::from("/opt/open-mpm/config/skills"));

        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    // ── Integration-style tests against bundled .open-mpm/skills/ ──────────
    //
    // Why: Confirms that the bundled `.md` skills under `.open-mpm/skills/`
    // still parse correctly and that the tag index surfaces the expected
    // entries. Guards against frontmatter drift in the shipped skill library.
    // Test: Run `cargo test --lib skill_registry_discovers_bundled_skills`.

    fn bundled_skills_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".open-mpm")
            .join("skills")
    }

    #[test]
    fn skill_registry_discovers_bundled_skills() {
        let paths = vec![bundled_skills_dir()];
        let registry = SkillRegistry::load(&paths);
        assert!(!registry.is_empty(), "expected at least one bundled skill");

        let python_hits = registry.find_by_tags(&["python"]);
        assert!(
            !python_hits.is_empty(),
            "expected at least one python-tagged skill"
        );

        let fastapi_hits = registry.find_by_tags(&["fastapi"]);
        assert!(
            fastapi_hits.iter().any(|s| s.name == "fastapi"),
            "expected fastapi skill discoverable by tag"
        );
    }

    #[test]
    fn skill_registry_ranks_by_tag_overlap() {
        let paths = vec![bundled_skills_dir()];
        let registry = SkillRegistry::load(&paths);
        let results = registry.find_by_tags(&["python", "fastapi", "pytest"]);
        // The registry is non-empty and results don't panic.
        assert!(!results.is_empty(), "expected non-empty results");
        // First result carries the highest tag overlap; verify it carries at
        // least one of the queried tags (sanity of ranking stability).
        let first = results[0];
        let queried: Vec<String> = ["python", "fastapi", "pytest"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let overlap = first
            .tags
            .iter()
            .filter(|t| queried.iter().any(|q| q.eq_ignore_ascii_case(t)))
            .count();
        assert!(
            overlap >= 1,
            "top-ranked skill should overlap at least one queried tag (got {overlap})"
        );
    }

    #[test]
    fn registry_tag_overlap_score_counts_matches() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "s", "d", &["a", "b", "c"]);
        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(reg.tag_overlap_score("s", &["a", "b"]), 2);
        assert_eq!(reg.tag_overlap_score("s", &["a", "z"]), 1);
        assert_eq!(reg.tag_overlap_score("s", &["z"]), 0);
        assert_eq!(reg.tag_overlap_score("missing", &["a"]), 0);
    }

    // ── #171: effectiveness scoring + persistence ──────────────────────────

    /// Why: Verifies that the effectiveness multiplier can flip ranking when
    /// raw tag overlap alone would have ordered the skills differently — a
    /// 1-tag, high-effectiveness skill must rank above a 2-tag, low-effectiveness
    /// one (`1*0.9 = 0.9 > 2*0.3 = 0.6`).
    /// What: Builds two skills, mutates effectiveness, then queries by tags.
    /// Test: `effectiveness_score_influences_ranking`.
    #[test]
    fn effectiveness_score_influences_ranking() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "stale", "d", &["python", "fastapi"]);
        write_skill(dir.path(), "fresh", "d", &["python"]);
        let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);

        // Sanity: with default effectiveness (0.5 each), 2-tag wins.
        let baseline = reg.find_by_tags(&["python", "fastapi"]);
        assert_eq!(baseline[0].name, "stale");

        // Tilt the scores hard enough to flip the ranking.
        reg.update_effectiveness("stale", 0.0); // 0.3*0 + 0.7*0.5 = 0.35
        for _ in 0..10 {
            reg.update_effectiveness("stale", 0.0);
        }
        for _ in 0..10 {
            reg.update_effectiveness("fresh", 1.0);
        }

        let ranked = reg.find_by_tags(&["python", "fastapi"]);
        assert_eq!(
            ranked[0].name, "fresh",
            "high-effectiveness 1-tag skill should rank above low-effectiveness 2-tag skill"
        );
    }

    /// Why: Locks the EMA formula so future refactors can't silently change
    /// the weighting and skew rankings on long-lived installs.
    /// What: Starts at default (0.5), pushes a 1.0 observation, asserts the
    /// expected 0.65 result.
    /// Test: `update_effectiveness_ema`.
    #[test]
    fn update_effectiveness_ema() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "x", "d", &["t"]);
        let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);

        // 0.3 * 1.0 + 0.7 * 0.5 = 0.65
        reg.update_effectiveness("x", 1.0);
        let meta = reg.get("x").unwrap();
        assert!((meta.effectiveness_score - 0.65).abs() < 1e-6);

        // 0.3 * 0.0 + 0.7 * 0.65 = 0.455
        reg.update_effectiveness("x", 0.0);
        let meta = reg.get("x").unwrap();
        assert!((meta.effectiveness_score - 0.455).abs() < 1e-6);

        // Out-of-range scores are clamped before applying.
        reg.update_effectiveness("x", 5.0);
        let meta = reg.get("x").unwrap();
        // 0.3 * 1.0 + 0.7 * 0.455 = 0.6185
        assert!((meta.effectiveness_score - 0.6185).abs() < 1e-6);
    }

    /// Why: Existing JSON indexes (or hand-edited fixtures) must not need an
    /// effectiveness field to deserialize — defaults keep migrations painless.
    /// What: Deserializes a `SkillMeta` from a minimal JSON document and
    /// asserts the new fields took their defaults.
    /// Test: `skill_meta_deserializes_with_defaults`.
    #[test]
    fn skill_meta_deserializes_with_defaults() {
        let raw = r#"{
            "name": "x",
            "description": "d",
            "tags": ["t"],
            "source_path": "/tmp/x.md"
        }"#;
        let meta: SkillMeta = serde_json::from_str(raw).expect("deserialize without new fields");
        assert!((meta.effectiveness_score - 0.5).abs() < 1e-6);
        assert_eq!(meta.use_count, 0);
        assert!(meta.last_used.is_none());
    }

    /// Why: Round-trip protection — a save followed by a load must restore
    /// every persisted field so effectiveness learning isn't lost on restart.
    /// What: Writes the index, reloads from disk, merges into a fresh
    /// registry, asserts the persisted values overwrote the defaults.
    /// Test: `merge_index_restores_effectiveness_after_reload`.
    #[test]
    fn merge_index_restores_effectiveness_after_reload() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "x", "d", &["t"]);
        let index_path = dir.path().join("index.json");

        // Run 1: train and persist.
        {
            let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
            reg.update_effectiveness("x", 1.0); // -> 0.65
            reg.record_use("x", "2026-04-24T00:00:00Z");
            reg.save_index(&index_path).expect("save_index");
        }

        // Run 2: fresh scan + merge.
        let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let meta = reg.get("x").unwrap();
        assert!(
            (meta.effectiveness_score - 0.5).abs() < 1e-6,
            "fresh scan resets to default"
        );

        reg.merge_index(&index_path).expect("merge_index");
        let meta = reg.get("x").unwrap();
        assert!((meta.effectiveness_score - 0.65).abs() < 1e-6);
        assert_eq!(meta.use_count, 1);
        assert_eq!(meta.last_used.as_deref(), Some("2026-04-24T00:00:00Z"));
    }

    // ── #184: Skill loading hang fixes ────────────────────────────────────

    /// Why: Verifies the per-source cap stops scanning once
    /// `MAX_SKILLS_PER_SOURCE` files have been indexed, preventing a 700-file
    /// directory from hanging startup.
    /// What: Generates `MAX_SKILLS_PER_SOURCE * 3` valid skill files in one
    /// directory, loads it, and asserts only the cap's worth get indexed.
    /// Test: `load_caps_skills_per_source`.
    #[test]
    fn load_caps_skills_per_source() {
        let dir = TempDir::new().unwrap();
        let n = MAX_SKILLS_PER_SOURCE * 3;
        for i in 0..n {
            let name = format!("s{i:04}");
            write_skill(dir.path(), &name, "d", &["t"]);
        }
        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert!(
            reg.len() <= MAX_SKILLS_PER_SOURCE,
            "expected <= {} skills loaded, got {} (cap not enforced)",
            MAX_SKILLS_PER_SOURCE,
            reg.len()
        );
        assert!(
            reg.len() >= MAX_SKILLS_PER_SOURCE,
            "expected at least {} skills loaded, got {} (cap aborted too early)",
            MAX_SKILLS_PER_SOURCE,
            reg.len()
        );
    }

    /// Why: Confirms that a directory exceeding `LARGE_DIR_MD_THRESHOLD` with
    /// no top-level TOML manifests (claude-mpm's `~/.claude/skills/` shape)
    /// is detected as external and skipped wholesale by `load`, replacing the
    /// 30+ minute hang with a fast WARN.
    /// What: Creates `LARGE_DIR_MD_THRESHOLD + 5` `.md` files in a flat dir
    /// (no `.toml`), then asserts `load` returns an empty registry. Detection
    /// is also asserted directly via `looks_like_external_skill_dir`.
    /// Test: `load_skips_external_skill_dir`.
    #[test]
    fn load_skips_external_skill_dir() {
        let dir = TempDir::new().unwrap();
        // Many .md files, no .toml manifests.
        for i in 0..(LARGE_DIR_MD_THRESHOLD + 5) {
            let name = format!("ext{i:04}");
            write_skill(dir.path(), &name, "d", &["t"]);
        }
        assert!(
            looks_like_external_skill_dir(dir.path()),
            "expected directory with {}+ .md files and no .toml to be flagged external",
            LARGE_DIR_MD_THRESHOLD
        );

        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        assert!(
            reg.is_empty(),
            "expected external dir to be skipped, got {} skills",
            reg.len()
        );
    }

    /// Why: Guards against false positives — a normal open-mpm skills dir
    /// with a TOML manifest must NOT be flagged as external even if it has
    /// many files.
    /// What: Writes a `skill-sources.toml` plus a few `.md` files; asserts
    /// `looks_like_external_skill_dir` returns false.
    /// Test: `looks_like_external_skill_dir_passes_open_mpm_layout`.
    #[test]
    fn looks_like_external_skill_dir_passes_open_mpm_layout() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("skill-sources.toml"), "# manifest").unwrap();
        for i in 0..10 {
            let name = format!("ok{i}");
            write_skill(dir.path(), &name, "d", &["t"]);
        }
        assert!(
            !looks_like_external_skill_dir(dir.path()),
            "directory with TOML manifest must not be flagged external"
        );
    }

    /// Why: Locks the env-var contract that CTRL relies on to keep its
    /// startup fast — when `OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY=1` is set,
    /// `skill_search_paths` must NOT include `~/.claude/skills/` or
    /// `~/.open-mpm/skills/`.
    /// What: Sets the env var, calls `skill_search_paths`, asserts the
    /// returned list contains only the project-local + bundled paths.
    /// Test: `skill_search_paths_respects_project_local_only_env`.
    #[test]
    fn skill_search_paths_respects_project_local_only_env() {
        // SAFETY: tests run single-threaded by default; we restore env on exit.
        let prev = std::env::var_os("OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY");
        unsafe {
            std::env::set_var("OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY", "1");
        }
        let paths = skill_search_paths(Path::new("/opt/open-mpm/config"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY", v),
                None => std::env::remove_var("OPEN_MPM_SKILLS_PROJECT_LOCAL_ONLY"),
            }
        }
        assert_eq!(
            paths.len(),
            2,
            "expected only project-local + bundled paths"
        );
        assert_eq!(paths[0], PathBuf::from(".open-mpm/skills"));
        assert_eq!(paths[1], PathBuf::from("/opt/open-mpm/config/skills"));
        assert!(
            !paths
                .iter()
                .any(|p| p.to_string_lossy().contains(".claude")),
            "project-local-only mode must not include .claude paths"
        );
    }

    /// Why: Verifies #197 — a stale index with an unrecognizable schema must
    /// be silently deleted and the registry must continue with fresh defaults
    /// rather than propagating an error that would crash startup.
    /// What: Writes malformed JSON to the index path, calls `merge_index`,
    /// asserts no error is returned, the file is gone, and the registry
    /// retains its scanned defaults.
    /// Test: `merge_index_stale_file_is_deleted_and_noop`.
    #[test]
    fn merge_index_stale_file_is_deleted_and_noop() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "x", "d", &["t"]);
        let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);

        // Write a stale index that cannot deserialize as SkillMeta — the
        // `source_path` field is missing, which is required.
        let index_path = dir.path().join("index.json");
        std::fs::write(
            &index_path,
            r#"{"x": {"name": "x", "missing_required": true}}"#,
        )
        .unwrap();

        // merge_index must succeed (no error), delete the stale file, and
        // leave effectiveness at the fresh-scan default.
        reg.merge_index(&index_path)
            .expect("stale index must not error");
        assert!(
            !index_path.exists(),
            "stale index file must be deleted after failed deserialization"
        );
        let meta = reg.get("x").unwrap();
        assert!(
            (meta.effectiveness_score - 0.5).abs() < 1e-6,
            "effectiveness must remain at default after stale-index discard"
        );
    }

    /// Why: Regression test for #216 — an index.json written by a pre-#216
    /// harness omits the `description` field from `SkillMeta` entries. The
    /// prior code treated a missing `description` as a fatal deserialization
    /// error, deleted the index, and emitted a WARN on every run. Now that
    /// `description` carries `#[serde(default)]`, the stale file deserializes
    /// cleanly and effectiveness scores are restored without any WARN.
    /// What: Writes a versioned index whose entries lack `description`, merges
    /// it, and asserts the persisted effectiveness is restored (not reset to
    /// the 0.5 default) — confirming the file was read rather than deleted.
    /// Test: `merge_index_missing_description_restores_effectiveness`.
    #[test]
    fn merge_index_missing_description_restores_effectiveness() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "x", "some description", &["t"]);
        let index_path = dir.path().join("index.json");

        // Simulate a pre-#216 index.json: SkillMeta entries have no
        // `description` field, but the versioned wrapper and all other fields
        // are present and valid.
        std::fs::write(
            &index_path,
            r#"{
  "schema_version": 1,
  "skills": {
    "x": {
      "name": "x",
      "tags": ["t"],
      "source_path": "/tmp/x.md",
      "effectiveness_score": 0.88,
      "use_count": 3,
      "last_used": "2026-04-01T00:00:00Z"
    }
  }
}"#,
        )
        .unwrap();

        let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        // Ensure fresh-scan default before merge.
        let meta = reg.get("x").unwrap();
        assert!((meta.effectiveness_score - 0.5).abs() < 1e-6);

        // Merge must succeed and restore persisted fields — no WARN, no delete.
        reg.merge_index(&index_path)
            .expect("index without description must merge cleanly");
        assert!(
            index_path.exists(),
            "index must NOT be deleted when description is merely absent (has serde default)"
        );
        let meta = reg.get("x").unwrap();
        assert!(
            (meta.effectiveness_score - 0.88).abs() < 1e-4,
            "persisted effectiveness_score must be restored; got {}",
            meta.effectiveness_score
        );
        assert_eq!(meta.use_count, 3);
        assert_eq!(meta.last_used.as_deref(), Some("2026-04-01T00:00:00Z"));
    }

    /// Why: #483 — the registry must expose a free-text `search` that ranks
    /// skills via the BM25 index built during `load`. Verifies the delegation
    /// is wired and that an `empty()` registry (no index) returns nothing.
    /// What: Builds a registry over a dir with two skills, searches for a term
    /// unique to one, and asserts it ranks first; then checks `empty()`.
    /// Test: `registry_search_delegates_to_bm25_index`.
    #[test]
    fn registry_search_delegates_to_bm25_index() {
        let dir = TempDir::new().unwrap();
        write_skill(
            dir.path(),
            "web-search",
            "search the web with brave",
            &["web"],
        );
        write_skill(
            dir.path(),
            "rust-async",
            "tokio runtime patterns",
            &["rust"],
        );

        let reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let hits = reg.search("how do I search the web", 3);
        assert!(!hits.is_empty(), "expected a BM25 hit");
        assert_eq!(hits[0], "web-search", "got {hits:?}");

        // An empty registry has no BM25 index → search returns empty.
        assert!(SkillRegistry::empty().search("anything", 3).is_empty());
    }

    /// Why: A missing index file is the first-run baseline and must be a
    /// no-op rather than an error so startup never breaks on a clean install.
    #[test]
    fn merge_index_missing_file_is_noop() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "x", "d", &["t"]);
        let mut reg = SkillRegistry::load(&[dir.path().to_path_buf()]);
        let nonexistent = dir.path().join("does-not-exist.json");
        reg.merge_index(&nonexistent).expect("missing file ok");
    }
}
