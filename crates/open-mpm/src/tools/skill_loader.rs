//! `load_skill` and `list_skills` tools + filesystem-backed `SkillResolver`.
//!
//! Why: Agents need to pull in domain-specific Markdown guidance on demand.
//! A resolver abstraction lets tests use an in-memory map while production
//! walks a known directory hierarchy.
//! What:
//!   - `FsSkillResolver` implements `SkillResolver`. Search order:
//!       1. `{project_root}/.claude/skills/{name}/SKILL.md`
//!       2. `{home}/.claude/skills/{name}/SKILL.md`
//!       3. `{project_root}/config/skills/{name}.md`
//!   - `SkillLoaderTool` and `SkillListTool` wrap a resolver as
//!     `ToolExecutor`s.
//! Test: Place files in a tempdir, point a `FsSkillResolver` at it, verify
//! `resolve()` returns the content.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::skills::registry::SkillRegistry as TagSkillRegistry;
use crate::skills::{SkillRegistry, strip_frontmatter};
use crate::tools::traits::{SkillResolver, ToolExecutor, ToolResult};

/// Filesystem-backed skill resolver.
pub struct FsSkillResolver {
    project_root: PathBuf,
    home: Option<PathBuf>,
}

impl FsSkillResolver {
    /// Build a resolver from a project root and optional home dir.
    pub fn new(project_root: PathBuf, home: Option<PathBuf>) -> Self {
        Self { project_root, home }
    }

    /// Build with sensible defaults: CWD as project root, `$HOME` as home.
    pub fn from_defaults() -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::new(project_root, home)
    }

    fn candidate_paths(&self, name: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        paths.push(
            self.project_root
                .join(".claude")
                .join("skills")
                .join(name)
                .join("SKILL.md"),
        );
        if let Some(home) = &self.home {
            paths.push(
                home.join(".claude")
                    .join("skills")
                    .join(name)
                    .join("SKILL.md"),
            );
        }
        paths.push(
            self.project_root
                .join(".open-mpm")
                .join("skills")
                .join(format!("{name}.md")),
        );
        paths
    }
}

impl SkillResolver for FsSkillResolver {
    fn resolve(&self, name: &str) -> Option<String> {
        for p in self.candidate_paths(name) {
            if p.exists()
                && let Ok(s) = std::fs::read_to_string(&p)
            {
                return Some(s);
            }
        }
        None
    }

    fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        // Search .claude/skills/* (directories with SKILL.md)
        let bases = {
            let mut b: Vec<PathBuf> = vec![self.project_root.join(".claude").join("skills")];
            if let Some(home) = &self.home {
                b.push(home.join(".claude").join("skills"));
            }
            b
        };
        for base in bases {
            if let Ok(entries) = std::fs::read_dir(&base) {
                for e in entries.flatten() {
                    if e.path().join("SKILL.md").exists()
                        && let Some(name) = e.file_name().to_str()
                    {
                        out.push(name.to_string());
                    }
                }
            }
        }
        // .open-mpm/skills/*.md
        if let Ok(entries) = std::fs::read_dir(self.project_root.join(".open-mpm").join("skills")) {
            for e in entries.flatten() {
                if let Some(ext) = e.path().extension()
                    && ext == "md"
                    && let Some(stem) = e.path().file_stem().and_then(|s| s.to_str())
                {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// `load_skill` tool — resolves and returns a named skill's content.
///
/// Why: Gives LLM agents a direct path to domain knowledge without baking
/// that knowledge into every system prompt. When a registry is present the
/// tool also supports a `query` fallback so an agent that does not know the
/// exact skill name can still discover the most relevant one.
/// What: Primarily calls `SkillResolver::resolve(name)`. When `registry` is
/// `Some(_)` and the caller supplied a `query` instead of `name`, ranks the
/// registry entries and returns an index plus the best match body.
/// Test: See `tests` below — exact name resolution and missing-name errors
/// are covered by the resolver tests; query handling is covered by the
/// registry tests in `skills::tests`.
pub struct SkillLoaderTool {
    resolver: Arc<dyn SkillResolver>,
    registry: Option<Arc<SkillRegistry>>,
}

impl SkillLoaderTool {
    /// Build a loader that only knows how to resolve by exact path.
    #[allow(dead_code)]
    pub fn new(resolver: Arc<dyn SkillResolver>) -> Self {
        Self {
            resolver,
            registry: None,
        }
    }

    /// Build a loader backed by both an fs resolver (for `.claude/skills/`
    /// style lookups) and a pre-scanned registry (for query ranking and
    /// frontmatter-aware rendering of `config/skills/*.md`).
    pub fn with_registry(resolver: Arc<dyn SkillResolver>, registry: Arc<SkillRegistry>) -> Self {
        Self {
            resolver,
            registry: Some(registry),
        }
    }
}

#[async_trait]
impl ToolExecutor for SkillLoaderTool {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "load_skill",
                "description": "Load the full Markdown content of a named skill (domain knowledge module). \
                    Provide `name` (exact match from list_skills) OR `query` (natural-language description; \
                    the best-ranked skill will be returned along with a short list of near-matches).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Exact skill name from list_skills output."
                        },
                        "query": {
                            "type": "string",
                            "description": "Natural-language query used to rank skills when `name` is omitted."
                        }
                    },
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let name_arg = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let query_arg = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(name) = name_arg {
            // Prefer the registry when available so the returned body has the
            // YAML frontmatter stripped; fall back to the generic resolver.
            if let Some(reg) = self.registry.as_ref()
                && reg.skills.iter().any(|s| s.name == name)
            {
                return match reg.load_content(name).await {
                    Ok(content) => ToolResult::ok(format!(
                        "## Skill: {name}\n\n{}",
                        strip_frontmatter(&content)
                    )),
                    Err(e) => ToolResult::err(e.to_string()),
                };
            }
            return match self.resolver.resolve(name) {
                Some(s) => ToolResult::ok(s),
                None => ToolResult::err(format!("Skill '{name}' not found.")),
            };
        }

        if let Some(query) = query_arg {
            let Some(reg) = self.registry.as_ref() else {
                return ToolResult::err(
                    "load_skill: 'query' requires a skill registry; provide 'name' instead",
                );
            };
            let matches = reg.search(query, 3);
            if matches.is_empty() {
                return ToolResult::ok(
                    "No matching skills found. Use list_skills to see all available.",
                );
            }
            let index = matches
                .iter()
                .map(|s| format!("- **{}**: {}", s.name, s.description))
                .collect::<Vec<_>>()
                .join("\n");
            let best = &matches[0];
            let body = reg
                .load_content(&best.name)
                .await
                .map(|c| strip_frontmatter(&c).to_string())
                .unwrap_or_default();
            return ToolResult::ok(format!(
                "## Matching Skills\n{index}\n\n## Best Match: {}\n\n{body}",
                best.name
            ));
        }

        ToolResult::err("load_skill: provide either 'name' or 'query'")
    }
}

/// `list_skills` tool — enumerates available skill names (and descriptions
/// when a registry is provided).
///
/// Why: Agents need fast, deterministic skill discovery keyed by
/// language/framework/workflow tags (#168). When a tag-indexed registry is
/// available, results are ranked by tag-overlap score in O(1) per tag with
/// no LLM or embedding call on the path.
/// What: Holds an optional legacy `SkillRegistry` (for the pre-#168 fallback
/// rendering) AND an optional tag-indexed `TagSkillRegistry`. When `tags` are
/// supplied to the tool call, the tag registry wins; otherwise the tool
/// falls back to the legacy index or a flat resolver list.
/// Test: `list_skills_returns_by_tag` (in this module).
pub struct SkillListTool {
    resolver: Arc<dyn SkillResolver>,
    registry: Option<Arc<SkillRegistry>>,
    tag_registry: Option<Arc<TagSkillRegistry>>,
}

impl SkillListTool {
    /// Construct a lister backed by a resolver but without a rich registry.
    ///
    /// Why: Useful in tests and in call sites that don't yet own a
    /// `SkillRegistry`; the tool falls back to a flat name list.
    /// What: Plain struct literal with `registry = None`.
    /// Test: Implicit — covered by integration via the research-agent registry.
    #[allow(dead_code)]
    pub fn new(resolver: Arc<dyn SkillResolver>) -> Self {
        Self {
            resolver,
            registry: None,
            tag_registry: None,
        }
    }

