//! Workflow JSON config parsing.
//!
//! Why: Workflows are declarative so they can be added without recompiling
//! the engine. JSON chosen for easy hand-authoring and tooling.
//! What: `WorkflowDef` / `PhaseDef` structs with `serde(Deserialize)`.
//! Test: Round-trip a minimal JSON doc through `serde_json::from_str`.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Defense-in-depth path join that refuses to escape `base` (#114).
///
/// Why: `validate_file_path` is a lexical check on assignments.json input, but
/// any future caller constructing a path from external data (or a malicious
/// agent that bypasses validation) could still write outside `out_dir`. This
/// helper resolves the candidate path lexically (without touching the
/// filesystem for the relative part), then asserts the result is contained
/// within the canonicalized `base`. Returns `None` for any escape attempt
/// (parent-traversal beyond root, absolute paths, paths that resolve outside
/// base) so callers can log+skip rather than crash.
/// What: Strips a leading `/`, walks the candidate components manually
/// resolving `..` segments against an in-memory stack, then joins onto the
/// canonicalized base and verifies the final path starts with the base.
/// Filesystem canonicalization runs only against `base` (which must exist);
/// the candidate path itself may not yet exist on disk.
/// Test: `safe_join_*` unit tests in this module.
pub fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    // Strip any leading slash so "/etc/passwd" and "etc/passwd" both produce
    // a candidate rooted at `base` rather than escaping to the filesystem
    // root before we get a chance to inspect it.
    let rel = rel.trim_start_matches('/');

    // Canonicalize the base — this requires `base` to exist. Without it we
    // cannot safely compare prefixes against symlink-rewritten ancestors.
    let canonical_base = base.canonicalize().ok()?;

    // Seed the path stack with the canonical base components, then apply
    // each `rel` component lexically. `..` pops the stack but is FORBIDDEN
    // from popping below the base depth — that would escape `base`. We do
    // NOT canonicalize the rel portion (the file may not yet exist on disk).
    let mut parts: Vec<std::ffi::OsString> = canonical_base
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            _ => None,
        })
        .collect();
    let base_depth = parts.len();
    for c in Path::new(rel).components() {
        match c {
            Component::ParentDir => {
                // Refuse to pop above the base prefix — any `..` that would
                // shrink `parts` below `base_depth` escapes `base`.
                if parts.len() <= base_depth {
                    return None;
                }
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_os_string()),
            // Treat any RootDir embedded in `rel` (post-strip) as a request
            // to reset to root — that's an explicit escape attempt.
            Component::RootDir => return None,
            // Prefix (Windows drive letters) and CurDir are no-ops here.
            _ => {}
        }
    }

    // Reassemble as an absolute path. On Unix we need a leading `/`; on
    // Windows the prefix component would normally provide the drive, but
    // open-mpm targets Unix so we use Path::new("/") as the root anchor.
    let mut final_path = PathBuf::from("/");
    for p in &parts {
        final_path.push(p);
    }

    if final_path.starts_with(&canonical_base) {
        Some(final_path)
    } else {
        None
    }
}

/// Top-level workflow definition loaded from `config/workflows/<name>.json`.
///
/// # Intent
/// Workflows are declarative so the engine can run new pipelines without code
/// changes. The JSON shape is versioned implicitly by the set of optional
/// fields — older configs keep working as new fields are added with
/// `#[serde(default)]`.
///
/// Test: `config_from_json_parses_minimal` round-trips a minimal config.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub phases: Vec<PhaseDef>,
    /// #76: Optional auto-push config. When present AND `enabled=true`, the
    /// engine stages `out_dir`, bumps Cargo.toml version, commits, and pushes
    /// after a successful workflow run.
    #[serde(default)]
    pub auto_push: Option<AutoPushConfig>,
    /// #84: Optional automatic GitHub issue lifecycle management. When present
    /// AND `enabled=true`, the engine creates a tracking issue at workflow
    /// start, comments after each phase, and closes the issue on success.
    #[serde(default)]
    pub ticket_management: Option<TicketManagementConfig>,
}

/// One phase in a workflow — a single agent invocation with its context
/// template and optional parallel/produces-files modifiers.
///
/// # Intent
/// Each phase is addressable by `name` so later phases can interpolate prior
/// outputs (`{{<phase_name>}}`). Keeping phase state inside the JSON (rather
/// than code) lets workflows be shared and reproduced verbatim.
///
/// Test: `config_from_json_parses_minimal`, `parallel_subtasks_parse_from_json`.
#[derive(Debug, Clone, Deserialize)]
pub struct PhaseDef {
    /// Phase id, e.g. `"research"`. Used as the key for context outputs.
    pub name: String,
    /// Agent name used for this phase (must resolve to a TOML config file).
    pub agent: String,
    /// Optional model override for this phase (takes precedence over the
    /// agent's own `agent.model`).
    #[serde(default, alias = "model_override")]
    pub model: Option<String>,
    /// Template string expanded against `WorkflowContext` before being sent
    /// to the agent. Supports `{{task}}`, `{{out_dir}}`, and
    /// `{{<phase_name>}}` substitutions.
    pub context_template: String,
    /// #64: When true, the engine scans this phase's output for
    /// `## File: <path>` sections and writes them under `out_dir` BEFORE
    /// advancing to the next phase. This is what lets the QA phase run
    /// pytest against the code the previous phase just produced.
    /// Defaults to `None` (treated as false) to preserve existing workflows.
    #[serde(default)]
    pub produces_files: Option<bool>,
    /// #73: Optional list of parallel subtasks. When present, one agent per
    /// subtask is spawned concurrently. When absent, single-agent behavior
    /// is unchanged.
    #[serde(default)]
    pub parallel_subtasks: Option<Vec<ParallelSubtask>>,
    /// #74: When true and parallel_subtasks is present, each sub-agent runs
    /// in a dedicated git worktree. Default false.
    #[serde(default)]
    pub worktree_protection: Option<bool>,
    /// #82: When true, the engine skips this phase entirely (no agent run,
    /// no output recorded, no file extraction). Lets workflows declare
    /// optional phases (e.g. `docs`) that are off by default and can be
    /// enabled by flipping `skip` to `false` without editing the phase list.
    /// Defaults to `None` (treated as false) for backwards compatibility.
    #[serde(default)]
    pub skip: Option<bool>,
    /// Skills-first: explicit skill names (or `["auto"]`) to inject for this
    /// phase via `SkillsLoader`. When absent, falls back to the agent TOML's
    /// `system_prompt.skills` field. `["auto"]` triggers language/framework
    /// auto-detection from the project directory and task text.
    /// Defaults to `None` so existing workflow JSON files are unaffected.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Override AST-native tool surface for this phase.
    ///
    /// Why: Bake-off data shows AST-native is materially cheaper for research
    /// and plan phases (-55-60%) but slightly more expensive for code/qa
    /// phases (+20%) with fewer tests generated. Per-phase override lets
    /// workflows opt research+plan into AST-native while keeping code+qa on
    /// the traditional toolchain — a hybrid that yields ~14% total cost
    /// reduction with no quality loss.
    /// What: `None` = inherit from the global `--ast-native` flag.
    /// `Some(true)` = force-enable for this phase. `Some(false)` =
    /// force-disable for this phase.
    /// Test: `phase_ast_native_parses_from_json`,
    /// `phase_ast_native_defaults_to_none` in this module.
    #[serde(default)]
    pub ast_native: Option<bool>,
}

