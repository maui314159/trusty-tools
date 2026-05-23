//! Git subprocess helpers for branch-aware search (issue #122).
//!
//! Why: when a `SearchQuery` carries `branch: Some(name)` but no explicit
//! `branch_files`, the search pipeline asks git which files diverge between
//! `HEAD` and the merge-base with that branch. We shell out rather than
//! linking libgit2 to keep the dependency surface small and to inherit the
//! caller's `.gitconfig` / safe.directory settings unchanged.
//! What: a single best-effort helper that runs `git merge-base HEAD <branch>`
//! followed by `git diff --name-only <base>..HEAD`. Any failure (non-git
//! workdir, unknown branch, detached HEAD, missing binary) returns `None`
//! with a `tracing::warn!` — the caller falls back to no boost rather than
//! failing the search.
//! Test: covered by unit tests in this module (no-git case) and the
//! integration tests in `core::indexer::tests` that exercise the explicit
//! `branch_files` path.

use std::path::Path;
use std::process::Command;

/// Compute the list of files modified on `branch` relative to the merge-base
/// with `HEAD`, by shelling out to `git`. Paths are returned exactly as `git
/// diff --name-only` prints them (forward-slash separated, relative to the
/// repo root).
///
/// Returns `None` on any failure — caller treats this as "no boost".
pub fn resolve_branch_files(root_path: &Path, branch: &str) -> Option<Vec<String>> {
    // 1) Find the merge-base between HEAD and the named branch.
    let base = Command::new("git")
        .args(["merge-base", "HEAD", branch])
        .current_dir(root_path)
        .output()
        .ok()?;
    if !base.status.success() {
        tracing::warn!(
            "branch file resolution failed for branch '{}': git merge-base exited {:?}",
            branch,
            base.status.code()
        );
        return None;
    }
    let base_sha = std::str::from_utf8(&base.stdout).ok()?.trim().to_owned();
    if base_sha.is_empty() {
        tracing::warn!(
            "branch file resolution failed for branch '{}': empty merge-base",
            branch
        );
        return None;
    }

    // 2) List files changed between the merge-base and HEAD.
    let diff = Command::new("git")
        .args(["diff", "--name-only", &format!("{}..HEAD", base_sha)])
        .current_dir(root_path)
        .output()
        .ok()?;
    if !diff.status.success() {
        tracing::warn!(
            "branch file resolution failed for branch '{}': git diff exited {:?}",
            branch,
            diff.status.code()
        );
        return None;
    }

    let body = std::str::from_utf8(&diff.stdout).ok()?;
    Some(
        body.lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Normalize a path string for comparison: strip a leading `./` so that
/// branch_files entries like `./src/foo.rs` and chunk files like
/// `src/foo.rs` compare equal.
pub fn normalize_path(p: &str) -> &str {
    p.strip_prefix("./").unwrap_or(p)
}

/// Read the current `HEAD` SHA for the repo rooted at `root_path` (issue #75).
///
/// Why: the search response advertises `results_may_be_stale` so callers know
/// when the index was built against an older commit than the working tree's
/// current HEAD. The check is O(1) git read — `git rev-parse HEAD`.
/// What: returns `Some(sha)` (40-char hex) on success, `None` for non-git
/// directories, detached HEAD without commits, missing `git` binary, or any
/// other best-effort failure. Never panics; never blocks the search hot path
/// on slow git ops (this is the only call we make).
/// Test: `test_head_sha_is_none_outside_git_repo`.
pub fn head_sha(root_path: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = std::str::from_utf8(&out.stdout).ok()?.trim().to_owned();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_branch_files_returns_none_when_not_a_repo() {
        // Why: helper must be best-effort. A non-git directory must produce
        // `None`, not a panic.
        let tmp = tempfile::tempdir().unwrap();
        // git merge-base will fail with non-zero exit in a non-repo dir.
        let result = resolve_branch_files(tmp.path(), "nope");
        assert!(result.is_none(), "expected None outside a git repo");
    }

    #[test]
    fn test_head_sha_is_none_outside_git_repo() {
        // Why: `head_sha` must be best-effort. A non-git directory must
        // produce `None`, not a panic.
        let tmp = tempfile::tempdir().unwrap();
        assert!(head_sha(tmp.path()).is_none());
    }

    #[test]
    fn test_normalize_path_strips_leading_dot_slash() {
        assert_eq!(normalize_path("./src/foo.rs"), "src/foo.rs");
        assert_eq!(normalize_path("src/foo.rs"), "src/foo.rs");
        assert_eq!(normalize_path(""), "");
    }
}