    /// Build a lister that, when called, returns the registry's rich index
    /// (name + description + tags) instead of a flat name list.
    pub fn with_registry(resolver: Arc<dyn SkillResolver>, registry: Arc<SkillRegistry>) -> Self {
        Self {
            resolver,
            registry: Some(registry),
            tag_registry: None,
        }
    }

    /// Build a lister backed by BOTH the legacy registry (fallback rendering)
    /// AND the tag-indexed registry (#168) used for `tags`-parameter queries.
    ///
    /// #170: Wired into `build_registry_for_agent` in `src/main.rs` so every
    /// sub-agent's `list_skills` tool uses tag-ranked results when a non-empty
    /// `TagSkillRegistry` is available.
    pub fn with_tag_registry(
        resolver: Arc<dyn SkillResolver>,
        registry: Option<Arc<SkillRegistry>>,
        tag_registry: Arc<TagSkillRegistry>,
    ) -> Self {
        Self {
            resolver,
            registry,
            tag_registry: Some(tag_registry),
        }
    }
}

#[async_trait]
impl ToolExecutor for SkillListTool {
    fn name(&self) -> &str {
        "list_skills"
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "list_skills",
                "description": "List skills available to load. When `tags` are provided, \
                    returns skills ranked by tag-overlap score (most matching tags first). \
                    Without `tags`, returns all skills sorted alphabetically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tags": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Tag filter. Example: [\"python\",\"fastapi\",\"pytest\"]. \
                                Common tag categories: language (python,rust,typescript,go), \
                                framework (fastapi,axum,react,django,flask), \
                                testing (pytest,jest,testing), \
                                workflow (wave-planning,tdd,docker)."
                        }
                    },
                    "additionalProperties": false
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // Extract tag filter if any.
        let tag_strings: Vec<String> = args
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        // Tag-indexed path (#168): when a tag registry is present and the
        // caller supplied tags, rank by overlap and return structured JSON.
        if let Some(tag_reg) = self.tag_registry.as_ref() {
            if !tag_strings.is_empty() {
                let refs: Vec<&str> = tag_strings.iter().map(String::as_str).collect();
                let matches = tag_reg.find_by_tags(&refs);
                if matches.is_empty() {
                    return ToolResult::ok(
                        "{\"skills\":[],\"note\":\"no skills matched the given tags\"}".to_string(),
                    );
                }
                let items: Vec<Value> = matches
                    .iter()
                    .map(|m| {
                        let score = tag_reg.tag_overlap_score(&m.name, &refs);
                        json!({
                            "name": m.name,
                            "description": m.description,
                            "tags": m.tags,
                            "match_score": score,
                        })
                    })
                    .collect();
                return ToolResult::ok(json!({ "skills": items }).to_string());
            }
            // No tags → list all, alphabetically.
            let mut all: Vec<Value> = tag_reg
                .list()
                .into_iter()
                .map(|m| {
                    json!({
                        "name": m.name,
                        "description": m.description,
                        "tags": m.tags,
                    })
                })
                .collect();
            all.sort_by(|a, b| {
                a.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
            });
            return ToolResult::ok(json!({ "skills": all }).to_string());
        }

        // Legacy fallback paths (pre-#168 behavior).
        if let Some(reg) = self.registry.as_ref()
            && !reg.is_empty()
        {
            return ToolResult::ok(reg.format_index());
        }
        let names = self.resolver.list();
        if names.is_empty() {
            return ToolResult::ok("(no skills available)");
        }
        ToolResult::ok(names.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn fs_resolver_reads_open_mpm_skills_md() {
        let tmp = tempdir();
        let skills_dir = tmp.join(".open-mpm").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        let skill_path = skills_dir.join("foo.md");
        fs::write(&skill_path, "hello skill").unwrap();

        let resolver = FsSkillResolver::new(tmp.clone(), None);
        let got = resolver.resolve("foo").expect("should find skill");
        assert_eq!(got, "hello skill");
    }

    #[test]
    fn fs_resolver_reads_claude_skills_dir() {
        let tmp = tempdir();
        let skill_dir = tmp.join(".claude").join("skills").join("bar");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "bar content").unwrap();

        let resolver = FsSkillResolver::new(tmp.clone(), None);
        let got = resolver.resolve("bar").expect("should find skill");
        assert_eq!(got, "bar content");
    }