/// #73: A single parallel subtask description for a phase.
///
/// Why: Lets a phase dispatch multiple specialized sub-agent runs concurrently
/// (e.g. "backend" + "frontend" + "tests") so independent workstreams overlap.
/// What: `label` identifies the subtask (used for worktree dir naming and
/// merge reports); `task_suffix` is appended to the rendered phase template.
/// Test: `parallel_subtasks_parse_from_json` in workflow config tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelSubtask {
    /// Short identifier used for worktree naming and merge reports.
    pub label: String,
    /// Appended to the phase's rendered task text for this sub-agent.
    pub task_suffix: String,
}

/// #76: Auto-push config — commits and pushes workflow output on success.
///
/// Why: For workflows that produce release-worthy artifacts, pushing to a
/// shared remote is the final step. Centralizing version bump + commit + push
/// keeps it opt-in and scriptable.
/// What: `enabled=false` by default (must be explicitly turned on).
/// `version_bump` is "patch" | "minor" | "none". `commit_message_template`
/// accepts `{{workflow}}`, `{{build}}`, `{{task_preview}}`, `{{version}}`
/// placeholders.
/// Test: `auto_push_config_defaults` round-trips a minimal JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoPushConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_version_bump")]
    pub version_bump: String,
    #[serde(default = "default_commit_template")]
    pub commit_message_template: String,
    #[serde(default = "default_remote")]
    pub push_remote: String,
    #[serde(default = "default_branch")]
    pub push_branch: String,
}

fn default_version_bump() -> String {
    "patch".to_string()
}

fn default_commit_template() -> String {
    "feat(workflow): {{workflow}} build {{build}} — {{task_preview}}".to_string()
}

fn default_remote() -> String {
    "origin".to_string()
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_true() -> bool {
    true
}

/// #84: Configuration for automatic GitHub issue lifecycle management.
///
/// Why: Every workflow run creates and manages a GitHub issue automatically,
/// providing full traceability without manual ticket creation. `enabled=false`
/// by default so the feature is strictly opt-in and existing workflows that
/// omit the block keep running unchanged.
/// What: Repo/assignee/milestone/labels drive issue creation; `auto_relate`,
/// `phase_comments`, and `close_on_success` control lifecycle hooks. Every
/// lifecycle call is a no-op when `enabled=false`, so shipping the struct as
/// `Default` is safe.
/// Test: `ticket_management_config_defaults` in this module;
/// `ticket_management_config_deserializes` in `tickets.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TicketManagementConfig {
    /// Enable/disable ticket management. Default: false (opt-in).
    #[serde(default)]
    pub enabled: bool,

    /// GitHub repo in owner/name format (e.g. "bobmatnyc/open-mpm").
    #[serde(default)]
    pub repo: String,

    /// GitHub username to assign issues to.
    #[serde(default)]
    pub assignee: String,

    /// Milestone title to assign issues to.
    #[serde(default)]
    pub milestone: String,

    /// Labels to apply to each run issue.
    #[serde(default)]
    pub labels: Vec<String>,

    /// Whether to search for related issues and cross-link them.
    #[serde(default = "default_true")]
    pub auto_relate: bool,

    /// Whether to add a comment after each phase completes.
    #[serde(default = "default_true")]
    pub phase_comments: bool,

    /// Whether to close the issue on successful workflow completion.
    #[serde(default = "default_true")]
    pub close_on_success: bool,
}

/// #88: One file assignment within a wave, produced by the plan-agent.
///
/// Why: The wave loop runs one code-agent per file; each agent needs its own
/// scoped instruction (path, purpose, line budget, dependency list) plus a
/// pointer to its stub so it can implement the signatures exactly as planned.
/// What: Deserializes from the `files[]` array inside each wave entry of
/// `assignments.json`.
/// Test: `assignments_load_parses_valid_json` round-trips a two-wave document.
#[derive(Debug, Deserialize, Clone)]
pub struct FileAssignment {
    /// Destination path within `out_dir` (e.g. `src/foo.py`). The code-agent
    /// may ONLY write to this path.
    pub path: String,
    /// Stub file name (relative to `out_dir/stubs/`) the code-agent reads to
    /// learn the signatures it must implement verbatim. `None` when the
    /// plan-agent emits `"stub": null` (e.g. for `__init__.py` files that
    /// need no implementation scaffold).
    pub stub: Option<String>,
    /// One-line purpose the agent gets in its prompt so it knows the intent
    /// without having to re-derive it from the stub.
    pub purpose: String,
    /// Relative paths of files this one depends on — the agent reads them for
    /// context before writing its own file. Defaults to empty.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Optional maximum line budget; the prompt advertises this so agents
    /// self-constrain. `None` falls back to a default (300) at the caller.
    pub max_lines: Option<u32>,
}

/// #88: One wave in the topological decomposition.
///
/// Why: Waves group files that can be implemented once all earlier waves are
/// done; within a wave files are still run sequentially so cost stays bounded
/// and conflicts are impossible.
/// What: `wave` is the 1-indexed ordinal, `files` is the ordered list of
/// assignments for that wave.
/// Test: Shares the `assignments_load_parses_valid_json` fixture.
#[derive(Debug, Deserialize, Clone)]
pub struct WaveDef {
    pub wave: u32,
    pub files: Vec<FileAssignment>,
}

/// #88: Top-level shape of `out_dir/assignments.json`.
///
/// Why: The workflow engine treats the presence of this file as the switch
/// from monolithic single-agent code-phase to per-file wave loop. Keeping the
/// schema small and self-contained lets the plan-agent emit it directly.
/// What: `error_convention` is an optional global hint (e.g. "exceptions",
/// "Result"). `waves` is the ordered list of waves.
/// Test: `assignments_load_returns_none_for_missing_file`,
/// `assignments_load_parses_valid_json`,
/// `assignments_load_returns_none_for_invalid_json`.
#[derive(Debug, Deserialize, Clone)]
pub struct Assignments {
    pub error_convention: Option<String>,
    pub waves: Vec<WaveDef>,
}

