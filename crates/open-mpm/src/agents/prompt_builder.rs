//! Layered system prompt assembly with CLAUDE.md ancestor walk.
//!
//! Why: An agent's effective system prompt is the sum of several sources:
//! (1) the base TOML `system_prompt.content`, (2) project-level CLAUDE.md
//! files walked from the working directory up to the filesystem root and
//! `$HOME/.claude/CLAUDE.md`, (3) user memory (cross-project preferences and
//! learnings), (4) resolved skills, and (5) any sub-agent context injected by
//! the orchestrator. Concatenating these inline across call sites leads to
//! drift; a builder enforces a single canonical layout.
//! What: `SystemPromptBuilder` accumulates layers in well-defined slots and
//! `build()` concatenates them with a stable separator (`"\n\n---\n\n"`).
//! Test: Unit tests verify the ancestor walk finds CLAUDE.md files, that
//! home-level instructions come before project-level, and that `layer_count`
//! and `build()` produce the expected output.

use std::path::{Path, PathBuf};

/// Separator placed between non-project-instruction layers. Markdown horizontal
/// rule keeps boundaries readable when the composed prompt is echoed to logs.
pub const LAYER_SEPARATOR: &str = "\n\n---\n\n";

// INTENT: Build a labeled separator naming the source file of the following layer.
pub fn labeled_separator(path: &Path) -> String {
    format!("\n\n--- [from: {}] ---\n\n", path.display())
}

// Harness protocol content is compiled into the binary via
// `crate::agents::harness_protocol`. The previous file-loading helpers
// (`harness_dir` / `load_harness_layer` / `load_harness_layer_from`) that
// read from `config/harness/*.md` were removed because harness protocol is
// binary-level behavior — not user config. See `harness_protocol.rs`.

/// Builder for a layered system prompt.
///
/// Slot ordering (build output):
///   goal_block → harness_layers → base → project_layers → user_memory_layer → skill_layers → subagent_layers
///
/// User memory sits after project instructions so project-specific knowledge
/// takes precedence, but before skills so that user preferences can influence
/// how skill guidance is interpreted by the model.
///
/// Harness layers sit between the goal block and the base (TOML) prompt so
/// that runtime-protocol rules — how to call `write_file`, when to call
/// `finish_task`, the absolute `out_dir` requirement, the `## Summary`
/// footer — are established BEFORE the agent's role-specific prompt. These
/// are sourced from `crate::agents::harness_protocol` constants (compiled
/// into the binary) and injected by the engine based on the agent's
/// runner/finish_task configuration, replacing ten copy-pasted sections
/// that used to live in individual agent TOMLs.
pub struct SystemPromptBuilder {
    base: String,
    harness_layers: Vec<String>,
    project_layers: Vec<(PathBuf, String)>,
    user_memory_layer: Option<String>,
    skill_layers: Vec<String>,
    subagent_layers: Vec<String>,
    /// Goal block prepended above the base prompt (#68). When `Some` and
    /// non-empty, rendered via `GoalBlock::to_prompt_header` followed by the
    /// standard `LAYER_SEPARATOR` before the base.
    goal_block: Option<crate::context::GoalBlock>,
    /// Project-memory layer (#241). Rendered between harness layers and base
    /// so the agent sees prior project context as preamble before its
    /// role-specific prompt. Distinct from `user_memory_layer`, which holds
    /// cross-project user preferences and sits *after* project instructions.
    memory_layer: Option<String>,
    /// MCP tool listing (#241). Rendered after skills so the model first reads
    /// its role + skills then learns about the external MCP services it can
    /// call. Role-gated by the caller (see `crate::mcp::McpConfig`).
    mcp_layer: Option<String>,
    /// Caveman-style output-compression instruction (#420). When set to a
    /// non-`None` style, the rendered fragment is appended as the final layer
    /// so the rules govern every subsequent response without being diluted
    /// by intervening content.
    output_style: crate::compress::OutputStyle,
}

