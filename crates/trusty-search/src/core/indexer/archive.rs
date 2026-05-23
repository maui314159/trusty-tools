//! Archive downranking signals (issue #75).
//!
//! Why: search results that come from archived / deprecated / legacy code paths
//! pollute the top-k for queries that should surface live code. Rather than
//! silently filtering them (which would hide real history), we apply a
//! multiplicative score penalty so they sink in ranking unless nothing better
//! matches. Operators / users still see them — labelled with `archive_reason`
//! so they understand why a hit was demoted.
//! What: a small set of pure classifiers (path keywords, code annotations) and
//! one filesystem-touching classifier (explicit `.archived` / `DEPRECATED`
//! marker files in the chunk's directory, with a per-directory cache) plus
//! one mtime-based "stale" classifier. The combined multiplier stacks
//! multiplicatively with a floor of `MIN_MULTIPLIER`.
//! Test: see `tests` submodule for each classifier in isolation and the
//! `archive_downranking_*` integration tests in `indexer::tests`.
//!
//! The order matters for `archive_reason`: explicit strong signals (path,
//! annotation, marker) win over the lighter mtime signal — the operator most
//! cares about the strongest reason.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Strong-signal penalty multiplier applied when an archive signal matches
/// (path keyword, code annotation, or explicit marker file).
pub(crate) const STRONG_PENALTY: f32 = 0.3;

/// Lighter penalty multiplier applied when the only signal is git mtime
/// staleness (file unmodified for >12 months). Only applied when no strong
/// signal already demoted the chunk.
pub(crate) const STALE_PENALTY: f32 = 0.7;

/// Floor on the combined multiplier — keeps two stacked penalties from
/// crushing a chunk to effectively zero, which would hide it from the top-k
/// even when nothing better is available.
pub(crate) const MIN_MULTIPLIER: f32 = 0.1;

/// Twelve-month staleness threshold (~365 days), used by the mtime classifier.
const STALE_THRESHOLD: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// Substrings (lowercased) in a file path that mark the path as archive/legacy.
const ARCHIVE_PATH_KEYWORDS: &[&str] = &[
    "archive",
    "deprecated",
    "legacy",
    "/old/",
    "backup",
    "obsolete",
    "unused",
];

/// Code-annotation patterns whose presence in chunk content marks the chunk as
/// deprecated. Case-sensitive on purpose — these are conventional spellings
/// that rarely vary in real code.
const ARCHIVE_ANNOTATIONS: &[&str] = &[
    "#[deprecated]",
    "@deprecated",
    "// TODO: remove",
    "// FIXME: obsolete",
    "// DEPRECATED",
    "#[allow(deprecated)]",
];

/// Inspect a file path for archive-keyword substrings.
///
/// Why: shared between the post-MMR archive step and unit tests so the rules
/// are inspectable in one place.
/// What: lowercases the path once, then returns the first matching keyword
/// (with a `path:` prefix) so the caller can use it as `archive_reason`.
/// Test: `test_path_keyword_matches_lowercase_substring`.
pub(crate) fn path_keyword_reason(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    for kw in ARCHIVE_PATH_KEYWORDS {
        if lower.contains(kw) {
            let label = kw.trim_matches('/');
            return Some(format!("path:{label}"));
        }
    }
    None
}

/// Inspect chunk content for deprecation annotations.
///
/// Why: source files often carry an explicit `#[deprecated]` / `@deprecated`
/// near an item even when the surrounding path is not archive-flavoured. This
/// lets us demote individual chunks without penalising the whole module.
/// What: scans for any of `ARCHIVE_ANNOTATIONS` as a substring (case-sensitive
/// — the patterns themselves carry the canonical case). Returns the first
/// matching pattern.
/// Test: `test_annotation_matches_deprecated_macro`.
pub(crate) fn annotation_reason(content: &str) -> Option<String> {
    for pat in ARCHIVE_ANNOTATIONS {
        if content.contains(pat) {
            return Some(format!("annotation:{pat}"));
        }
    }
    None
}