impl Assignments {
    /// Validate that a single wave-file path is safe to join with `out_dir` (#114).
    ///
    /// Why: `run_wave_loop` does `out_dir.join(&file.path)` to both read and
    /// write the agent's assigned file. Without validation a malicious or
    /// buggy plan-agent could emit `../../src/main.rs` or `/etc/passwd` and
    /// have the code-agent overwrite files outside the workflow output dir.
    /// Also emits a non-fatal warning (#152) when a flat filename looks like
    /// it may be encoding directory structure with underscores instead of
    /// slashes (e.g. `stages_extraction.py` instead of `stages/extraction.py`).
    /// What: Rejects (1) empty strings, (2) absolute paths, (3) any component
    /// equal to `..`. Emits a tracing WARN for suspiciously flat filenames.
    /// Lexical-only — we don't touch the filesystem here.
    /// Test: `validate_file_path_*` unit tests in this module.
    pub fn validate_file_path(path: &str) -> Result<()> {
        anyhow::ensure!(!path.is_empty(), "file path is empty");
        let p = Path::new(path);
        anyhow::ensure!(
            p.is_relative(),
            "file path '{}' is absolute; must be relative to out_dir",
            path,
        );
        for component in p.components() {
            if matches!(component, std::path::Component::ParentDir) {
                anyhow::bail!(
                    "file path '{}' contains '..' component; refusing to write outside out_dir",
                    path,
                );
            }
        }

        // #152: Heuristic warning for paths that appear to encode directory
        // structure using underscores instead of slashes. We only check the
        // final filename component (not intermediate directories) and only
        // fire when the path is flat (no `/` parent) and the stem (filename
        // without extension) is long and contains multiple underscores —
        // a strong signal of `stages_extraction.py` rather than `conftest.py`.
        // Non-fatal: legitimate flat filenames like `my_module.py` pass.
        if let Some(filename) = p.file_name().and_then(|n| n.to_str()) {
            let has_parent = p.parent().map(|par| par != Path::new("")).unwrap_or(false);
            if !has_parent {
                let stem = Path::new(filename)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(filename);
                let underscore_count = stem.chars().filter(|&c| c == '_').count();
                if stem.len() > 20 && underscore_count >= 2 {
                    tracing::warn!(
                        path = path,
                        stem = stem,
                        underscore_count = underscore_count,
                        "assignments.json path '{}' looks like it may encode directory structure \
                         with underscores (e.g. 'stages_extraction.py' instead of \
                         'stages/extraction.py'). If this file belongs inside a subdirectory, \
                         update the plan-agent output to use forward-slash separators.",
                        path
                    );
                }
            }
        }

        Ok(())
    }