impl SystemPromptBuilder {
    // INTENT: Seed the builder with the agent's configured base prompt.
    pub fn new(base: impl Into<String>) -> Self {
        // Inject runtime context variables into the system prompt (#367).
        // {{OPEN_MPM_VERSION}} → current binary version from Cargo.toml so
        // agents like ctrl can answer "what version?" without needing a tool.
        let base = base
            .into()
            .replace("{{OPEN_MPM_VERSION}}", env!("CARGO_PKG_VERSION"));
        Self {
            base,
            harness_layers: Vec::new(),
            project_layers: Vec::new(),
            user_memory_layer: None,
            skill_layers: Vec::new(),
            subagent_layers: Vec::new(),
            goal_block: None,
            memory_layer: None,
            mcp_layer: None,
            output_style: crate::compress::OutputStyle::None,
        }
    }

    /// Substitute the agent-identity template variables `{{AGENT_MODEL}}` and
    /// `{{AGENT_RUNNER}}` in the base prompt (#478).
    ///
    /// Why: `{{OPEN_MPM_VERSION}}` is substituted in `new()` so agents can
    /// answer "what version?", but agents had no way to learn their own model
    /// or runner — ctrl in particular skips the deployment footer entirely, so
    /// it could not answer "what model am I?". These placeholders let any
    /// agent's TOML embed its model/runner directly in the identity line.
    /// What: Replaces every `{{AGENT_MODEL}}` and `{{AGENT_RUNNER}}` occurrence
    /// in the already-seeded `base` string. Chained after `new()` so the
    /// constructor signature stays non-breaking for the ~30 existing callers.
    /// Test: `agent_model_substituted_in_prompt`,
    /// `agent_runner_substituted_in_prompt`.
    pub fn with_agent_context(mut self, model: &str, runner: &str) -> Self {
        self.base = self
            .base
            .replace("{{AGENT_MODEL}}", model)
            .replace("{{AGENT_RUNNER}}", runner);
        self
    }

    /// Set the caveman-style output-compression instruction (#420).
    ///
    /// Why: Sub-agent output tokens are the largest single cost lever in a
    /// multi-turn session. A short rule block at the end of the system prompt
    /// reduces output verbosity 22–87% with no accuracy loss.
    /// What: Stores the style; `build()` appends the matching fragment as the
    /// final layer. Default is `OutputStyle::None` (no fragment appended).
    /// Test: `output_style_full_appended_at_end`,
    /// `output_style_none_appends_nothing`.
    pub fn with_output_style(mut self, style: crate::compress::OutputStyle) -> Self {
        self.output_style = style;
        self
    }

    // INTENT: Append a harness-protocol layer (write_file / finish_task /
    // out_dir / summary rules) that applies to every agent of the matching
    // runner. Empty content is dropped. Harness layers are injected between
    // the goal block and the base (TOML) prompt at build time.
    pub fn add_harness_layer(mut self, content: impl Into<String>) -> Self {
        let text = content.into();
        if !text.trim().is_empty() {
            self.harness_layers.push(text);
        }
        self
    }

    // INTENT: Attach a goal block that will be prepended to the built prompt.
    #[allow(dead_code)]
    pub fn with_goal_block(mut self, block: Option<crate::context::GoalBlock>) -> Self {
        self.goal_block = block.filter(|b| !b.is_empty());
        self
    }

    // INTENT: Inject user memory (cross-project preferences) after project layers but before skills.
    #[allow(dead_code)]
    pub fn with_user_memory(mut self, content: impl Into<String>) -> Self {
        let text = content.into();
        if !text.trim().is_empty() {
            self.user_memory_layer = Some(text);
        }
        self
    }

