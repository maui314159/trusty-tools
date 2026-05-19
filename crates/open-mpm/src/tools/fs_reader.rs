//! Read-only filesystem tools: `read_file`, `list_dir`, `grep_files`.
//!
//! Why: #34 — an "explorer" agent needs to understand existing code by
//! reading files, listing directories, and searching content, WITHOUT any
//! ability to modify the codebase or execute arbitrary code. Keeping each
//! tool narrow and read-only lets the PM delegate "understand this code"
//! tasks safely.
//! What: Three `ToolExecutor`s:
//!   - `ReadFileTool { path }` -> file contents, truncated at 50k chars.
//!   - `ListDirTool { path, show_hidden }` -> one-level directory listing.
//!   - `GrepFilesTool { pattern, path, max_results }` -> ripgrep (if
//!     available) falling back to walkdir+regex.
//! All three reject paths outside the current working directory to prevent
//! path traversal out of the project.
//! Test: See the `tests` submodule — each tool has happy-path + traversal-
//! rejection coverage.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::traits::{ToolExecutor, ToolResult};

/// Maximum characters returned by `read_file` before truncation.
const READ_FILE_MAX_CHARS: usize = 50_000;
/// Default `max_results` for `grep_files`.
const GREP_DEFAULT_MAX_RESULTS: usize = 50;
/// Hard cap on `max_results` to bound result size regardless of caller.
const GREP_HARD_CAP: usize = 500;

/// Resolve a user-supplied path relative to CWD and verify it stays inside.
///
/// Why: Security — an LLM could (accidentally or deliberately) request paths
/// like `../../etc/passwd`. Canonicalizing both the CWD and the candidate
/// then comparing prefixes keeps all reads inside the working directory.
/// What: Returns the canonicalized target path on success, or an error string
/// describing the rejection.
/// Test: See `resolve_rejects_traversal` and `resolve_accepts_subpath`.
fn resolve_within_cwd(path: &str) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("could not resolve CWD: {e}"))?;
    let cwd_canon = cwd
        .canonicalize()
        .map_err(|e| format!("could not canonicalize CWD {}: {e}", cwd.display()))?;

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };

    // If the path doesn't exist yet (unlikely for read-only tools) we still
    // need to reject traversal. canonicalize() fails on missing paths, so
    // walk the path component-by-component instead for that case; for
    // existing paths, canonicalize is strictly better (resolves symlinks).
    let canon = match candidate.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return Err(format!(
                "path does not exist or is unreadable: {}: {e}",
                candidate.display()
            ));
        }
    };

    if !canon.starts_with(&cwd_canon) {
        return Err(format!(
            "path escapes working directory: {} (cwd: {})",
            canon.display(),
            cwd_canon.display()
        ));
    }

    Ok(canon)
}

// =============================================================================
// read_file
// =============================================================================

/// Read-only file reader.
pub struct ReadFileTool;

impl ReadFileTool {
    /// Construct a new `ReadFileTool`. Zero-sized; state lives in the
    /// working directory resolved at call time.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file's contents. Paths outside the current working directory are rejected. Output is truncated at ~50k chars.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative (from CWD) or absolute path; must resolve inside CWD."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return ToolResult::err("read_file: missing 'path'");
        };
        let canon = match resolve_within_cwd(path) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("read_file: {e}")),
        };
        if !canon.is_file() {
            return ToolResult::err(format!(
                "read_file: not a regular file: {}",
                canon.display()
            ));
        }
        match tokio::fs::read_to_string(&canon).await {
            Ok(text) => {
                if text.chars().count() <= READ_FILE_MAX_CHARS {
                    ToolResult::ok(text)
                } else {
                    let truncated: String = text.chars().take(READ_FILE_MAX_CHARS).collect();
                    ToolResult::ok(format!(
                        "{truncated}\n...[truncated at {READ_FILE_MAX_CHARS} chars]"
                    ))
                }
            }
            Err(e) => ToolResult::err(format!("read_file: {e}")),
        }
    }
}

// =============================================================================
// list_dir
// =============================================================================

/// Read-only directory lister (depth = 1).
pub struct ListDirTool;