    #[test]
    fn fs_resolver_returns_none_for_unknown() {
        let tmp = tempdir();
        let resolver = FsSkillResolver::new(tmp, None);
        assert!(resolver.resolve("doesnotexist").is_none());
    }

    #[test]
    fn fs_resolver_list_enumerates_skills() {
        let tmp = tempdir();
        let skills_dir = tmp.join(".open-mpm").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(skills_dir.join("a.md"), "a").unwrap();
        fs::write(skills_dir.join("b.md"), "b").unwrap();
        let skill_dir = tmp.join(".claude").join("skills").join("c");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "c").unwrap();

        let resolver = FsSkillResolver::new(tmp.clone(), None);
        let list = resolver.list();
        assert!(list.contains(&"a".to_string()));
        assert!(list.contains(&"b".to_string()));
        assert!(list.contains(&"c".to_string()));
    }

    fn tempdir() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("open-mpm-skill-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[tokio::test]
    async fn list_skills_returns_by_tag() {
        let tmp = tempdir();
        fs::write(
            tmp.join("fastapi.md"),
            "---\nname: fastapi\ndescription: async routes\ntags: [python, fastapi]\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            tmp.join("pytest.md"),
            "---\nname: pytest\ndescription: fixtures\ntags: [python, pytest]\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            tmp.join("rust.md"),
            "---\nname: rust\ndescription: rust idioms\ntags: [rust]\n---\nbody\n",
        )
        .unwrap();

        let tag_reg = Arc::new(TagSkillRegistry::load(std::slice::from_ref(&tmp)));
        let resolver: Arc<dyn SkillResolver> = Arc::new(FsSkillResolver::new(tmp.clone(), None));
        let tool = SkillListTool::with_tag_registry(resolver, None, tag_reg);

        let result = tool.execute(json!({"tags": ["python"]})).await;
        let content = result.content();
        assert!(content.contains("\"fastapi\""));
        assert!(content.contains("\"pytest\""));
        assert!(
            !content.contains("\"rust\""),
            "rust has no python tag; should be filtered out: {content}"
        );

        // Multi-tag: overlap score should rank fastapi (2) above pytest (1).
        let result = tool.execute(json!({"tags": ["python", "fastapi"]})).await;
        let content = result.content();
        let fastapi_pos = content.find("\"fastapi\"").unwrap();
        let pytest_pos = content.find("\"pytest\"").unwrap();
        assert!(
            fastapi_pos < pytest_pos,
            "fastapi should rank first: {content}"
        );
        assert!(content.contains("\"match_score\":2"));
    }

    #[tokio::test]
    async fn list_skills_without_tags_returns_all_alphabetical() {
        let tmp = tempdir();
        fs::write(
            tmp.join("b.md"),
            "---\nname: b-skill\ndescription: d\ntags: [t]\n---\nbody\n",
        )
        .unwrap();
        fs::write(
            tmp.join("a.md"),
            "---\nname: a-skill\ndescription: d\ntags: [t]\n---\nbody\n",
        )
        .unwrap();
        let tag_reg = Arc::new(TagSkillRegistry::load(std::slice::from_ref(&tmp)));
        let resolver: Arc<dyn SkillResolver> = Arc::new(FsSkillResolver::new(tmp.clone(), None));
        let tool = SkillListTool::with_tag_registry(resolver, None, tag_reg);

        let result = tool.execute(json!({})).await;
        let content = result.content();
        let a_pos = content.find("\"a-skill\"").expect("a-skill in output");
        let b_pos = content.find("\"b-skill\"").expect("b-skill in output");
        assert!(a_pos < b_pos, "alphabetical order expected: {content}");
    }
}