    // INTENT: Walk ancestor directories collecting CLAUDE.md and AGENTS.md project instruction files.
    pub fn walk_project_instructions(mut self, start: &Path) -> Self {
        // Home-level instructions go first (outermost scope).
        if let Some(home) = std::env::var_os("HOME") {
            let home_dir = PathBuf::from(home).join(".claude");
            for fname in ["CLAUDE.md", "AGENTS.md"] {
                let p = home_dir.join(fname);
                if p.exists() {
                    self.push_if_nonempty(p);
                }
            }
        }

        // Collect ancestor dirs leaf-to-root, then reverse so root-first.
        let mut ancestors: Vec<PathBuf> = Vec::new();
        let mut cur: Option<&Path> = Some(start);
        while let Some(c) = cur {
            ancestors.push(c.to_path_buf());
            cur = c.parent();
        }
        // Reverse so shallowest (filesystem root) comes first — #31.
        ancestors.reverse();

        for dir in ancestors {
            for fname in ["CLAUDE.md", "AGENTS.md"] {
                let p = dir.join(fname);
                if p.exists() {
                    self.push_if_nonempty(p);
                }
            }
        }

        self
    }

    // INTENT: Read a candidate instruction file and push it as a project layer if non-empty.
    fn push_if_nonempty(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                self.project_layers.push((path, text));
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "skipping instruction file: read failed");
            }
        }
    }

    // INTENT: Append one resolved skill's content as its own layer.
    pub fn add_skill(mut self, content: impl Into<String>) -> Self {
        self.skill_layers.push(content.into());
        self
    }

    /// Append an MCP-tool description block as its own layer (#241).
    ///
    /// Why: Coordinating agents (ctrl, PM, research, observe) need to know
    /// which external MCP services they can call. The block is rendered
    /// once by `crate::mcp::McpConfig::render_prompt_section` and injected
    /// here so prompt assembly stays in one place.
    /// What: Stores the rendered Markdown; empty/whitespace input is dropped.
    /// Test: `mcp_layer_appears_after_skills_before_subagent`.
    pub fn add_mcp_layer(mut self, section: impl Into<String>) -> Self {
        let text = section.into();
        if !text.trim().is_empty() {
            self.mcp_layer = Some(text);
        }
        self
    }

    /// Append a project-memory layer with recalled context strings (#241).
    ///
    /// Why: ctrl seeds prompts with relevant memories from kuzu-memory so
    /// the model is grounded in prior project context (decisions, conventions)
    /// without the user re-stating them. Rendered between harness layers and
    /// the base so the agent reads memory as preamble.
    /// What: Formats the input list as a bulleted Markdown section. An empty
    /// input vec is a no-op (no section is added).
    /// Test: `memory_layer_empty_vec_is_noop`,
    /// `memory_layer_renders_bullet_list`.
    pub fn add_memory_layer(mut self, memories: Vec<String>) -> Self {
        let cleaned: Vec<String> = memories
            .into_iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        if cleaned.is_empty() {
            return self;
        }
        let mut section = String::from("## Project Memory\n\n");
        for m in cleaned {
            // Indent multi-line memories so the bullet hierarchy stays clean.
            let indented = m.replace('\n', "\n  ");
            section.push_str(&format!("- {indented}\n"));
        }
        self.memory_layer = Some(section.trim_end().to_string());
        self
    }

    // INTENT: Append one sub-agent-provided context block as its own layer.
    #[allow(dead_code)]
    pub fn add_subagent_context(mut self, content: impl Into<String>) -> Self {
        self.subagent_layers.push(content.into());
        self
    }

    // INTENT: Count all non-empty layers including base, goal, user memory, project, skill, and subagent.
    #[allow(dead_code)]
    pub fn layer_count(&self) -> usize {
        let mut n = 0;
        if self.goal_block.as_ref().is_some_and(|g| !g.is_empty()) {
            n += 1;
        }
        n += self.harness_layers.len();
        if self.memory_layer.is_some() {
            n += 1;
        }
        if !self.base.trim().is_empty() {
            n += 1;
        }
        n += self.project_layers.len();
        if self.user_memory_layer.is_some() {
            n += 1;
        }
        n += self.skill_layers.len();
        if self.mcp_layer.is_some() {
            n += 1;
        }
        n += self.subagent_layers.len();
        if crate::compress::output_compression_prompt(self.output_style).is_some() {
            n += 1;
        }
        n
    }

    // INTENT: Concatenate all layers in canonical order with labeled/generic separators.
    pub fn build(self) -> String {
        let mut out = String::new();

        // #68: Goal block comes FIRST so the model can't miss it.
        if let Some(goal) = &self.goal_block
            && !goal.is_empty()
        {
            out.push_str(&goal.to_prompt_header());
            out.push_str(LAYER_SEPARATOR);
        }

        // Harness layers: runtime-protocol rules (write_file / finish_task /
        // out_dir / summary) that apply to every agent of the matching
        // runner. Inserted between the goal block and base so protocol
        // expectations are established before the agent's role prompt.
        for layer in &self.harness_layers {
            if !layer.trim().is_empty() {
                out.push_str(layer);
                out.push_str(LAYER_SEPARATOR);
            }
        }

        // Project-memory layer (#241): kuzu-memory recall results sit between
        // harness protocol and base so the agent reads memory before its
        // role-specific prompt.
        if let Some(mem) = &self.memory_layer {
            out.push_str(mem);
            out.push_str(LAYER_SEPARATOR);
        }

        if !self.base.trim().is_empty() {
            out.push_str(&self.base);
        }

        // Project layers: each preceded by a labeled separator carrying the
        // source file path so downstream debugging is trivial.
        for (path, content) in self.project_layers {
            out.push_str(&labeled_separator(&path));
            out.push_str(content.trim_end());
        }

        // User memory: injected after project layers but before skills so
        // project-specific knowledge takes precedence while user preferences
        // still influence skill interpretation.
        if let Some(memory) = self.user_memory_layer {
            if !out.is_empty() {
                out.push_str(LAYER_SEPARATOR);
            }
            out.push_str(&memory);
        }

        // Skill and subagent layers use the generic section separator.
        for content in self.skill_layers {
            if !out.is_empty() {
                out.push_str(LAYER_SEPARATOR);
            }
            out.push_str(&content);
        }
        // MCP layer (#241): rendered after skills so the model first reads
        // its role + skills, then learns about the external MCP services it
        // can call.
        if let Some(mcp) = self.mcp_layer {
            if !out.is_empty() {
                out.push_str(LAYER_SEPARATOR);
            }
            out.push_str(&mcp);
        }
        for content in self.subagent_layers {
            if !out.is_empty() {
                out.push_str(LAYER_SEPARATOR);
            }
            out.push_str(&content);
        }

        // #420: Caveman-style output-compression rules. Appended last so the
        // rules govern every subsequent response.
        if let Some(rules) = crate::compress::output_compression_prompt(self.output_style) {
            if !out.is_empty() {
                out.push_str(LAYER_SEPARATOR);
            }
            out.push_str(rules);
        }

        // #420: Section dedup pass — removes `## Heading` blocks that appear
        // more than once across the layer concatenation (e.g. two skills both
        // bundling the same `## Testing` section). Cheap (single FNV-1a pass)
        // and idempotent.
        crate::compress::dedup_sections(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::HOME_LOCK;
    use std::fs;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("open-mpm-prompt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn builder_empty_has_only_base() {
        let b = SystemPromptBuilder::new("base prompt");
        assert_eq!(b.layer_count(), 1);
        assert_eq!(b.build(), "base prompt");
    }

    #[test]
    fn build_joins_with_separator() {
        let out = SystemPromptBuilder::new("BASE")
            .add_skill("SKILL A")
            .add_skill("SKILL B")
            .add_subagent_context("SUB")
            .build();
        let parts: Vec<&str> = out.split(LAYER_SEPARATOR).collect();
        assert_eq!(parts, vec!["BASE", "SKILL A", "SKILL B", "SUB"]);
    }

    #[test]
    fn user_memory_injected_between_project_and_skills() {
        let out = SystemPromptBuilder::new("BASE")
            .with_user_memory("## User Memory\nprefers snake_case")
            .add_skill("SKILL")
            .build();
        let parts: Vec<&str> = out.split(LAYER_SEPARATOR).collect();
        assert_eq!(
            parts,
            vec!["BASE", "## User Memory\nprefers snake_case", "SKILL"]
        );
    }

    #[test]
    fn user_memory_empty_string_is_ignored() {
        let b = SystemPromptBuilder::new("BASE").with_user_memory("   ");
        assert_eq!(b.layer_count(), 1);
        assert_eq!(b.build(), "BASE");
    }

    #[test]
    fn user_memory_counted_in_layer_count() {
        let b = SystemPromptBuilder::new("BASE")
            .with_user_memory("MEMORY")
            .add_skill("S");
        assert_eq!(b.layer_count(), 3);
    }

    #[test]
    fn user_memory_after_project_layers() {
        // HOME_LOCK serializes with other tests that mutate $HOME process-wide.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempdir();
        fs::write(root.join("CLAUDE.md"), "PROJECT RULES").unwrap();

        unsafe {
            std::env::set_var("HOME", tempdir());
        }
        let out = SystemPromptBuilder::new("BASE")
            .walk_project_instructions(&root)
            .with_user_memory("USER PREFS")
            .add_skill("SKILL")
            .build();

        let project_pos = out.find("PROJECT RULES").unwrap();
        let memory_pos = out.find("USER PREFS").unwrap();
        let skill_pos = out.find("SKILL").unwrap();
        assert!(
            project_pos < memory_pos,
            "project layers must precede user memory"
        );
        assert!(memory_pos < skill_pos, "user memory must precede skills");
    }

    #[test]
    fn root_before_subdir() {
        // HOME_LOCK serializes with other tests that mutate $HOME process-wide.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempdir();
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("CLAUDE.md"), "ROOT RULES").unwrap();
        fs::write(nested.join("CLAUDE.md"), "NESTED RULES").unwrap();

        unsafe {
            std::env::set_var("HOME", tempdir());
        }
        let builder = SystemPromptBuilder::new("BASE").walk_project_instructions(&nested);

        assert_eq!(builder.project_layers.len(), 2);

        let out = builder.build();
        assert!(out.contains("ROOT RULES"));
        assert!(out.contains("NESTED RULES"));
        let root_pos = out.find("ROOT RULES").unwrap();
        let nested_pos = out.find("NESTED RULES").unwrap();
        assert!(root_pos < nested_pos, "root CLAUDE.md must precede nested");
    }

    #[test]
    fn agents_md_recognized_alongside_claude_md() {
        // HOME_LOCK serializes with other tests that mutate $HOME process-wide.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempdir();
        fs::write(root.join("CLAUDE.md"), "CLAUDE MD BODY").unwrap();
        fs::write(root.join("AGENTS.md"), "AGENTS MD BODY").unwrap();

        unsafe {
            std::env::set_var("HOME", tempdir());
        }
        let builder = SystemPromptBuilder::new("BASE").walk_project_instructions(&root);
        assert_eq!(builder.project_layers.len(), 2);

        let out = builder.build();
        assert!(out.contains("CLAUDE MD BODY"));
        assert!(out.contains("AGENTS MD BODY"));
        let c = out.find("CLAUDE MD BODY").unwrap();
        let a = out.find("AGENTS MD BODY").unwrap();
        assert!(c < a, "CLAUDE.md must precede AGENTS.md at the same level");
    }

    #[test]
    fn labeled_separators_contain_path() {
        // HOME_LOCK serializes with other tests that mutate $HOME process-wide.
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempdir();
        let claude = root.join("CLAUDE.md");
        fs::write(&claude, "ROOT RULES").unwrap();

        unsafe {
            std::env::set_var("HOME", tempdir());
        }
        let out = SystemPromptBuilder::new("BASE")
            .walk_project_instructions(&root)
            .build();

        let expected_label = format!("[from: {}]", claude.display());
        assert!(
            out.contains(&expected_label),
            "expected separator with {expected_label:?}; got: {out}"
        );
    }

    #[test]
    fn walk_skips_missing_files() {
        let root = tempdir();
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let builder = SystemPromptBuilder::new("BASE").walk_project_instructions(&nested);
        assert!(builder.project_layers.is_empty());
    }

    #[test]
    fn walk_picks_up_real_project_claude_md() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let claude = manifest_dir.join("CLAUDE.md");
        if !claude.exists() {
            return;
        }
        let out = SystemPromptBuilder::new("BASE")
            .walk_project_instructions(&manifest_dir)
            .build();
        assert!(out.contains("open-mpm"));
    }

    #[test]
    fn build_prepends_goal_block_when_set() {
        let goal = crate::context::GoalBlock {
            primary: "Ship it".into(),
            secondary: vec!["fast".into()],
            task_split_required: false,
        };
        let out = SystemPromptBuilder::new("BASE BODY")
            .with_goal_block(Some(goal))
            .build();
        let goal_pos = out.find("## TASK GOALS").expect("goal header present");
        let base_pos = out.find("BASE BODY").expect("base present");
        assert!(goal_pos < base_pos, "goal header must precede base");
    }

    #[test]
    fn build_without_goal_block_omits_header() {
        let out = SystemPromptBuilder::new("BASE")
            .with_goal_block(None)
            .build();
        assert!(!out.contains("## TASK GOALS"));
    }

    #[test]
    fn layer_count_tracks_additions() {
        let b = SystemPromptBuilder::new("BASE")
            .add_skill("S")
            .add_subagent_context("X");
        assert_eq!(b.layer_count(), 3);
    }

    #[test]
    fn harness_layer_injected_before_base() {
        let out = SystemPromptBuilder::new("BASE BODY")
            .add_harness_layer("HARNESS PROTOCOL")
            .build();
        let h_pos = out.find("HARNESS PROTOCOL").expect("harness present");
        let b_pos = out.find("BASE BODY").expect("base present");
        assert!(h_pos < b_pos, "harness layer must precede base");
    }

    #[test]
    fn harness_layer_absent_when_empty() {
        let out = SystemPromptBuilder::new("BASE")
            .add_harness_layer("   ")
            .add_harness_layer("")
            .build();
        assert_eq!(out, "BASE");
    }

    #[test]
    fn harness_layer_counted_in_layer_count() {
        let b = SystemPromptBuilder::new("BASE")
            .add_harness_layer("H1")
            .add_harness_layer("H2")
            .add_skill("S");
        assert_eq!(b.layer_count(), 4);
    }

    // ── Content tests for compiled-in harness protocol constants ─────────
    //
    // Why: The harness layer content is contractual — agents rely on the
    // `out_dir` + `## Summary` + `write_file` + `finish_task` protocol rules.
    // These tests fail loudly if someone empties or reshapes the constants
    // in `harness_protocol.rs`, catching silent regressions in agent behavior.
    // Test: Run `cargo test --lib harness_`.

    #[test]
    fn harness_base_protocol_contains_output_directory_rule() {
        let p = crate::agents::harness_protocol::BASE_PROTOCOL;
        assert!(!p.trim().is_empty(), "BASE_PROTOCOL must not be empty");
        assert!(
            p.contains("out_dir") || p.contains("Output Directory"),
            "BASE_PROTOCOL must contain output-directory instructions"
        );
        assert!(
            p.contains("Summary"),
            "BASE_PROTOCOL must contain summary requirement"
        );
    }

    #[test]
    fn harness_claude_code_protocol_contains_write_file() {
        assert!(
            crate::agents::harness_protocol::CLAUDE_CODE_PROTOCOL.contains("write_file"),
            "CLAUDE_CODE_PROTOCOL must contain write_file instructions"
        );
    }

    #[test]
    fn harness_finish_task_protocol_contains_finish_task() {
        assert!(
            crate::agents::harness_protocol::FINISH_TASK_PROTOCOL.contains("finish_task"),
            "FINISH_TASK_PROTOCOL must contain finish_task instructions"
        );
    }

    #[test]
    fn prompt_builder_harness_layer_precedes_agent_base() {
        let builder =
            SystemPromptBuilder::new("AGENT_ROLE_CONTENT").add_harness_layer("HARNESS_CONTENT");
        let built = builder.build();
        let harness_pos = built.find("HARNESS_CONTENT").expect("harness missing");
        let agent_pos = built.find("AGENT_ROLE_CONTENT").expect("agent missing");
        assert!(
            harness_pos < agent_pos,
            "Harness layer must appear before agent base content"
        );
    }

    // ── MCP + project memory layers (#241) ────────────────────────────────

    #[test]
    fn mcp_layer_appears_after_skills_before_subagent() {
        let out = SystemPromptBuilder::new("BASE")
            .add_skill("SKILL")
            .add_mcp_layer("## MCP TOOLS")
            .add_subagent_context("SUB")
            .build();
        let s = out.find("SKILL").unwrap();
        let m = out.find("## MCP TOOLS").unwrap();
        let sub = out.find("SUB").unwrap();
        assert!(s < m, "skill must precede MCP layer");
        assert!(m < sub, "MCP layer must precede subagent layer");
    }

    #[test]
    fn mcp_layer_empty_input_is_dropped() {
        let b = SystemPromptBuilder::new("BASE").add_mcp_layer("   ");
        assert_eq!(b.layer_count(), 1);
        assert_eq!(b.build(), "BASE");
    }

    #[test]
    fn mcp_layer_counted_in_layer_count() {
        let b = SystemPromptBuilder::new("BASE")
            .add_skill("S")
            .add_mcp_layer("MCP");
        assert_eq!(b.layer_count(), 3);
    }

    #[test]
    fn memory_layer_renders_bullet_list() {
        let out = SystemPromptBuilder::new("BASE BODY")
            .add_memory_layer(vec!["fact one".into(), "fact two".into()])
            .build();
        assert!(out.contains("## Project Memory"));
        assert!(out.contains("- fact one"));
        assert!(out.contains("- fact two"));
        // Must precede base.
        let mem_pos = out.find("## Project Memory").unwrap();
        let base_pos = out.find("BASE BODY").unwrap();
        assert!(mem_pos < base_pos, "memory layer must precede base");
    }

    #[test]
    fn memory_layer_empty_vec_is_noop() {
        let b = SystemPromptBuilder::new("BASE").add_memory_layer(vec![]);
        assert_eq!(b.layer_count(), 1);
        assert_eq!(b.build(), "BASE");
    }

    #[test]
    fn memory_layer_filters_empty_strings() {
        let b = SystemPromptBuilder::new("BASE").add_memory_layer(vec![
            "".into(),
            "   ".into(),
            "real".into(),
        ]);
        assert_eq!(b.layer_count(), 2);
        let out = b.build();
        assert!(out.contains("- real"));
    }

    #[test]
    fn memory_layer_counted_in_layer_count() {
        let b = SystemPromptBuilder::new("BASE")
            .add_memory_layer(vec!["one".into()])
            .add_skill("S");
        assert_eq!(b.layer_count(), 3);
    }

    #[test]
    fn full_layer_ordering_with_all_slots() {
        let goal = crate::context::GoalBlock {
            primary: "Goal".into(),
            secondary: vec![],
            task_split_required: false,
        };
        let out = SystemPromptBuilder::new("BASE_BODY")
            .with_goal_block(Some(goal))
            .add_harness_layer("HARNESS")
            .add_memory_layer(vec!["MEM_FACT".into()])
            .with_user_memory("USER_MEM")
            .add_skill("SKILL_ONE")
            .add_mcp_layer("MCP_BLOCK")
            .add_subagent_context("SUB_CTX")
            .build();
        // Expected: goal → harness → memory → base → user_memory → skill → mcp → subagent
        let pos = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        assert!(pos("## TASK GOALS") < pos("HARNESS"));
        assert!(pos("HARNESS") < pos("MEM_FACT"));
        assert!(pos("MEM_FACT") < pos("BASE_BODY"));
        assert!(pos("BASE_BODY") < pos("USER_MEM"));
        assert!(pos("USER_MEM") < pos("SKILL_ONE"));
        assert!(pos("SKILL_ONE") < pos("MCP_BLOCK"));
        assert!(pos("MCP_BLOCK") < pos("SUB_CTX"));
    }

    // ── #420: Output-style compression + section dedup ───────────────────

    #[test]
    fn output_style_full_appended_at_end() {
        let out = SystemPromptBuilder::new("BASE BODY")
            .add_skill("SKILL CONTENT")
            .with_output_style(crate::compress::OutputStyle::Full)
            .build();
        let base_pos = out.find("BASE BODY").unwrap();
        let skill_pos = out.find("SKILL CONTENT").unwrap();
        let style_pos = out
            .find("Output Style: Full")
            .expect("style fragment present");
        assert!(base_pos < skill_pos);
        assert!(skill_pos < style_pos, "output style must be last layer");
    }

    #[test]
    fn output_style_none_appends_nothing() {
        let out = SystemPromptBuilder::new("BASE")
            .with_output_style(crate::compress::OutputStyle::None)
            .build();
        assert!(!out.contains("Output Style"));
        assert_eq!(out, "BASE");
    }

    #[test]
    fn output_style_ultra_contains_telegraphic_rule() {
        let out = SystemPromptBuilder::new("BASE")
            .with_output_style(crate::compress::OutputStyle::Ultra)
            .build();
        assert!(out.contains("Output Style: Ultra"));
        assert!(out.contains("telegraphic"));
    }

    #[test]
    fn build_runs_section_dedup() {
        // A single skill that itself contains two identical `## Shared Heading`
        // sections — section dedup should drop the second occurrence. (Two
        // separate skill layers are interleaved with LAYER_SEPARATOR `---`,
        // which breaks exact-match dedup; that's expected behavior.)
        let dup_skill =
            "intro\n## Shared Heading\nbody text here\n## Shared Heading\nbody text here";
        let out = SystemPromptBuilder::new("BASE")
            .add_skill(dup_skill)
            .build();
        let count = out.matches("## Shared Heading").count();
        assert_eq!(count, 1, "section dedup should drop duplicate heading");
    }

    #[test]
    fn output_style_counted_in_layer_count() {
        let b =
            SystemPromptBuilder::new("BASE").with_output_style(crate::compress::OutputStyle::Full);
        assert_eq!(b.layer_count(), 2);
        let b =
            SystemPromptBuilder::new("BASE").with_output_style(crate::compress::OutputStyle::None);
        assert_eq!(b.layer_count(), 1);
    }

    // ── #478: Agent-identity template variables ──────────────────────────

    #[test]
    fn agent_model_substituted_in_prompt() {
        let out = SystemPromptBuilder::new("running on {{AGENT_MODEL}} today")
            .with_agent_context("anthropic/claude-opus-4-7", "claude-code")
            .build();
        assert!(
            out.contains("running on anthropic/claude-opus-4-7 today"),
            "AGENT_MODEL placeholder must be replaced; got: {out}"
        );
        assert!(
            !out.contains("{{AGENT_MODEL}}"),
            "placeholder must not survive substitution"
        );
    }

    #[test]
    fn agent_runner_substituted_in_prompt() {
        let out = SystemPromptBuilder::new("runner is {{AGENT_RUNNER}}")
            .with_agent_context("some/model", "in-process")
            .build();
        assert!(
            out.contains("runner is in-process"),
            "AGENT_RUNNER placeholder must be replaced; got: {out}"
        );
        assert!(!out.contains("{{AGENT_RUNNER}}"));
    }

    #[test]
    fn agent_context_noop_when_no_placeholders() {
        let out = SystemPromptBuilder::new("plain base prompt")
            .with_agent_context("model", "runner")
            .build();
        assert_eq!(out, "plain base prompt");
    }

    #[test]
    fn harness_layer_after_goal_block_before_base() {
        let goal = crate::context::GoalBlock {
            primary: "Ship it".into(),
            secondary: vec![],
            task_split_required: false,
        };
        let out = SystemPromptBuilder::new("BASE BODY")
            .with_goal_block(Some(goal))
            .add_harness_layer("HARNESS PROTOCOL")
            .build();
        let goal_pos = out.find("## TASK GOALS").expect("goal header");
        let harness_pos = out.find("HARNESS PROTOCOL").expect("harness");
        let base_pos = out.find("BASE BODY").expect("base");
        assert!(goal_pos < harness_pos, "goal precedes harness");
        assert!(harness_pos < base_pos, "harness precedes base");
    }
}
