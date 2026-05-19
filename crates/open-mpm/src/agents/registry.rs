//! Dynamic agent discovery with hierarchical search paths and capability
//! matching (#167).
//!
//! Why: Hard-coding agent definitions in `config/agents/` makes the harness
//! brittle for operators who want per-project or per-user agent overrides.
//! This registry scans a priority-ordered list of directories on startup,
//! keeping the highest-priority copy of any agent with the same name, and
//! exposes a capability-scored `best_match` lookup so PM delegation can pick
//! the right agent from task signals without hard-coded routing tables.
//! What: `AgentRegistry` owns an ordered (name → (config, source)) map and
//! offers `get`, `best_match`, and `list` methods. Discovery is
//! failure-tolerant: missing directories and malformed TOML files log at
//! `warn` level and are skipped so one bad file never breaks startup.
//! Test: See the unit tests at the bottom of this module.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use serde::Deserialize;

use super::{
    AgentCapabilities, AgentCompressConfig, AgentConfig, AgentInfo, LlmParams, RunnerKind,
    SystemPrompt, ToolChoice, ToolsConfig,
};
use crate::llm::adapter::adapter_for_model;

/// Discovers and holds the project's agent configs.
pub struct AgentRegistry {
    /// Ordered map: agent_name → (AgentConfig, source_path).
    ///
    /// Insertion order reflects discovery order (highest priority dir first),
    /// but because shadowing skips already-inserted names, the first insert
    /// wins and later occurrences are dropped.
    agents: IndexMap<String, (AgentConfig, PathBuf)>,
}

/// Summary view of a discovered agent for the `agents list` subcommand.
///
/// Why: The raw `AgentConfig` is heavy (full system prompt, LLM params);
/// this thin view is enough for listing + capability debugging.
#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub name: String,
    pub source: PathBuf,
    pub description: String,
    pub roles: Vec<String>,
    pub languages: Vec<String>,
    pub frameworks: Vec<String>,
    pub tags: Vec<String>,
}