    /// Validate structural invariants of the wave decomposition (#114).
    ///
    /// Why: A malformed `assignments.json` can silently skip dependencies or
    /// allow a later wave to run before its prerequisites land. Catching all
    /// violations at load time makes plan-agent bugs loud and keeps the wave
    /// loop's invariants (sequential 1-indexed ordinals, no duplicate paths,
    /// `depends_on` references a path in a strictly earlier wave) explicit.
    /// What: Collects every violation into one multi-line error so operators
    /// can fix them in a single pass. Adopted from
    /// `out/self-review-1/patches/02_wave_ordinal_validation.rs`.
    /// Test: `validate_assignments_*` unit tests in this module.
    pub fn validate(&self) -> Result<()> {
        let (fatal, ordering) = self.collect_validation_errors();
        let mut errors = fatal;
        errors.extend(ordering);
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "assignments.json validation failed:\n  - {}",
                errors.join("\n  - ")
            )
        }
    }

    /// Collect validation errors, splitting fatal (un-repairable) from
    /// ordering (repairable) violations (#162).
    ///
    /// Why: The wave validator historically rejected *any* error — including
    /// same-wave dependencies that are trivially fixable by reshuffling files
    /// into a topologically valid ordering. Splitting violation classes lets
    /// `load()` attempt repair when only ordering is broken while still
    /// rejecting plans with duplicates, path traversal, bad ordinals, or
    /// true dependency cycles.
    /// What: Returns `(fatal_errors, ordering_errors)`. Fatal = empty waves,
    /// non-sequential ordinals, duplicate paths, unsafe paths, unknown deps.
    /// Ordering = depends_on references a path in the same wave or a later
    /// wave (linearizable via Kahn's algorithm unless it's a true cycle).
    /// Test: `wave_validator_repairs_same_wave_dep` exercises the split.
    fn collect_validation_errors(&self) -> (Vec<String>, Vec<String>) {
        let mut fatal: Vec<String> = Vec::new();
        let mut ordering: Vec<String> = Vec::new();

        if self.waves.is_empty() {
            fatal.push("assignments has no waves".into());
        }

        for (i, wave) in self.waves.iter().enumerate() {
            let expected = (i + 1) as u32;
            if wave.wave != expected {
                fatal.push(format!(
                    "wave position {}: expected ordinal {expected}, got {}",
                    i, wave.wave
                ));
            }
        }

        // Path traversal + duplicate detection.
        let mut seen_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut all_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for wave in &self.waves {
            for file in &wave.files {
                if let Err(e) = Self::validate_file_path(&file.path) {
                    fatal.push(format!("wave {}: {e}", wave.wave));
                }
                if !seen_paths.insert(&file.path) {
                    fatal.push(format!("duplicate file path: '{}'", file.path));
                }
                all_paths.insert(&file.path);
            }
        }

        // depends_on classification:
        // - unknown dep (not in any wave) → fatal (can't repair without inventing a file)
        // - same-wave or later-wave dep → ordering (repairable via topo sort)
        let mut earlier_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for wave in &self.waves {
            for file in &wave.files {
                for dep in &file.depends_on {
                    if !all_paths.contains(dep.as_str()) {
                        fatal.push(format!(
                            "file '{}' in wave {} depends on '{}' which is not declared in any wave",
                            file.path, wave.wave, dep
                        ));
                    } else if !earlier_paths.contains(dep.as_str()) {
                        ordering.push(format!(
                            "file '{}' in wave {} depends on '{}' which is not in an earlier wave",
                            file.path, wave.wave, dep
                        ));
                    }
                }
            }
            for file in &wave.files {
                earlier_paths.insert(&file.path);
            }
        }

        (fatal, ordering)
    }

    /// Repair same-wave / forward dependency violations by topologically
    /// re-assigning files to waves (#162).
    ///
    /// Why: Plan-agents sometimes emit plans where `conftest.py` and the
    /// `test_*.py` files that import it live in the same wave. Rejecting the
    /// entire plan forces the engine to fall back to the legacy monolithic
    /// code phase, which defeats the wave loop for L1–L5. Before rejection,
    /// attempt Kahn's algorithm: if the dependency graph is a DAG, we can
    /// always produce a valid layered ordering.
    /// What: Builds the dependency graph across ALL files in all waves,
    /// runs Kahn's BFS to compute the longest-path layer for each node, and
    /// rewrites `self.waves` accordingly. Returns `Ok(true)` when any file
    /// moved, `Ok(false)` when the existing layering was already valid, and
    /// `Err(_)` when a true cycle prevents linearization (or the graph
    /// references an unknown dependency — caller should treat that as fatal).
    /// Test: `wave_validator_repairs_same_wave_dep`,
    /// `wave_validator_rejects_true_cycle`,
    /// `wave_validator_handles_conftest_pattern`.
    pub fn repair_wave_ordering(&mut self) -> Result<bool, String> {
        use std::collections::{HashMap, HashSet, VecDeque};

        // Collect every file path and its dependencies + per-file assignment.
        // We keep the original insertion order so that ties in layer
        // assignment produce a deterministic output aligned with the input.
        let mut order: Vec<String> = Vec::new();
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        let mut original_layer: HashMap<String, u32> = HashMap::new();
        let mut files_by_path: HashMap<String, FileAssignment> = HashMap::new();
        let mut known_paths: HashSet<String> = HashSet::new();

        for wave in &self.waves {
            for f in &wave.files {
                known_paths.insert(f.path.clone());
            }
        }

        for wave in &self.waves {
            for f in &wave.files {
                order.push(f.path.clone());
                deps.insert(f.path.clone(), f.depends_on.clone());
                original_layer.insert(f.path.clone(), wave.wave);
                files_by_path.insert(f.path.clone(), f.clone());
            }
        }

        // Reject early if any dep references an unknown path — we cannot
        // manufacture a node for it.
        for (path, dlist) in &deps {
            for d in dlist {
                if !known_paths.contains(d) {
                    return Err(format!(
                        "file '{path}' depends on '{d}' which is not declared in any wave"
                    ));
                }
            }
        }

        // Build in-degree and reverse-adjacency for Kahn's algorithm.
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for path in &order {
            in_degree.insert(path.clone(), 0);
            dependents.insert(path.clone(), Vec::new());
        }
        for (path, dlist) in &deps {
            for d in dlist {
                // path depends on d → d must come first. Edge d -> path.
                *in_degree.get_mut(path).unwrap() += 1;
                dependents.get_mut(d).unwrap().push(path.clone());
            }
        }

        // Kahn's algorithm with layer assignment: layer(node) = 1 + max(layer(deps)).
        let mut layer: HashMap<String, u32> = HashMap::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for path in &order {
            if in_degree[path] == 0 {
                layer.insert(path.clone(), 1);
                queue.push_back(path.clone());
            }
        }

        let mut processed = 0usize;
        while let Some(node) = queue.pop_front() {
            processed += 1;
            let node_layer = layer[&node];
            // Clone dependent list to avoid borrow conflicts.
            let deps_of_node = dependents[&node].clone();
            for dep in deps_of_node {
                let cur = in_degree.get_mut(&dep).unwrap();
                *cur -= 1;
                let candidate_layer = node_layer + 1;
                let existing = layer.get(&dep).copied().unwrap_or(0);
                if candidate_layer > existing {
                    layer.insert(dep.clone(), candidate_layer);
                }
                if *cur == 0 {
                    queue.push_back(dep);
                }
            }
        }

        if processed != order.len() {
            // Unprocessed nodes → cycle.
            let unresolved: Vec<String> = order
                .iter()
                .filter(|p| in_degree.get(*p).copied().unwrap_or(0) > 0)
                .cloned()
                .collect();
            return Err(format!(
                "dependency cycle detected; cannot linearize {} file(s): {}",
                unresolved.len(),
                unresolved.join(", ")
            ));
        }

        // Determine if repair actually changes anything.
        let mut changed = false;
        for path in &order {
            if original_layer[path] != layer[path] {
                changed = true;
                break;
            }
        }

        if !changed {
            return Ok(false);
        }

        // Rebuild self.waves from the layer map, preserving the original
        // per-layer insertion order so the output is deterministic.
        let max_layer = layer.values().copied().max().unwrap_or(0);
        let mut new_waves: Vec<WaveDef> = (1..=max_layer)
            .map(|w| WaveDef {
                wave: w,
                files: Vec::new(),
            })
            .collect();
        for path in &order {
            let l = layer[path];
            let idx = (l - 1) as usize;
            new_waves[idx]
                .files
                .push(files_by_path.get(path).unwrap().clone());
        }

        // Drop any empty waves (shouldn't happen with Kahn's layering, but
        // guard against it so the ordinals stay sequential).
        new_waves.retain(|w| !w.files.is_empty());
        for (i, w) in new_waves.iter_mut().enumerate() {
            w.wave = (i + 1) as u32;
        }

        self.waves = new_waves;
        Ok(true)
    }

    /// Best-effort loader that returns `None` when the file is absent OR when
    /// parsing fails.
    ///
    /// Why: Falling back silently to the legacy monolithic code phase means a
    /// partially written or malformed `assignments.json` never bricks a
    /// workflow run — the engine just behaves as it did before #88. HOWEVER,
    /// silent failures made it impossible to diagnose why the wave loop was
    /// skipped in live runs (#88 follow-up): a plan-agent that emits JSON
    /// missing one required field would silently return `None`, leaving
    /// operators guessing. We now log every branch (absent / read-fail /
    /// parse-fail / ok) so a `RUST_LOG=debug` run makes the decision obvious.
    /// What: Reads `out_dir/assignments.json`; returns `Some(Self)` on a
    /// clean parse, otherwise `None`. Every return path emits a log line.
    /// Test: All three `assignments_load_*` tests in this module plus
    /// `wave_loop_runs_one_agent_per_file` exercise the success path, and
    /// `assignments_load_returns_none_for_invalid_json` covers the parse-fail
    /// log path (log line is informational, not asserted).
    pub fn load(out_dir: &Path) -> Option<Self> {
        let path = out_dir.join("assignments.json");
        if !path.exists() {
            tracing::debug!(
                path = %path.display(),
                "assignments.json not found; falling back to monolithic code phase"
            );
            return None;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "assignments.json exists but read failed; falling back to monolithic code phase"
                );
                return None;
            }
        };
        match serde_json::from_str::<Self>(&content) {
            Ok(mut a) => {
                // #114/#162: Structural validation. Split errors into fatal
                // (duplicates, path traversal, bad ordinals, empty waves,
                // unknown deps) vs ordering (same-wave or forward deps).
                // Ordering-only failures are repairable via topological sort
                // (Kahn's algorithm) — don't fall back to the monolithic code
                // phase when we can trivially fix the layering.
                let (fatal, ordering) = a.collect_validation_errors();
                if !fatal.is_empty() {
                    tracing::error!(
                        path = %path.display(),
                        errors = %fatal.join("; "),
                        "assignments.json has fatal validation errors; refusing to run wave loop"
                    );
                    return None;
                }
                if !ordering.is_empty() {
                    let violation_count = ordering.len();
                    match a.repair_wave_ordering() {
                        Ok(true) => {
                            tracing::warn!(
                                path = %path.display(),
                                violations = violation_count,
                                "wave validator repaired {} same-wave dependency violations via topological sort",
                                violation_count
                            );
                        }
                        Ok(false) => {
                            tracing::error!(
                                path = %path.display(),
                                "wave validator reported ordering violations but repair made no changes; refusing to run wave loop"
                            );
                            return None;
                        }
                        Err(cycle_msg) => {
                            tracing::error!(
                                path = %path.display(),
                                error = %cycle_msg,
                                "assignments.json has true dependency cycles; refusing to run wave loop"
                            );
                            return None;
                        }
                    }
                    if let Err(e) = a.validate() {
                        tracing::error!(
                            path = %path.display(),
                            error = %e,
                            "assignments.json still invalid after repair; refusing to run wave loop"
                        );
                        return None;
                    }
                }
                tracing::info!(
                    path = %path.display(),
                    waves = a.waves.len(),
                    files = a.waves.iter().map(|w| w.files.len()).sum::<usize>(),
                    "assignments.json parsed; wave loop will run"
                );
                Some(a)
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    bytes = content.len(),
                    "assignments.json parse failed; falling back to monolithic code phase"
                );
                None
            }
        }
    }
}