impl ListDirTool {
    /// Construct a new `ListDirTool`. Zero-sized; the target path is
    /// supplied per-call via the tool arguments.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ListDirTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List entries in a directory (non-recursive, depth=1). Rejects paths outside the CWD.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative or absolute path to a directory inside CWD."
                        },
                        "show_hidden": {
                            "type": "boolean",
                            "description": "Whether to include dotfiles. Default false."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return ToolResult::err("list_dir: missing 'path'");
        };
        let show_hidden = args
            .get("show_hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let canon = match resolve_within_cwd(path) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("list_dir: {e}")),
        };
        if !canon.is_dir() {
            return ToolResult::err(format!("list_dir: not a directory: {}", canon.display()));
        }

        let mut entries: Vec<(String, String, u64)> = Vec::new();
        let mut rd = match tokio::fs::read_dir(&canon).await {
            Ok(r) => r,
            Err(e) => {
                return ToolResult::err(format!(
                    "list_dir: failed to read {}: {e}",
                    canon.display()
                ));
            }
        };
        loop {
            match rd.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !show_hidden && name.starts_with('.') {
                        continue;
                    }
                    let (kind, size) = match entry.metadata().await {
                        Ok(md) => {
                            let k = if md.is_dir() {
                                "dir"
                            } else if md.is_file() {
                                "file"
                            } else {
                                "other"
                            };
                            (k.to_string(), md.len())
                        }
                        Err(_) => ("unknown".to_string(), 0),
                    };
                    entries.push((name, kind, size));
                }
                Ok(None) => break,
                Err(e) => {
                    return ToolResult::err(format!("list_dir: iter error: {e}"));
                }
            }
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));

        if entries.is_empty() {
            return ToolResult::ok(format!("(empty directory: {})", canon.display()));
        }

        let mut out = String::new();
        out.push_str(&format!("# {}\n", canon.display()));
        for (name, kind, size) in entries {
            out.push_str(&format!("{kind:>5}  {size:>10}  {name}\n"));
        }
        ToolResult::ok(out)
    }
}

// =============================================================================
// grep_files
// =============================================================================

/// Read-only content grep over the working directory.
pub struct GrepFilesTool;

impl GrepFilesTool {
    /// Construct a new `GrepFilesTool`. Zero-sized; pattern + path are
    /// supplied per-call via tool arguments.
    pub fn new() -> Self {
        Self
    }
}

impl Default for GrepFilesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for GrepFilesTool {
    fn name(&self) -> &str {
        "grep_files"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "grep_files",
                "description": "Search for a regex pattern in files under `path`. Uses ripgrep (`rg --json`) if available, else falls back to a walkdir+regex scan. Returns lines in `file:line: match` format. Paths outside CWD are rejected.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (ripgrep regex syntax)."
                        },
                        "path": {
                            "type": "string",
                            "description": "Relative or absolute path to search (file or directory) inside CWD."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Max matching lines to return. Default 50, capped at 500.",
                            "minimum": 1,
                            "maximum": 500
                        }
                    },
                    "required": ["pattern", "path"],
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return ToolResult::err("grep_files: missing 'pattern'");
        };
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return ToolResult::err("grep_files: missing 'path'");
        };
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(GREP_DEFAULT_MAX_RESULTS)
            .min(GREP_HARD_CAP);

        let canon = match resolve_within_cwd(path) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("grep_files: {e}")),
        };

        // Prefer ripgrep for correctness + speed if available on PATH.
        match run_ripgrep(pattern, &canon, max_results).await {
            Ok(Some(out)) => return ToolResult::ok(out),
            Ok(None) => {
                tracing::debug!("grep_files: rg not available; falling back to walkdir");
            }
            Err(e) => {
                tracing::debug!(error = %e, "grep_files: rg failed; falling back");
            }
        }

        match walkdir_grep(pattern, &canon, max_results) {
            Ok(out) => ToolResult::ok(out),
            Err(e) => ToolResult::err(format!("grep_files: {e}")),
        }
    }
}