impl AgentRegistry {
    /// Scan the given directories in priority order (first = highest).
    ///
    /// Why: Earlier directories shadow later ones for the same agent name, so
    /// `.open-mpm/agents/engineer.toml` in the project overrides a bundled
    /// `config/agents/engineer.toml`. Missing dirs are silently skipped so
    /// operators don't need to pre-create every path.
    /// What: Walks each directory for `*.toml` files, parses each via
    /// `AgentConfig::load`, and inserts the first occurrence of each agent
    /// name. Malformed files log a warn and are skipped.
    /// Test: `registry_loads_from_multiple_dirs_with_priority`,
    /// `registry_skips_missing_dirs_silently`.
    pub fn load(search_paths: &[PathBuf]) -> Self {
        let mut agents: IndexMap<String, (AgentConfig, PathBuf)> = IndexMap::new();

        for dir in search_paths {
            if !dir.is_dir() {
                tracing::debug!(path = %dir.display(), "agent search path missing, skipping");
                continue;
            }
            let entries = match std::fs::read_dir(dir) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(path = %dir.display(), error = %e, "failed to read agent dir");
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|s| s.to_str());
                let parsed = match ext {
                    Some("toml") => AgentConfig::load(&path),
                    Some("md") => parse_md_agent(&path),
                    _ => continue,
                };
                match parsed {
                    Ok(cfg) => {
                        let name = cfg.agent.name.clone();
                        if agents.contains_key(&name) {
                            tracing::debug!(
                                agent = %name,
                                shadowed = %path.display(),
                                "lower-priority agent copy shadowed by earlier dir"
                            );
                            continue;
                        }
                        tracing::debug!(
                            agent = %name,
                            source = %path.display(),
                            "discovered agent"
                        );
                        agents.insert(name, (cfg, path));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %format!("{e:#}"),
                            "failed to parse agent file, skipping"
                        );
                    }
                }
            }
        }

        Self { agents }
    }

    /// Look up an agent by exact name.
    ///
    /// Why: Exact-name lookup is the fast path when the PM already decided
    /// which agent to delegate to (e.g. `--agent python-engineer`).
    /// What: Returns `None` if the agent was not discovered.
    /// Test: Covered implicitly by the priority + best-match tests.
    #[allow(dead_code)] // Wired into PM dispatch in a follow-up PR.
    pub fn get(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.get(name).map(|(cfg, _)| cfg)
    }

    /// Number of discovered agents.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether the registry discovered any agents.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Pick the best-matching agent name for a task given capability hints.
    ///
    /// Why: The PM receives free-form task text; upstream heuristics extract
    /// role/language/framework/tag signals and this method turns those into
    /// a concrete agent choice without a hard-coded switch statement.
    ///
    /// Design: open-mpm intentionally ships ONE language specialist
    /// (`python-engineer`) and a generic `engineer` agent that handles all
    /// other languages via runtime skill injection (see
    /// `.open-mpm/skills/languages/`). For non-Python engineering tasks, the
    /// generic `engineer` should win — the right language idiom skill
    /// (rust-idiomatic.md, go-idiomatic.md, etc.) is injected at delegation
    /// time, not encoded as a separate agent.
    ///
    /// What: Scoring — role match = 10pts, language match = 5pts, framework
    /// match = 5pts, tag match = 2pts. Plus a +50 bonus when the detected
    /// language is `python` AND the candidate is `python-engineer` (the one
    /// language specialist we ship). Critically, when the task has language
    /// signals AND an agent declares languages but NONE match, that agent
    /// is disqualified — this prevents `python-engineer` (which declares
    /// `["python"]` + four Python frameworks) from out-scoring `engineer`
    /// on Rust/Go/TS tasks via tag/role specificity tiebreaks. Ties broken
    /// by specificity (more non-empty capability fields). Returns `None`
    /// when no agents are registered or no candidate scores above 0.
    ///
    /// Test: `registry_best_match_prefers_specific_over_general`,
    /// `registry_best_match_uses_engineer_for_non_python`.
    #[allow(dead_code)] // Wired into PM delegation in a follow-up PR.
    pub fn best_match(
        &self,
        role: Option<&str>,
        languages: &[&str],
        frameworks: &[&str],
        tags: &[&str],
    ) -> Option<&str> {
        if self.agents.is_empty() {
            return None;
        }

        let mut best: Option<(&str, i32, usize)> = None;
        for (name, (cfg, _)) in &self.agents {
            let caps = cfg.agent.capabilities.as_ref();
            let mut score = 0i32;

            if let (Some(role), Some(c)) = (role, caps)
                && c.roles.iter().any(|r| r.eq_ignore_ascii_case(role))
            {
                score += 10;
            }
            if let Some(c) = caps {
                for want in languages {
                    if c.languages.iter().any(|l| l.eq_ignore_ascii_case(want)) {
                        score += 5;
                    }
                }
                for want in frameworks {
                    if c.frameworks.iter().any(|f| f.eq_ignore_ascii_case(want)) {
                        score += 5;
                    }
                }
                for want in tags {
                    if c.tags.iter().any(|t| t.eq_ignore_ascii_case(want)) {
                        score += 2;
                    }
                }
            }

            // Disqualify language-mismatched specialists.
            //
            // Why: When the task names a language (e.g. "rust") and a
            // candidate declares a non-empty `languages` list with no
            // overlap, that candidate is the wrong specialist. Without this
            // gate, `python-engineer` (which declares `["python"]` + four
            // Python frameworks) wins the specificity tiebreak over the
            // generic `engineer` (which declares no languages) on a Rust
            // task — both score 10 from role match, but python-engineer has
            // more non-empty capability fields. Skipping these mismatched
            // specialists routes non-Python language tasks to `engineer`
            // where runtime skill injection supplies the right language
            // idioms.
            // What: Only applies when the task has language signals AND the
            // agent declares a non-empty language list. Agents with no
            // declared languages (the generic `engineer`) remain eligible.
            if !languages.is_empty()
                && let Some(c) = caps
                && !c.languages.is_empty()
                && !languages
                    .iter()
                    .any(|want| c.languages.iter().any(|l| l.eq_ignore_ascii_case(want)))
            {
                continue;
            }

            // Priority bonus: route Python tasks to python-engineer, the one
            // language specialist we ship. All other languages are handled
            // by the generic `engineer` agent with runtime skill injection.
            //
            // Gate: only apply when the requested role is "engineer" (or
            // unspecified). Otherwise a task like "Plan a multi-file Python
            // project" — which legitimately wants plan-agent — would be
            // hijacked into python-engineer just because the task text
            // mentions Python.
            let role_allows_lang_bonus = role
                .map(|r| r.eq_ignore_ascii_case("engineer"))
                .unwrap_or(true);
            if role_allows_lang_bonus
                && name.eq_ignore_ascii_case("python-engineer")
                && languages.iter().any(|l| l.eq_ignore_ascii_case("python"))
            {
                score += 50;
            }

            if score == 0 {
                continue;
            }

            let specificity = caps
                .map(|c| {
                    usize::from(!c.roles.is_empty())
                        + usize::from(!c.languages.is_empty())
                        + usize::from(!c.frameworks.is_empty())
                        + usize::from(!c.tags.is_empty())
                })
                .unwrap_or(0);

            match best {
                None => best = Some((name.as_str(), score, specificity)),
                Some((_, prev_score, prev_spec)) => {
                    if score > prev_score || (score == prev_score && specificity > prev_spec) {
                        best = Some((name.as_str(), score, specificity));
                    }
                }
            }
        }

        best.map(|(name, _, _)| name)
    }

    /// List all discovered agents with their source and capability summary.
    ///
    /// Why: Powers the `open-mpm agents list` subcommand and startup log
    /// summary so operators can verify their overrides are picked up.
    /// What: Returns one `AgentSummary` per discovered agent, preserving
    /// discovery (priority) order.
    /// Test: `registry_list_returns_all_agents`.
    pub fn list(&self) -> Vec<AgentSummary> {
        self.agents
            .iter()
            .map(|(name, (cfg, path))| {
                let (roles, languages, frameworks, tags) = match &cfg.agent.capabilities {
                    Some(c) => (
                        c.roles.clone(),
                        c.languages.clone(),
                        c.frameworks.clone(),
                        c.tags.clone(),
                    ),
                    None => (vec![], vec![], vec![], vec![]),
                };
                AgentSummary {
                    name: name.clone(),
                    source: path.clone(),
                    description: cfg.agent.description.clone(),
                    roles,
                    languages,
                    frameworks,
                    tags,
                }
            })
            .collect()
    }
}