/// Marker files whose presence in a chunk's parent directory flags the whole
/// directory as archived. Checked at the immediate parent only — we do not
/// walk ancestors because that would penalise every chunk under a marker'd
/// monorepo root.
const MARKER_FILES: &[&str] = &[".archived", "DEPRECATED"];

/// Per-directory cache for marker-file lookups.
///
/// Why: a single search returns up to `top_k` chunks; many will share the same
/// parent directory. Caching the marker check per directory turns N filesystem
/// hits into K (K = unique parent directories), which on a flat module is
/// typically 1.
/// What: a `HashMap<dir → Option<reason>>` snapped at the start of the
/// downrank pass. Empty when no chunk in the candidate set has a real
/// filesystem path (test indexers under `/tmp/...`).
/// Test: `test_marker_file_check_is_cached_per_directory`.
#[derive(Default)]
pub(crate) struct MarkerCache {
    inner: HashMap<PathBuf, Option<String>>,
}

impl MarkerCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Check whether the chunk's parent directory contains a marker file.
    ///
    /// Why: keeps the per-result loop tight — at most one filesystem stat per
    /// unique parent directory.
    /// What: resolves `<root_path>/<chunk_file>` and probes
    /// `<dir>/<MARKER>` for each marker. Caches the boolean outcome under the
    /// directory path.
    /// Test: covered by `test_marker_file_check_is_cached_per_directory` and
    /// `test_marker_file_detected_in_parent_dir`.
    pub(crate) fn reason_for(&mut self, root_path: &Path, chunk_file: &str) -> Option<String> {
        let abs = root_path.join(chunk_file);
        let dir = abs.parent()?.to_path_buf();
        if let Some(cached) = self.inner.get(&dir) {
            return cached.clone();
        }
        let mut found: Option<String> = None;
        for marker in MARKER_FILES {
            if dir.join(marker).exists() {
                found = Some(format!("marker:{marker}"));
                break;
            }
        }
        self.inner.insert(dir, found.clone());
        found
    }
}

/// Check the file's mtime against the staleness threshold.
///
/// Why: code that has been untouched for over a year is more likely to be a
/// legacy artefact than current implementation. We only apply this signal
/// when no stronger signal already fired (the caller enforces that).
/// What: resolves `<root_path>/<chunk_file>`, reads `metadata().modified()`,
/// and returns `Some("stale:git_mtime")` when it is older than
/// `STALE_THRESHOLD`. Returns `None` for non-existent files or unreadable
/// metadata — best-effort by design.
/// Test: covered by `test_stale_signal_fires_for_old_files` in
/// `indexer::tests`.
pub(crate) fn stale_reason(root_path: &Path, chunk_file: &str) -> Option<String> {
    let abs = root_path.join(chunk_file);
    let meta = std::fs::metadata(&abs).ok()?;
    let mtime = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(mtime).ok()?;
    if age > STALE_THRESHOLD {
        Some("stale:git_mtime".to_string())
    } else {
        None
    }
}