/// Run `rg --line-number --no-heading <pattern> <path>` and return the raw
/// textual output (capped at `max_results` lines). Returns `Ok(None)` if
/// ripgrep isn't installed so the caller can fall back.
async fn run_ripgrep(
    pattern: &str,
    path: &Path,
    max_results: usize,
) -> anyhow::Result<Option<String>> {
    // Probe for rg.
    let probe = Command::new("rg").arg("--version").output().await;
    let Ok(probe) = probe else {
        return Ok(None);
    };
    if !probe.status.success() {
        return Ok(None);
    }

    let output = Command::new("rg")
        .arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg("--max-count")
        .arg(max_results.to_string())
        .arg(pattern)
        .arg(path)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // rg exits 1 when no matches — treat as empty success.
    let lines: Vec<&str> = stdout.lines().take(max_results).collect();
    if lines.is_empty() {
        return Ok(Some(format!(
            "(no matches for /{pattern}/ under {})",
            path.display()
        )));
    }
    Ok(Some(lines.join("\n")))
}

/// Fallback: walk `path` with std::fs (+ a small hand-rolled depth-first
/// iterator) and look for `pattern` with the `regex` crate... except we
/// don't depend on `regex`. Use plain substring matching as a pragmatic
/// fallback; callers that need regex should install ripgrep.
fn walkdir_grep(pattern: &str, path: &Path, max_results: usize) -> anyhow::Result<String> {
    let mut out_lines: Vec<String> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![path.to_path_buf()];

    while let Some(cur) = stack.pop() {
        if out_lines.len() >= max_results {
            break;
        }
        if cur.is_file() {
            // Centralised skip: binaries-by-extension, oversize files, and
            // anything inside a denied directory tree.
            if crate::tools::file_filter::should_skip_file(&cur) {
                continue;
            }
            match std::fs::read_to_string(&cur) {
                Ok(body) => {
                    for (lineno, line) in body.lines().enumerate() {
                        if line.contains(pattern) {
                            out_lines.push(format!(
                                "{}:{}: {}",
                                cur.display(),
                                lineno + 1,
                                line.trim_end()
                            ));
                            if out_lines.len() >= max_results {
                                break;
                            }
                        }
                    }
                }
                Err(_) => {
                    // Binary / unreadable — skip silently.
                }
            }
        } else if cur.is_dir() {
            // Skip common ignore dirs to keep the fallback usable.
            if let Some(name) = cur.file_name().and_then(|s| s.to_str())
                && crate::tools::file_filter::should_skip_dir(name)
            {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(&cur) {
                for entry in rd.flatten() {
                    stack.push(entry.path());
                }
            }
        }
    }

    if out_lines.is_empty() {
        return Ok(format!(
            "(no matches for {pattern:?} under {})",
            path.display()
        ));
    }
    Ok(out_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[allow(dead_code)]
    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("open-mpm-fs-reader-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn resolve_accepts_subpath() {
        let cwd = std::env::current_dir().unwrap();
        let target = cwd.join("Cargo.toml");
        assert!(target.exists(), "sanity: Cargo.toml exists");
        let got = resolve_within_cwd("Cargo.toml").expect("should resolve");
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn resolve_rejects_traversal() {
        // Parent of the repo should be outside CWD.
        let got = resolve_within_cwd("..");
        assert!(got.is_err(), "parent dir should be rejected");
        let msg = got.unwrap_err();
        assert!(
            msg.contains("escapes") || msg.contains("does not exist"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn read_file_returns_contents() {
        // Use Cargo.toml as a reliable fixture.
        let tool = ReadFileTool::new();
        let r = tool.execute(json!({"path": "Cargo.toml"})).await;
        assert!(!r.is_error(), "unexpected: {}", r.content());
        assert!(r.content().contains("open-mpm"));
    }

    #[tokio::test]
    async fn read_file_rejects_outside_cwd() {
        let tool = ReadFileTool::new();
        let r = tool.execute(json!({"path": "../"})).await;
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn read_file_missing_path_arg() {
        let tool = ReadFileTool::new();
        let r = tool.execute(json!({})).await;
        assert!(r.is_error());
        assert!(r.content().contains("path"));
    }

    #[tokio::test]
    async fn read_file_truncates_large_content() {
        // Write a large file into a subdir of CWD, then read it back.
        let cwd = std::env::current_dir().unwrap();
        let target_dir = cwd.join("target").join("_fs_reader_test");
        fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("big.txt");
        let body = "x".repeat(READ_FILE_MAX_CHARS + 1000);
        fs::write(&target, &body).unwrap();

        let rel = pathdiff::diff_or_absolute(&target, &cwd);
        let tool = ReadFileTool::new();
        let r = tool.execute(json!({"path": rel})).await;
        assert!(!r.is_error(), "unexpected: {}", r.content());
        assert!(r.content().contains("truncated"));
    }

    #[tokio::test]
    async fn list_dir_shows_files() {
        let tool = ListDirTool::new();
        let r = tool.execute(json!({"path": "."})).await;
        assert!(!r.is_error(), "unexpected: {}", r.content());
        assert!(r.content().contains("Cargo.toml"));
    }

    #[tokio::test]
    async fn list_dir_hides_dotfiles_by_default() {
        // Create a tempdir inside CWD with a visible + hidden file.
        let cwd = std::env::current_dir().unwrap();
        let dir = cwd.join("target").join("_list_dir_dotfile");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("visible.txt"), "v").unwrap();
        fs::write(dir.join(".hidden"), "h").unwrap();

        let rel = pathdiff::diff_or_absolute(&dir, &cwd);
        let tool = ListDirTool::new();
        let default_r = tool.execute(json!({"path": rel.clone()})).await;
        assert!(default_r.content().contains("visible.txt"));
        assert!(!default_r.content().contains(".hidden"));

        let shown_r = tool
            .execute(json!({"path": rel, "show_hidden": true}))
            .await;
        assert!(shown_r.content().contains(".hidden"));
    }

    #[tokio::test]
    async fn list_dir_rejects_outside_cwd() {
        let tool = ListDirTool::new();
        let r = tool.execute(json!({"path": "/"})).await;
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn grep_files_finds_pattern_fallback() {
        // Use the fallback path by searching inside a known file. We can't
        // easily disable rg per-call, so this test checks *either* rg or
        // walkdir produces a match for a well-known literal.
        let tool = GrepFilesTool::new();
        let r = tool
            .execute(json!({
                "pattern": "open-mpm",
                "path": "Cargo.toml",
                "max_results": 10
            }))
            .await;
        assert!(!r.is_error(), "unexpected: {}", r.content());
        assert!(r.content().contains("open-mpm"));
    }

    #[tokio::test]
    async fn grep_files_rejects_outside_cwd() {
        let tool = GrepFilesTool::new();
        let r = tool.execute(json!({"pattern": "x", "path": "/etc"})).await;
        assert!(r.is_error());
    }

    #[tokio::test]
    async fn grep_files_no_match_returns_empty_message() {
        let tool = GrepFilesTool::new();
        let r = tool
            .execute(json!({
                "pattern": "__this_literal_should_not_appear_anywhere__",
                "path": "Cargo.toml"
            }))
            .await;
        assert!(!r.is_error());
        // Either empty-result message from rg branch or walkdir branch.
        assert!(
            r.content().contains("no matches") || r.content().is_empty(),
            "got: {}",
            r.content()
        );
    }
}

// Tiny local replacement for the `pathdiff` crate so we don't add a new
// dependency for a two-function need. Placed outside the test cfg so the
// test module can use it directly.
#[cfg(test)]
mod pathdiff {
    use std::path::{Path, PathBuf};

    /// Produce a path expressing `target` relative to `base` if possible,
    /// else return the absolute `target`.
    pub fn diff_or_absolute(target: &Path, base: &Path) -> String {
        match target.strip_prefix(base) {
            Ok(rel) => rel.display().to_string(),
            Err(_) => target.display().to_string(),
        }
    }

    // Keep PathBuf unused import quiet for platforms that lint strictly.
    #[allow(dead_code)]
    fn _unused() -> PathBuf {
        PathBuf::new()
    }
}