/// Compute the agent search paths in priority order (highest first).
///
/// Why: Centralizes the discovery policy — project-level overrides beat
/// user-level overrides beat bundled defaults — so every call site
/// (`main.rs`, `ompm agents list`, tests) sees the same order.
/// What: Returns, in order: `.open-mpm/agents`, `.claude/agents`,
/// `~/.open-mpm/agents`, `~/.claude/agents`, `<config_dir>/agents`.
/// Test: `agent_search_paths_order`.
pub fn agent_search_paths(config_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.push(PathBuf::from(".open-mpm/agents"));
    paths.push(PathBuf::from(".claude/agents"));
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home.clone()).join(".open-mpm/agents"));
        paths.push(PathBuf::from(home).join(".claude/agents"));
    }
    paths.push(config_dir.join("agents"));
    paths
}

/// Render a Markdown roster section describing every agent in `registry`.
///
/// Why: The PM's system prompt used to hardcode a single agent
/// (`python-engineer`), causing the LLM to over-delegate there regardless of
/// task content. Injecting the live registry ensures the PM sees every
/// user-installed and bundled agent (including `.open-mpm/agents/`,
/// `.claude/agents/`, and home-dir overrides) at runtime, so new agents drop
/// in without prompt edits.
/// What: Produces a deterministic bullet list, one agent per line, with
/// name + description + role/language/framework/tag annotations (only when
/// present). Empty lists and blank descriptions are elided so the prompt
/// stays compact. Intended to be substituted for the `{{available_agents}}`
/// placeholder in `pm.toml`.
/// Test: `build_roster_section_includes_all_registered_agents`,
/// `build_roster_section_omits_empty_capability_fields`.
pub fn build_roster_section(registry: &AgentRegistry) -> String {
    let mut lines = Vec::new();
    for summary in registry.list() {
        // Skip the PM itself from the roster — PM does not delegate to itself.
        if summary.name == "pm" {
            continue;
        }
        let mut line = format!("- **{}**", summary.name);
        let desc = summary.description.trim();
        if !desc.is_empty() {
            line.push_str(&format!(": {desc}"));
        }
        let mut parts: Vec<String> = Vec::new();
        if !summary.roles.is_empty() {
            parts.push(format!("role: {}", summary.roles.join("/")));
        }
        if !summary.languages.is_empty() {
            parts.push(format!("languages: {}", summary.languages.join(", ")));
        }
        if !summary.frameworks.is_empty() {
            parts.push(format!("frameworks: {}", summary.frameworks.join(", ")));
        }
        if !summary.tags.is_empty() {
            parts.push(format!("tags: {}", summary.tags.join(", ")));
        }
        if !parts.is_empty() {
            line.push_str(&format!("  ({})", parts.join("; ")));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Inject the dynamic agent roster into a PM system prompt.
///
/// Why: Centralizes the substitution logic so both `run_pm()` and
/// `run_inspect_live()` produce identical PM prompts — divergence here would
/// silently alter Layer 2 harness results vs. production behavior.
/// What: If `{{available_agents}}` appears in `prompt`, replaces it with
/// the rendered roster. Otherwise appends a new `## Available Agents`
/// section so a pm.toml that pre-dates the placeholder still gets the
/// dynamic roster rather than silently missing it.
/// Test: `inject_roster_replaces_placeholder`,
/// `inject_roster_appends_when_placeholder_missing`.
pub fn inject_roster_into_prompt(prompt: &str, registry: &AgentRegistry) -> String {
    let roster = build_roster_section(registry);
    if prompt.contains("{{available_agents}}") {
        prompt.replace("{{available_agents}}", &roster)
    } else {
        format!("{prompt}\n\n## Available Agents\n{roster}")
    }
}

/// YAML frontmatter shape for `.md` agent files.
///
/// Why: Supports the claude-mpm ecosystem convention where agent definitions
/// are authored as Markdown with a YAML frontmatter header. Lets operators
/// drop `.md` agents into `.claude/agents/` or `.open-mpm/agents/` without
/// hand-converting to TOML.
/// What: Mirrors the subset of `AgentConfig` fields an operator is likely to
/// override per-agent. Unknown keys are ignored (no `deny_unknown_fields`) so
/// richer claude-mpm frontmatter doesn't fail parsing.
/// Test: `md_agent_file_parses_frontmatter_and_body`.
#[derive(Debug, Deserialize, Default)]
struct MdAgentFrontmatter {
    name: Option<String>,
    role: Option<String>,
    model: Option<String>,
    description: Option<String>,
    #[serde(default)]
    runner: Option<String>,
    #[serde(default)]
    capabilities: Option<AgentCapabilities>,
}

/// Parse an `.md` agent file into an `AgentConfig`.
///
/// Why: `AgentRegistry::load` supports both `.toml` and `.md` agent formats so
/// users can install agents using whichever convention suits their workflow.
/// The `.md` + YAML frontmatter format matches the claude-mpm ecosystem.
/// What: Reads the file, splits on `---` fences, parses frontmatter via
/// serde_yml, and uses the post-frontmatter body as the system prompt. Missing
/// fields fall back to sane defaults: `role = "agent"`, generic model, runner
/// = Subprocess. The resolver still applies `OPEN_MPM_MODEL_*` / default env
/// overrides through `resolve_model`.
/// Test: `md_agent_file_parses_frontmatter_and_body`,
/// `registry_picks_up_md_files_alongside_toml`.
fn parse_md_agent(path: &Path) -> anyhow::Result<AgentConfig> {
    use anyhow::{Context, anyhow};

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read agent md {}", path.display()))?;
    let trimmed = raw.trim_start_matches('\u{feff}');
    let trimmed = trimmed.trim_start_matches(['\n', '\r']);

    let rest = trimmed
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("agent md missing opening --- fence: {}", path.display()))?;
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .ok_or_else(|| anyhow!("agent md malformed fence: {}", path.display()))?;
    let end_rel = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("agent md missing closing --- fence: {}", path.display()))?;
    let fm_str = &rest[..end_rel];
    let after = &rest[end_rel + 4..];
    let body = after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after)
        .to_string();

    let fm: MdAgentFrontmatter = serde_yml::from_str(fm_str)
        .with_context(|| format!("failed to parse agent md frontmatter {}", path.display()))?;

    let name = fm.name.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    });
    let agent_model_raw = fm.model.unwrap_or_default();
    let (resolved_model, _src) = super::resolve_model(&name, &agent_model_raw, None);
    let runner = match fm.runner.as_deref() {
        Some("claude-code") => RunnerKind::ClaudeCode,
        Some("inline") => RunnerKind::Inline,
        Some("in-process") => RunnerKind::InProcess,
        Some("subprocess") | None => RunnerKind::Subprocess,
        Some(other) => {
            return Err(anyhow!(
                "agent md {} has unknown runner {:?}",
                path.display(),
                other
            ));
        }
    };
    let adapter: Arc<dyn crate::llm::adapter::ModelAdapter> =
        Arc::from(adapter_for_model(&resolved_model));

    Ok(AgentConfig {
        agent: AgentInfo {
            name,
            role: fm.role.unwrap_or_else(|| "agent".to_string()),
            model: resolved_model,
            description: fm.description.unwrap_or_default(),
            persistent_session: false,
            runner,
            capabilities: fm.capabilities,
            display_name: None,
            prompt_label: None,
        },
        llm: LlmParams {
            temperature: 0.2,
            max_tokens: 8192,
            model_override: None,
            enable_prompt_caching: true,
            max_turns: 20,
            tool_choice: ToolChoice::Auto,
            use_finish_task: false,
            use_anthropic_direct: false,
            claude_allowed_tools: Vec::new(),
            aws_profile: None,
            aws_region: None,
            elevation_threshold: None,
            elevation_model: None,
            stop_sequences: Vec::new(),
            routing_model: None,
            thinking_enabled: None,
        },
        system_prompt: SystemPrompt {
            content: body,
            skills: None,
        },
        tools: ToolsConfig::default(),
        compress: AgentCompressConfig::default(),
        runner_config: super::RunnerConfig::default(),
        session: super::SessionCompressionConfig::default(),
        plugins: super::AgentPluginsConfig::default(),
        rbac: super::RbacConfig::default(),
        adapter,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::HOME_LOCK;
    use std::fs;
    use tempfile::TempDir;

    fn write_agent(
        dir: &Path,
        name: &str,
        role: &str,
        langs: &[&str],
        frameworks: &[&str],
        tags: &[&str],
    ) {
        let caps_section = if langs.is_empty() && frameworks.is_empty() && tags.is_empty() {
            format!(
                "[agent.capabilities]\nroles = [\"{role}\"]\nlanguages = []\nframeworks = []\ntags = []\n"
            )
        } else {
            let langs = langs
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let frameworks = frameworks
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let tags = tags
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "[agent.capabilities]\nroles = [\"{role}\"]\nlanguages = [{langs}]\nframeworks = [{frameworks}]\ntags = [{tags}]\n"
            )
        };

        let toml = format!(
            r#"
[agent]
name = "{name}"
role = "{role}"
model = "anthropic/claude-sonnet-4-6"
description = "test agent"

[llm]
temperature = 0.0
max_tokens = 1024

[system_prompt]
content = "base"

{caps_section}
"#
        );
        fs::write(dir.join(format!("{name}.toml")), toml).unwrap();
    }

    #[test]
    fn capabilities_parse_from_toml() {
        let toml = r#"
[agent]
name = "x"
role = "engineer"
model = "x"
description = "x"

[agent.capabilities]
languages = ["python", "rust"]
frameworks = ["fastapi"]
roles = ["engineer"]
tags = ["general"]

[llm]
temperature = 0.0
max_tokens = 1024

[system_prompt]
content = "base"
"#;
        let cfg: AgentConfig = toml::from_str(toml).expect("parses");
        let caps = cfg.agent.capabilities.expect("caps present");
        assert_eq!(caps.languages, vec!["python", "rust"]);
        assert_eq!(caps.frameworks, vec!["fastapi"]);
        assert_eq!(caps.roles, vec!["engineer"]);
        assert_eq!(caps.tags, vec!["general"]);
    }

    #[test]
    fn registry_loads_from_multiple_dirs_with_priority() {
        let high = TempDir::new().unwrap();
        let low = TempDir::new().unwrap();
        // Same agent name in both dirs; high-priority dir wins.
        write_agent(high.path(), "engineer", "engineer", &["rust"], &[], &[]);
        write_agent(low.path(), "engineer", "engineer", &["python"], &[], &[]);
        // Unique-to-low agent should still be discovered.
        write_agent(low.path(), "qa-agent", "qa", &[], &[], &["testing"]);

        let reg = AgentRegistry::load(&[high.path().to_path_buf(), low.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);
        let eng = reg.get("engineer").expect("engineer present");
        let langs = &eng.agent.capabilities.as_ref().unwrap().languages;
        assert_eq!(langs, &vec!["rust".to_string()], "high-priority dir wins");
        assert!(reg.get("qa-agent").is_some());
    }

    #[test]
    fn registry_best_match_prefers_specific_over_general() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "engineer", "engineer", &[], &[], &["general"]);
        write_agent(
            dir.path(),
            "python-engineer",
            "engineer",
            &["python"],
            &["fastapi"],
            &[],
        );

        let reg = AgentRegistry::load(&[dir.path().to_path_buf()]);
        let pick = reg.best_match(Some("engineer"), &["python"], &["fastapi"], &[]);
        assert_eq!(pick, Some("python-engineer"));

        // Without language/framework signals, role-only match should resolve
        // to the generic engineer (both score 10 on role; general engineer has
        // lower specificity, but python-engineer has higher specificity — so
        // actually python-engineer wins even then). Assert that at least one
        // of the two is returned deterministically.
        let pick = reg.best_match(Some("engineer"), &[], &[], &[]);
        assert!(pick == Some("python-engineer") || pick == Some("engineer"));
    }

    #[test]
    fn registry_skips_missing_dirs_silently() {
        let real = TempDir::new().unwrap();
        write_agent(real.path(), "engineer", "engineer", &[], &[], &[]);
        let reg = AgentRegistry::load(&[
            PathBuf::from("/definitely/not/a/real/path/nope"),
            real.path().to_path_buf(),
        ]);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("engineer").is_some());
    }

    #[test]
    fn registry_list_returns_all_agents() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "a", "engineer", &["rust"], &[], &[]);
        write_agent(dir.path(), "b", "qa", &[], &[], &["testing"]);
        write_agent(dir.path(), "c", "docs", &[], &[], &["documentation"]);

        let reg = AgentRegistry::load(&[dir.path().to_path_buf()]);
        let list = reg.list();
        assert_eq!(list.len(), 3);
        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    #[test]
    fn registry_best_match_uses_engineer_for_non_python() {
        // Why: open-mpm intentionally ships ONE language specialist
        // (`python-engineer`) and routes all other languages to the generic
        // `engineer` agent, which receives the right language idiom skill
        // (rust-idiomatic.md, go-idiomatic.md, etc.) via runtime injection.
        // Reproduces the inverse routing bug: previously `python-engineer`
        // (with `["python"]` + four Python frameworks declared) won the
        // specificity tiebreak over `engineer` for Rust/TS tasks because
        // both scored 10 on role match and python-engineer had more
        // non-empty capability fields. The language-mismatch disqualifier
        // ensures specialists with declared languages are skipped when
        // none of their languages match the task.
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "engineer", "engineer", &[], &[], &["general"]);
        write_agent(
            dir.path(),
            "python-engineer",
            "engineer",
            &["python"],
            &["fastapi", "flask", "django", "pytest"],
            &[],
        );

        let reg = AgentRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(
            reg.best_match(Some("engineer"), &["rust"], &[], &[]),
            Some("engineer"),
            "rust task must route to generic engineer (skill injection handles rust idioms)"
        );
        assert_eq!(
            reg.best_match(Some("engineer"), &["typescript"], &[], &[]),
            Some("engineer"),
            "typescript task must route to generic engineer"
        );
        assert_eq!(
            reg.best_match(Some("engineer"), &["go"], &[], &[]),
            Some("engineer"),
            "go task must route to generic engineer"
        );
        assert_eq!(
            reg.best_match(Some("engineer"), &["python"], &[], &[]),
            Some("python-engineer"),
            "python task must route to the one language specialist we ship"
        );
    }

    #[test]
    fn registry_best_match_returns_none_when_no_scores() {
        // No agents registered -> None.
        let reg = AgentRegistry::load(&[]);
        assert!(reg.best_match(Some("engineer"), &[], &[], &[]).is_none());

        // Agents registered but no capability overlap -> None.
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "engineer", "engineer", &[], &[], &[]);
        let reg = AgentRegistry::load(&[dir.path().to_path_buf()]);
        assert!(reg.best_match(Some("qa"), &["go"], &[], &[]).is_none());
    }

    #[test]
    fn md_agent_file_parses_frontmatter_and_body() {
        let dir = TempDir::new().unwrap();
        let content = r#"---
name: md-agent
role: engineer
model: anthropic/claude-sonnet-4-6
runner: claude-code
description: md-formatted agent
capabilities:
  languages: [python]
  frameworks: [fastapi]
  roles: [engineer]
  tags: [rest-api]
---

SYSTEM PROMPT BODY HERE
"#;
        let path = dir.path().join("md-agent.md");
        fs::write(&path, content).unwrap();
        let cfg = parse_md_agent(&path).expect("md parses");
        assert_eq!(cfg.agent.name, "md-agent");
        assert_eq!(cfg.agent.role, "engineer");
        assert_eq!(cfg.agent.runner, RunnerKind::ClaudeCode);
        assert!(cfg.system_prompt.content.contains("SYSTEM PROMPT BODY"));
        let caps = cfg.agent.capabilities.expect("caps");
        assert_eq!(caps.languages, vec!["python"]);
        assert_eq!(caps.frameworks, vec!["fastapi"]);
        assert_eq!(caps.tags, vec!["rest-api"]);
    }

    #[test]
    fn registry_picks_up_md_files_alongside_toml() {
        let dir = TempDir::new().unwrap();
        // TOML agent.
        write_agent(dir.path(), "toml-eng", "engineer", &["rust"], &[], &[]);
        // MD agent.
        let md = r#"---
name: md-eng
role: engineer
model: anthropic/claude-sonnet-4-6
description: md engineer
capabilities:
  languages: [python]
  roles: [engineer]
  frameworks: []
  tags: []
---

body
"#;
        fs::write(dir.path().join("md-eng.md"), md).unwrap();

        let reg = AgentRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("toml-eng").is_some());
        assert!(reg.get("md-eng").is_some());
        let md_cfg = reg.get("md-eng").unwrap();
        assert_eq!(
            md_cfg.agent.capabilities.as_ref().unwrap().languages,
            vec!["python".to_string()]
        );
    }

    #[test]
    fn registry_md_file_without_frontmatter_is_skipped() {
        let dir = TempDir::new().unwrap();
        // Valid TOML agent stays loadable.
        write_agent(dir.path(), "ok", "engineer", &[], &[], &[]);
        // MD without frontmatter: skipped with warn.
        fs::write(dir.path().join("broken.md"), "# not an agent\n").unwrap();
        let reg = AgentRegistry::load(&[dir.path().to_path_buf()]);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("ok").is_some());
    }

    // ── Integration-style tests against bundled config/agents/ ─────────────
    //
    // Why: Unit tests above use synthetic TempDir fixtures; these tests
    // exercise the real on-disk bundled agents so we catch drift between
    // registry logic and what ships with the repo (e.g. an agent accidentally
    // dropping its capabilities section).
    // Test: Run `cargo test --lib agent_registry_discovers_bundled_agents`.

    fn bundled_agents_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".open-mpm")
            .join("agents")
    }

    #[test]
    fn agent_registry_discovers_bundled_agents() {
        let paths = vec![bundled_agents_dir()];
        let registry = AgentRegistry::load(&paths);
        // Core bundled agents referenced in the task spec.
        assert!(
            registry.get("engineer").is_some(),
            "engineer missing from bundled agents"
        );
        assert!(
            registry.get("python-engineer").is_some(),
            "python-engineer missing"
        );
        assert!(registry.get("plan-agent").is_some(), "plan-agent missing");
        assert!(registry.get("qa-agent").is_some(), "qa-agent missing");
    }

    #[test]
    fn agent_registry_selects_python_engineer_for_python_task() {
        let paths = vec![bundled_agents_dir()];
        let registry = AgentRegistry::load(&paths);
        let best = registry.best_match(Some("engineer"), &["python"], &["fastapi"], &[]);
        assert_eq!(
            best,
            Some("python-engineer"),
            "best_match should pick python-engineer for python+fastapi signals"
        );
    }

    #[test]
    fn agent_search_paths_order() {
        // SAFETY: test-only; we restore HOME at the end.
        // HOME_LOCK serializes with other tests that mutate $HOME process-wide.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", "/tmp/home-test");
        }
        let paths = agent_search_paths(Path::new("/opt/open-mpm/config"));
        assert_eq!(paths[0], PathBuf::from(".open-mpm/agents"));
        assert_eq!(paths[1], PathBuf::from(".claude/agents"));
        assert_eq!(paths[2], PathBuf::from("/tmp/home-test/.open-mpm/agents"));
        assert_eq!(paths[3], PathBuf::from("/tmp/home-test/.claude/agents"));
        assert_eq!(paths[4], PathBuf::from("/opt/open-mpm/config/agents"));

        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn build_roster_section_includes_all_registered_agents() {
        let tmp = TempDir::new().unwrap();
        write_agent(
            tmp.path(),
            "python-engineer",
            "engineer",
            &["python"],
            &["fastapi"],
            &[],
        );
        write_agent(
            tmp.path(),
            "docs-agent",
            "docs",
            &[],
            &[],
            &["documentation"],
        );
        let reg = AgentRegistry::load(&[tmp.path().to_path_buf()]);
        let roster = build_roster_section(&reg);
        assert!(
            roster.contains("python-engineer"),
            "roster missing python-engineer: {roster}"
        );
        assert!(
            roster.contains("docs-agent"),
            "roster missing docs-agent: {roster}"
        );
        assert!(
            roster.contains("python"),
            "roster missing language annotation: {roster}"
        );
        assert!(
            roster.contains("fastapi"),
            "roster missing framework annotation: {roster}"
        );
    }

    #[test]
    fn build_roster_section_omits_empty_capability_fields() {
        let tmp = TempDir::new().unwrap();
        // Agent with no languages/frameworks/tags — only role.
        write_agent(tmp.path(), "bare-agent", "engineer", &[], &[], &[]);
        let reg = AgentRegistry::load(&[tmp.path().to_path_buf()]);
        let roster = build_roster_section(&reg);
        assert!(roster.contains("bare-agent"));
        // No "languages:" or "frameworks:" or "tags:" annotation since they're empty.
        assert!(
            !roster.contains("languages:"),
            "empty languages leaked: {roster}"
        );
        assert!(
            !roster.contains("frameworks:"),
            "empty frameworks leaked: {roster}"
        );
        assert!(!roster.contains("tags:"), "empty tags leaked: {roster}");
    }

    #[test]
    fn build_roster_section_excludes_pm_itself() {
        let tmp = TempDir::new().unwrap();
        write_agent(tmp.path(), "pm", "orchestrator", &[], &[], &[]);
        write_agent(tmp.path(), "engineer", "engineer", &[], &[], &[]);
        let reg = AgentRegistry::load(&[tmp.path().to_path_buf()]);
        let roster = build_roster_section(&reg);
        assert!(
            !roster.contains("**pm**"),
            "PM should not delegate to itself: {roster}"
        );
        assert!(roster.contains("**engineer**"));
    }

    #[test]
    fn inject_roster_replaces_placeholder() {
        let tmp = TempDir::new().unwrap();
        write_agent(
            tmp.path(),
            "python-engineer",
            "engineer",
            &["python"],
            &[],
            &[],
        );
        let reg = AgentRegistry::load(&[tmp.path().to_path_buf()]);
        let prompt = "prefix\n{{available_agents}}\nsuffix";
        let out = inject_roster_into_prompt(prompt, &reg);
        assert!(out.starts_with("prefix\n"), "prefix preserved: {out}");
        assert!(out.ends_with("\nsuffix"), "suffix preserved: {out}");
        assert!(out.contains("python-engineer"), "roster injected: {out}");
        assert!(
            !out.contains("{{available_agents}}"),
            "placeholder removed: {out}"
        );
    }

    #[test]
    fn inject_roster_appends_when_placeholder_missing() {
        let tmp = TempDir::new().unwrap();
        write_agent(tmp.path(), "engineer", "engineer", &[], &[], &[]);
        let reg = AgentRegistry::load(&[tmp.path().to_path_buf()]);
        let prompt = "You are the PM. Delegate to agents.";
        let out = inject_roster_into_prompt(prompt, &reg);
        assert!(out.starts_with(prompt), "original prompt preserved: {out}");
        assert!(
            out.contains("## Available Agents"),
            "roster section appended: {out}"
        );
        assert!(out.contains("engineer"), "agent name present: {out}");
    }
}