impl WorkflowDef {
    /// Load a workflow from a JSON file path.
    ///
    /// Why: Centralizes read + parse error messages for the CLI caller.
    /// What: Reads the file, deserializes via serde_json, and validates the
    /// resulting config so semantic errors (e.g. ticket_management enabled
    /// without a repo) fail loudly at load time instead of silently at
    /// runtime (#102).
    /// Test: See `config_from_json_parses_minimal` and
    /// `validate_rejects_enabled_ticket_management_without_repo` below.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read workflow config {}", path.display()))?;
        let def: WorkflowDef = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse workflow JSON {}", path.display()))?;
        def.validate()
            .with_context(|| format!("invalid workflow config {}", path.display()))?;
        Ok(def)
    }

    /// Validate semantic invariants of a loaded workflow config (#102).
    ///
    /// Why: Some fields only matter when another is enabled; catching the
    /// mismatch here converts a confusing runtime 404 (e.g. empty repo
    /// yielding a `POST /repos//issues` request) into a clear load-time
    /// failure that names the offending field.
    /// What: Returns `Err` when `ticket_management.enabled` is true but
    /// `repo` is empty. Additional validations can be appended here.
    /// Test: `validate_rejects_enabled_ticket_management_without_repo`,
    /// `validate_accepts_disabled_ticket_management_without_repo`.
    pub fn validate(&self) -> Result<()> {
        if let Some(tm) = self.ticket_management.as_ref()
            && tm.enabled
            && tm.repo.trim().is_empty()
        {
            anyhow::bail!(
                "ticket_management.enabled = true but ticket_management.repo is empty; \
                 set repo to \"owner/name\" or set enabled = false"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_json_parses_minimal() {
        let raw = r#"
        {
            "name": "t",
            "description": "d",
            "phases": [
                {"name":"a","agent":"agent-a","context_template":"hi"},
                {"name":"b","agent":"agent-b","model_override":"m","context_template":"bye"}
            ]
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        assert_eq!(def.name, "t");
        assert_eq!(def.phases.len(), 2);
        assert_eq!(def.phases[0].name, "a");
        assert!(def.phases[0].model.is_none());
        assert_eq!(def.phases[1].model.as_deref(), Some("m"));
    }

    #[test]
    fn parallel_subtasks_parse_from_json() {
        // #73: `parallel_subtasks` + `worktree_protection` land on PhaseDef
        // without breaking workflows that omit them.
        let raw = r#"
        {
            "name": "p",
            "phases": [
                {
                    "name": "code",
                    "agent": "code-agent",
                    "context_template": "{{task}}",
                    "parallel_subtasks": [
                        {"label": "a", "task_suffix": "do a"},
                        {"label": "b", "task_suffix": "do b"}
                    ],
                    "worktree_protection": true
                }
            ]
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        let subs = def.phases[0]
            .parallel_subtasks
            .as_ref()
            .expect("subtasks present");
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].label, "a");
        assert_eq!(subs[1].task_suffix, "do b");
        assert_eq!(def.phases[0].worktree_protection, Some(true));
    }

    #[test]
    fn auto_push_config_defaults() {
        // #76: `auto_push` at workflow level parses with defaults.
        let raw = r#"
        {
            "name": "p",
            "phases": [{"name":"a","agent":"x","context_template":"t"}],
            "auto_push": {"enabled": true}
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        let ap = def.auto_push.expect("auto_push present");
        assert!(ap.enabled);
        assert_eq!(ap.version_bump, "patch");
        assert_eq!(ap.push_remote, "origin");
        assert_eq!(ap.push_branch, "main");
        assert!(ap.commit_message_template.contains("{{workflow}}"));
    }

    #[test]
    fn phase_ast_native_parses_from_json() {
        // Per-phase ast_native override deserializes correctly.
        // Why: Hybrid mode (research+plan AST-native, code+qa traditional)
        // requires the field to round-trip through serde_json.
        // What: A JSON doc with `"ast_native": true` must produce
        // `Some(true)` on the PhaseDef.
        let raw = r#"{"name":"research","agent":"r","context_template":"t","ast_native":true}"#;
        let phase: PhaseDef = serde_json::from_str(raw).unwrap();
        assert_eq!(phase.ast_native, Some(true));

        let raw_false = r#"{"name":"qa","agent":"q","context_template":"t","ast_native":false}"#;
        let phase: PhaseDef = serde_json::from_str(raw_false).unwrap();
        assert_eq!(phase.ast_native, Some(false));
    }

    #[test]
    fn phase_ast_native_defaults_to_none() {
        // Phases without `ast_native` inherit the global --ast-native flag.
        // Why: Backward compatibility — existing workflow JSON files must
        // continue to deserialize without the new field.
        // What: A PhaseDef parsed from JSON omitting `ast_native` must have
        // `ast_native == None`.
        let raw = r#"{"name":"code","agent":"e","context_template":"t"}"#;
        let phase: PhaseDef = serde_json::from_str(raw).unwrap();
        assert!(phase.ast_native.is_none());
    }

    #[test]
    fn phase_skip_parses_and_defaults_to_none() {
        // #82: `skip: true` on a phase parses, and a phase without `skip` at
        // all leaves the field as `None` (treated as false by the engine).
        let raw = r#"
        {
            "name": "w",
            "phases": [
                {"name": "a", "agent": "x", "context_template": "t"},
                {"name": "b", "agent": "y", "context_template": "t", "skip": true}
            ]
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        assert_eq!(def.phases[0].skip, None);
        assert_eq!(def.phases[1].skip, Some(true));
    }

    #[test]
    fn ticket_management_config_defaults() {
        // #84: A workflow JSON that includes a minimal `ticket_management`
        // block must parse with the boolean defaults (`auto_relate`,
        // `phase_comments`, `close_on_success` all true) and `enabled=false`
        // when omitted, so adding the field does not accidentally enable
        // ticket creation for workflows that never opted in.
        let raw = r#"
        {
            "name": "w",
            "phases": [{"name":"a","agent":"x","context_template":"t"}],
            "ticket_management": {"enabled": true, "repo": "owner/repo"}
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        let tm = def.ticket_management.expect("ticket_management present");
        assert!(tm.enabled);
        assert_eq!(tm.repo, "owner/repo");
        assert!(tm.auto_relate);
        assert!(tm.phase_comments);
        assert!(tm.close_on_success);

        // Omitting the block entirely leaves it as `None`.
        let raw_absent = r#"
        {
            "name": "w",
            "phases": [{"name":"a","agent":"x","context_template":"t"}]
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw_absent).unwrap();
        assert!(def.ticket_management.is_none());
    }

    #[test]
    fn assignments_load_returns_none_for_missing_file() {
        // #88: Absent assignments.json → None so the engine falls back to
        // monolithic code phase.
        let tmp = tempfile::tempdir().unwrap();
        let loaded = Assignments::load(tmp.path());
        assert!(loaded.is_none());
    }

    #[test]
    fn assignments_load_parses_valid_json() {
        // #88: A two-wave document round-trips through Assignments::load.
        let tmp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "error_convention": "exceptions",
            "waves": [
                {
                    "wave": 1,
                    "files": [
                        {
                            "path": "src/util.py",
                            "stub": "util.py",
                            "purpose": "helpers",
                            "max_lines": 120
                        }
                    ]
                },
                {
                    "wave": 2,
                    "files": [
                        {
                            "path": "src/main.py",
                            "stub": "main.py",
                            "purpose": "entrypoint",
                            "depends_on": ["src/util.py"]
                        }
                    ]
                }
            ]
        }"#;
        std::fs::write(tmp.path().join("assignments.json"), raw).unwrap();
        let a = Assignments::load(tmp.path()).expect("parses");
        assert_eq!(a.error_convention.as_deref(), Some("exceptions"));
        assert_eq!(a.waves.len(), 2);
        assert_eq!(a.waves[0].wave, 1);
        assert_eq!(a.waves[0].files.len(), 1);
        assert_eq!(a.waves[0].files[0].path, "src/util.py");
        assert_eq!(a.waves[0].files[0].max_lines, Some(120));
        assert!(a.waves[0].files[0].depends_on.is_empty());
        assert_eq!(a.waves[1].files[0].depends_on, vec!["src/util.py"]);
    }

    #[test]
    fn assignments_load_returns_none_for_invalid_json() {
        // #88: Malformed JSON must not crash the engine — just return None.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("assignments.json"), "{ not json").unwrap();
        let loaded = Assignments::load(tmp.path());
        assert!(loaded.is_none());
    }

    #[test]
    fn file_assignment_stub_can_be_null() {
        // Plan-agent emits `"stub": null` for files with no scaffold (e.g.
        // `__init__.py`). Serde must accept null without rejecting the document.
        let raw = r#"{
            "waves": [
                {
                    "wave": 1,
                    "files": [
                        {
                            "path": "src/__init__.py",
                            "stub": null,
                            "purpose": "package marker"
                        }
                    ]
                }
            ]
        }"#;
        let a: Assignments = serde_json::from_str(raw).expect("null stub must parse");
        assert_eq!(a.waves[0].files[0].stub, None);
    }

    #[test]
    fn validate_rejects_enabled_ticket_management_without_repo() {
        // #102: `ticket_management.enabled = true` + empty `repo` must fail
        // validation with a clear message rather than silently producing a
        // 404 at runtime.
        let raw = r#"
        {
            "name": "w",
            "phases": [{"name":"a","agent":"x","context_template":"t"}],
            "ticket_management": {"enabled": true, "repo": ""}
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        let err = def.validate().expect_err("empty repo must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("repo"),
            "error should mention the empty repo field, got: {msg}"
        );
    }

    #[test]
    fn validate_accepts_disabled_ticket_management_without_repo() {
        // #102: When disabled, an empty repo is irrelevant — validation must
        // still pass so operators can keep the block around for reference.
        let raw = r#"
        {
            "name": "w",
            "phases": [{"name":"a","agent":"x","context_template":"t"}],
            "ticket_management": {"enabled": false, "repo": ""}
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        assert!(def.validate().is_ok());
    }

    #[test]
    fn validate_accepts_enabled_ticket_management_with_repo() {
        let raw = r#"
        {
            "name": "w",
            "phases": [{"name":"a","agent":"x","context_template":"t"}],
            "ticket_management": {"enabled": true, "repo": "owner/name"}
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        assert!(def.validate().is_ok());
    }

    #[test]
    fn auto_push_absent_by_default() {
        let raw = r#"
        {
            "name": "p",
            "phases": [{"name":"a","agent":"x","context_template":"t"}]
        }
        "#;
        let def: WorkflowDef = serde_json::from_str(raw).unwrap();
        assert!(def.auto_push.is_none());
    }

    // ---- #114: Assignments validation tests ----

    fn fa(path: &str, depends_on: Vec<&str>) -> FileAssignment {
        FileAssignment {
            path: path.to_string(),
            stub: None,
            purpose: format!("test {path}"),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            max_lines: None,
        }
    }

    #[test]
    fn validate_file_path_accepts_safe_relative() {
        assert!(Assignments::validate_file_path("src/foo.rs").is_ok());
        assert!(Assignments::validate_file_path("a/b/c/d.py").is_ok());
        assert!(Assignments::validate_file_path("file.txt").is_ok());
    }

    #[test]
    fn validate_file_path_rejects_absolute() {
        // #114: absolute paths must be rejected to prevent out_dir escape.
        let err = Assignments::validate_file_path("/etc/passwd").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("absolute"), "got: {msg}");
        assert!(msg.contains("/etc/passwd"), "got: {msg}");
    }

    #[test]
    fn validate_file_path_rejects_parent_traversal() {
        let err = Assignments::validate_file_path("../escape.py").unwrap_err();
        assert!(err.to_string().contains(".."), "got: {err}");

        let err2 = Assignments::validate_file_path("src/../../etc/passwd").unwrap_err();
        assert!(err2.to_string().contains(".."), "got: {err2}");
    }

    #[test]
    fn validate_file_path_rejects_empty() {
        let err = Assignments::validate_file_path("").unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
    }

    #[test]
    fn validate_file_path_rejects_bare_dotdot() {
        let err = Assignments::validate_file_path("..").unwrap_err();
        assert!(err.to_string().contains(".."), "got: {err}");
    }

    #[test]
    fn validate_assignments_accepts_valid_two_wave() {
        let asg = Assignments {
            error_convention: None,
            waves: vec![
                WaveDef {
                    wave: 1,
                    files: vec![fa("a.py", vec![])],
                },
                WaveDef {
                    wave: 2,
                    files: vec![fa("b.py", vec!["a.py"])],
                },
            ],
        };
        assert!(asg.validate().is_ok());
    }

    #[test]
    fn validate_assignments_rejects_non_sequential_ordinals() {
        let asg = Assignments {
            error_convention: None,
            waves: vec![
                WaveDef {
                    wave: 1,
                    files: vec![fa("a.py", vec![])],
                },
                WaveDef {
                    wave: 3,
                    files: vec![fa("b.py", vec!["a.py"])],
                },
            ],
        };
        let err = asg.validate().unwrap_err().to_string();
        assert!(err.contains("expected ordinal 2"), "got: {err}");
    }

    #[test]
    fn validate_assignments_rejects_unsafe_path() {
        let asg = Assignments {
            error_convention: None,
            waves: vec![WaveDef {
                wave: 1,
                files: vec![fa("../../etc/passwd", vec![])],
            }],
        };
        let err = asg.validate().unwrap_err().to_string();
        assert!(err.contains(".."), "got: {err}");
    }

    #[test]
    fn validate_assignments_rejects_duplicate_paths() {
        let asg = Assignments {
            error_convention: None,
            waves: vec![
                WaveDef {
                    wave: 1,
                    files: vec![fa("a.py", vec![])],
                },
                WaveDef {
                    wave: 2,
                    files: vec![fa("a.py", vec![])],
                },
            ],
        };
        let err = asg.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn validate_assignments_rejects_forward_or_same_wave_dep() {
        let asg_same = Assignments {
            error_convention: None,
            waves: vec![WaveDef {
                wave: 1,
                files: vec![fa("a.py", vec![]), fa("b.py", vec!["a.py"])],
            }],
        };
        let err = asg_same.validate().unwrap_err().to_string();
        assert!(err.contains("not in an earlier wave"), "got: {err}");

        let asg_fwd = Assignments {
            error_convention: None,
            waves: vec![
                WaveDef {
                    wave: 1,
                    files: vec![fa("a.py", vec!["b.py"])],
                },
                WaveDef {
                    wave: 2,
                    files: vec![fa("b.py", vec![])],
                },
            ],
        };
        let err = asg_fwd.validate().unwrap_err().to_string();
        assert!(err.contains("not in an earlier wave"), "got: {err}");
    }

    #[test]
    fn validate_assignments_rejects_empty_waves() {
        let asg = Assignments {
            error_convention: None,
            waves: vec![],
        };
        let err = asg.validate().unwrap_err().to_string();
        assert!(err.contains("no waves"), "got: {err}");
    }

    #[test]
    fn assignments_load_rejects_path_traversal_via_disk() {
        // #114: A plan-agent emitting `../../etc/passwd` must NOT cause the
        // wave loop to run. `load()` returns None (with an ERROR log) and
        // the engine falls back to the monolithic code phase.
        let tmp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "waves": [
                {
                    "wave": 1,
                    "files": [
                        {
                            "path": "../../etc/passwd",
                            "stub": null,
                            "purpose": "attack"
                        }
                    ]
                }
            ]
        }"#;
        std::fs::write(tmp.path().join("assignments.json"), raw).unwrap();
        let loaded = Assignments::load(tmp.path());
        assert!(
            loaded.is_none(),
            "path-traversal assignments must be rejected"
        );
    }

    #[test]
    fn assignments_load_rejects_absolute_path_via_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "waves": [
                {
                    "wave": 1,
                    "files": [
                        {
                            "path": "/etc/passwd",
                            "stub": null,
                            "purpose": "attack"
                        }
                    ]
                }
            ]
        }"#;
        std::fs::write(tmp.path().join("assignments.json"), raw).unwrap();
        let loaded = Assignments::load(tmp.path());
        assert!(
            loaded.is_none(),
            "absolute-path assignment must be rejected"
        );
    }

    // ---- #152: Flat-path heuristic warning tests ----

    #[test]
    fn validate_file_path_accepts_proper_directory_path() {
        // #152: Properly structured paths with slashes must pass without error.
        assert!(
            Assignments::validate_file_path("src/doc_pipeline/stages/extraction.py").is_ok(),
            "directory-structured path must be accepted"
        );
        assert!(
            Assignments::validate_file_path("tests/test_api.py").is_ok(),
            "test path with one directory level must be accepted"
        );
        assert!(
            Assignments::validate_file_path("app/routers/users.py").is_ok(),
            "nested path must be accepted"
        );
    }

    #[test]
    fn validate_file_path_accepts_legitimate_flat_file() {
        // #152: Short flat filenames like conftest.py or __init__.py must pass
        // without triggering the heuristic warning (they are not encoding
        // directory structure — they are genuinely flat files).
        assert!(
            Assignments::validate_file_path("conftest.py").is_ok(),
            "conftest.py is a legitimate flat file"
        );
        assert!(
            Assignments::validate_file_path("__init__.py").is_ok(),
            "__init__.py is a legitimate flat file"
        );
        assert!(
            Assignments::validate_file_path("my_module.py").is_ok(),
            "short flat file with one underscore must be accepted"
        );
    }

    #[test]
    fn validate_file_path_accepts_suspiciously_flat_but_non_fatal() {
        // #152: A long flat filename that looks like encoded directory structure
        // (e.g. stages_extraction_helpers.py) must still PASS validate_file_path
        // (the check is non-fatal — it only emits a warning). The result is Ok.
        assert!(
            Assignments::validate_file_path("stages_extraction_helpers.py").is_ok(),
            "heuristic warning for flat path must be non-fatal"
        );
        assert!(
            Assignments::validate_file_path("src_doc_pipeline_stages_extraction.py").is_ok(),
            "very long flat path must be non-fatal (warning only)"
        );
    }

    // ---- #162: Wave validator topological repair tests ----

    #[test]
    fn wave_validator_repairs_same_wave_dep() {
        // #162: A depends on B but both are in wave 1 → repair moves B to
        // wave 1 and A to wave 2. The original validate() would reject.
        let mut asg = Assignments {
            error_convention: None,
            waves: vec![WaveDef {
                wave: 1,
                files: vec![fa("a.py", vec!["b.py"]), fa("b.py", vec![])],
            }],
        };
        // Pre-repair validation must fail with an ordering violation.
        assert!(asg.validate().is_err(), "same-wave dep must fail validate");

        let changed = asg
            .repair_wave_ordering()
            .expect("acyclic graph must repair");
        assert!(changed, "repair must report changes");

        // Post-repair: B in wave 1, A in wave 2, and validate() passes.
        assert_eq!(asg.waves.len(), 2);
        assert_eq!(asg.waves[0].wave, 1);
        assert_eq!(asg.waves[1].wave, 2);
        let wave1_paths: Vec<&str> = asg.waves[0].files.iter().map(|f| f.path.as_str()).collect();
        let wave2_paths: Vec<&str> = asg.waves[1].files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(wave1_paths, vec!["b.py"]);
        assert_eq!(wave2_paths, vec!["a.py"]);
        assert!(
            asg.validate().is_ok(),
            "repaired assignments must pass validation"
        );
    }

    #[test]
    fn wave_validator_rejects_true_cycle() {
        // #162: A depends on B, B depends on A → no topological ordering
        // exists; repair must return Err(cycle message).
        let mut asg = Assignments {
            error_convention: None,
            waves: vec![WaveDef {
                wave: 1,
                files: vec![fa("a.py", vec!["b.py"]), fa("b.py", vec!["a.py"])],
            }],
        };
        let err = asg
            .repair_wave_ordering()
            .expect_err("cycle must not be repairable");
        assert!(
            err.contains("cycle") || err.contains("linearize"),
            "error should mention cycle, got: {err}"
        );
    }

    #[test]
    fn wave_validator_handles_conftest_pattern() {
        // #162: Realistic case — conftest.py and several test_*.py files are
        // all in wave 1 because the plan-agent didn't recognize the import
        // relationship. After repair, conftest.py must move to an earlier
        // wave than every test file that depends on it.
        let mut asg = Assignments {
            error_convention: None,
            waves: vec![WaveDef {
                wave: 1,
                files: vec![
                    fa("tests/conftest.py", vec![]),
                    fa("tests/test_alpha.py", vec!["tests/conftest.py"]),
                    fa("tests/test_beta.py", vec!["tests/conftest.py"]),
                    fa("tests/test_gamma.py", vec!["tests/conftest.py"]),
                ],
            }],
        };

        let changed = asg
            .repair_wave_ordering()
            .expect("conftest pattern is a DAG");
        assert!(changed);
        assert_eq!(asg.waves.len(), 2);
        // conftest must be alone (or at least present) in wave 1.
        let wave1_paths: Vec<&str> = asg.waves[0].files.iter().map(|f| f.path.as_str()).collect();
        let wave2_paths: Vec<&str> = asg.waves[1].files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            wave1_paths.contains(&"tests/conftest.py"),
            "conftest must be in wave 1, got {wave1_paths:?}"
        );
        for tf in [
            "tests/test_alpha.py",
            "tests/test_beta.py",
            "tests/test_gamma.py",
        ] {
            assert!(
                wave2_paths.contains(&tf),
                "{tf} must be in wave 2, got {wave2_paths:?}"
            );
        }
        assert!(asg.validate().is_ok());
    }

    #[test]
    fn wave_validator_repair_noop_on_valid_plan() {
        // #162: An already-valid plan must report no changes (repair returns
        // Ok(false)) and leave the waves untouched.
        let mut asg = Assignments {
            error_convention: None,
            waves: vec![
                WaveDef {
                    wave: 1,
                    files: vec![fa("a.py", vec![])],
                },
                WaveDef {
                    wave: 2,
                    files: vec![fa("b.py", vec!["a.py"])],
                },
            ],
        };
        let changed = asg
            .repair_wave_ordering()
            .expect("valid plan must not error");
        assert!(!changed, "valid plan must not be modified");
        assert_eq!(asg.waves.len(), 2);
        assert_eq!(asg.waves[0].files[0].path, "a.py");
        assert_eq!(asg.waves[1].files[0].path, "b.py");
    }

    #[test]
    fn wave_validator_load_repairs_same_wave_via_disk() {
        // #162: End-to-end — a plan with a same-wave violation written to
        // disk must load successfully (repair kicks in) instead of returning
        // None and falling back to monolithic execution.
        let tmp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "waves": [
                {
                    "wave": 1,
                    "files": [
                        {"path": "tests/conftest.py", "stub": null, "purpose": "shared fixtures"},
                        {"path": "tests/test_api.py", "stub": null, "purpose": "api tests",
                         "depends_on": ["tests/conftest.py"]}
                    ]
                }
            ]
        }"#;
        std::fs::write(tmp.path().join("assignments.json"), raw).unwrap();
        let a = Assignments::load(tmp.path()).expect("load must repair, not reject");
        assert_eq!(a.waves.len(), 2, "repair must produce 2 waves");
        assert_eq!(a.waves[0].files[0].path, "tests/conftest.py");
        assert_eq!(a.waves[1].files[0].path, "tests/test_api.py");
    }

    #[test]
    fn wave_validator_load_rejects_true_cycle_via_disk() {
        // #162: A real cycle in assignments.json must return None (fall back
        // to monolithic) because repair cannot linearize it.
        let tmp = tempfile::tempdir().unwrap();
        let raw = r#"{
            "waves": [
                {
                    "wave": 1,
                    "files": [
                        {"path": "a.py", "stub": null, "purpose": "a",
                         "depends_on": ["b.py"]},
                        {"path": "b.py", "stub": null, "purpose": "b",
                         "depends_on": ["a.py"]}
                    ]
                }
            ]
        }"#;
        std::fs::write(tmp.path().join("assignments.json"), raw).unwrap();
        let loaded = Assignments::load(tmp.path());
        assert!(loaded.is_none(), "true cycle must be rejected");
    }

    // -------- safe_join (#114) defense-in-depth path-traversal guard --------

    #[test]
    fn safe_join_rejects_parent_traversal() {
        // Why: ../../etc/passwd must NOT escape the out_dir, even when the
        // candidate would lexically resolve outside the base.
        // Test: assert safe_join returns None for parent-traversal attempts.
        let tmp = tempfile::tempdir().unwrap();
        assert!(safe_join(tmp.path(), "../../etc/passwd").is_none());
        assert!(safe_join(tmp.path(), "../escape.txt").is_none());
        assert!(safe_join(tmp.path(), "src/../../escape.txt").is_none());
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        // Why: An absolute path like /tmp/evil must be rejected — we strip the
        // leading slash and treat it as relative; any subsequent escape via
        // `..` should still be caught.
        // Test: absolute paths are anchored to base; pure absolute paths that
        // don't traverse simply land inside base.
        let tmp = tempfile::tempdir().unwrap();
        // Pure traversal absolute paths must be rejected.
        assert!(safe_join(tmp.path(), "/etc/../../etc/passwd").is_none());
        // But a normal absolute path becomes relative-to-base (defensive — we
        // never want a write to actually land at /tmp/evil regardless).
        let resolved = safe_join(tmp.path(), "/tmp/evil").expect("anchored");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn safe_join_allows_normal_path() {
        // Why: Legitimate relative paths like src/main.py must succeed and
        // resolve inside the base.
        let tmp = tempfile::tempdir().unwrap();
        let resolved = safe_join(tmp.path(), "src/main.py").expect("legit path");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("src/main.py"));
    }

    #[test]
    fn safe_join_allows_nested_path() {
        // Why: Deep nested legitimate paths (pkg/sub/module.py) must succeed.
        let tmp = tempfile::tempdir().unwrap();
        let resolved = safe_join(tmp.path(), "pkg/sub/module.py").expect("nested path");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("pkg/sub/module.py"));
    }

    #[test]
    fn safe_join_resolves_internal_dotdot_safely() {
        // Why: `pkg/../pkg2/file.py` is a legitimate (if odd) construct — the
        // parent is consumed by `pkg`. It should resolve inside base.
        let tmp = tempfile::tempdir().unwrap();
        let resolved = safe_join(tmp.path(), "pkg/../pkg2/file.py").expect("dotdot ok");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("pkg2/file.py"));
    }
}