/// Compute the combined archive multiplier and the human-readable reason
/// label for a single chunk.
///
/// Why: keeps the stacking semantics (multiplicative penalties, floor) in one
/// place so the post-MMR loop just calls this once per result.
/// What: scans the four signals in order — path keywords, code annotations,
/// marker files, then mtime staleness (only when no strong signal already
/// fired). Returns `(multiplier, Some(reason))` for the first matching strong
/// signal (so the caller can label it precisely) and stacks penalties from
/// the others multiplicatively, clamped to `MIN_MULTIPLIER`. Returns
/// `(1.0, None)` when nothing matches.
/// Test: `test_penalties_stack_with_floor` and integration coverage.
pub(crate) fn classify(
    root_path: &Path,
    chunk_file: &str,
    chunk_content: &str,
    markers: &mut MarkerCache,
) -> (f32, Option<String>) {
    let mut multiplier = 1.0_f32;
    let mut first_reason: Option<String> = None;
    let mut strong_fired = false;

    if let Some(reason) = path_keyword_reason(chunk_file) {
        multiplier *= STRONG_PENALTY;
        first_reason.get_or_insert(reason);
        strong_fired = true;
    }
    if let Some(reason) = annotation_reason(chunk_content) {
        multiplier *= STRONG_PENALTY;
        first_reason.get_or_insert(reason);
        strong_fired = true;
    }
    if let Some(reason) = markers.reason_for(root_path, chunk_file) {
        multiplier *= STRONG_PENALTY;
        first_reason.get_or_insert(reason);
        strong_fired = true;
    }
    if !strong_fired {
        if let Some(reason) = stale_reason(root_path, chunk_file) {
            multiplier *= STALE_PENALTY;
            first_reason.get_or_insert(reason);
        }
    }

    if multiplier < MIN_MULTIPLIER {
        multiplier = MIN_MULTIPLIER;
    }
    (multiplier, first_reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_keyword_matches_lowercase_substring() {
        // Why: keyword detection must be case-insensitive and substring-based
        // so paths like `src/Legacy/foo.rs` or `crates/archive_old/bar.rs`
        // are both flagged.
        assert_eq!(
            path_keyword_reason("src/Legacy/foo.rs"),
            Some("path:legacy".to_string())
        );
        assert_eq!(
            path_keyword_reason("crates/deprecated/bar.rs"),
            Some("path:deprecated".to_string())
        );
        // The `/old/` keyword carries leading/trailing slashes; the label
        // should be just "old".
        assert_eq!(
            path_keyword_reason("src/old/foo.rs"),
            Some("path:old".to_string())
        );
        assert_eq!(path_keyword_reason("src/main.rs"), None);
    }

    #[test]
    fn test_annotation_matches_deprecated_macro() {
        // Why: chunk content matching is substring-based; even multi-line
        // bodies should fire on the first hit.
        assert_eq!(
            annotation_reason("#[deprecated]\nfn old() {}"),
            Some("annotation:#[deprecated]".to_string())
        );
        assert_eq!(
            annotation_reason("/** @deprecated use new_fn */"),
            Some("annotation:@deprecated".to_string())
        );
        assert_eq!(annotation_reason("fn alive() {}"), None);
    }

    #[test]
    fn test_marker_file_check_is_cached_per_directory() {
        // Why: marker checks touch the filesystem; the cache must collapse
        // repeated lookups under the same parent directory to a single hit.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("legacy_module");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join(".archived"), "").unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.join("b.rs"), "fn b() {}").unwrap();

        let mut cache = MarkerCache::new();
        let r1 = cache.reason_for(tmp.path(), "legacy_module/a.rs");
        let r2 = cache.reason_for(tmp.path(), "legacy_module/b.rs");
        assert_eq!(r1, Some("marker:.archived".to_string()));
        assert_eq!(r2, Some("marker:.archived".to_string()));
        // Both lookups should have produced the same cache entry — one key
        // for the shared parent directory.
        assert_eq!(cache.inner.len(), 1);
    }

    #[test]
    fn test_marker_file_detected_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("retiring");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("DEPRECATED"), "use new_module instead").unwrap();
        std::fs::write(dir.join("foo.rs"), "fn foo() {}").unwrap();
        let mut cache = MarkerCache::new();
        let r = cache.reason_for(tmp.path(), "retiring/foo.rs");
        assert_eq!(r, Some("marker:DEPRECATED".to_string()));
    }

    #[test]
    fn test_penalties_stack_with_floor() {
        // Two strong signals (path + annotation) stack: 0.3 * 0.3 = 0.09,
        // floored to MIN_MULTIPLIER = 0.1. The label takes the first signal
        // encountered (path).
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = MarkerCache::new();
        let (mult, reason) = classify(
            tmp.path(),
            "src/legacy/foo.rs",
            "#[deprecated]\nfn old() {}",
            &mut cache,
        );
        assert!((mult - MIN_MULTIPLIER).abs() < f32::EPSILON);
        assert_eq!(reason, Some("path:legacy".to_string()));
    }

    #[test]
    fn test_clean_chunk_passes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = MarkerCache::new();
        let (mult, reason) = classify(tmp.path(), "src/main.rs", "fn main() {}", &mut cache);
        assert!((mult - 1.0).abs() < f32::EPSILON);
        assert_eq!(reason, None);
    }
}
