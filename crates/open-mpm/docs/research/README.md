# open-mpm Research: Findings & Feasibility Assessment

**Date:** 2026-04-22  
**Researcher:** Research Agent (claude-sonnet-4.6)  
**Purpose:** Inform architecture of open-mpm, a Rust-based AI agent harness comparable to claude-mpm

---

## What We're Building

**open-mpm** is a Rust harness for AI agent orchestration, targeting feature parity with claude-mpm. Core capabilities:

- Agent delegation (PM orchestrator spawning and managing sub-agents)
- Tool calling (MCP-style, via OpenRouter/Claude API)
- Agent definitions (file-based, skills system)
- Subprocess message passing (PM ↔ sub-agent IPC)
- OpenRouter as LLM backend, Claude Sonnet 4.6 as default model

---

## Research Files

| File | Topic |
|------|-------|
| [rust-ai-frameworks.md](./rust-ai-frameworks.md) | Existing Rust AI frameworks: Rig, Swiftide, llm-chain, langchain-rust, AutoAgents, rmcp |
| [openrouter-api.md](./openrouter-api.md) | OpenRouter API: completions, streaming, tool calling, Claude Sonnet 4.6 specifics |
| [subprocess-ipc-patterns.md](./subprocess-ipc-patterns.md) | tokio::process, NDJSON framing, bidirectional IPC, MCP stdio transport |
| [agent-delegation-patterns.md](./agent-delegation-patterns.md) | Orchestrator-worker, agents-as-tools, handoffs, parallel execution, protocol design |
| [openai-plans.md](./openai-plans.md) | OpenAI reasoning models, preambles, structured outputs, plan mode patterns |
| [claude-code-techniques.md](./claude-code-techniques.md) | Claude Code internal architecture (April 2026 leak): agentic loop, tool schemas, multi-agent spawning, CLAUDE.md spec, prompt caching |
| [other-harnesses-lessons.md](./other-harnesses-lessons.md) | Roo Code, Cline, Codex CLI: context condensation, shadow Git, Orchestrator context poisoning, per-mode models |
| [workflow-engine-design.md](./workflow-engine-design.md) | Workflow engine design: `--workflow` flag, WorkflowDef/PhaseDef structs, web_search + skill_loader tools |
| [agent-decomposition-patterns.md](./agent-decomposition-patterns.md) | One-agent-per-file decomposition: stub files, interface-first design, TDD order, file size norms, inter-agent consistency risks |
| [restate-evaluation.md](./restate-evaluation.md) | Restate durable execution engine: technical fit, licensing (BUSL), architecture constraints, alternatives — No-Go verdict for current single-binary phase |
| [output-token-optimization.md](./output-token-optimization.md) | Output token optimization: structured formats, verbosity-reduction prompts, max_tokens by role, prompt caching, CoT vs direct, model selection for token efficiency |

---

## Key Findings Summary

### 1. Rust AI Frameworks

The Rust AI framework ecosystem in 2026 is active but fragmented:

| Framework | What It Does Well | What's Missing |
|-----------|------------------|----------------|
| **Rig** | Unified LLM interface, tool calling, agent-as-tool | Process orchestration, skill defs |
| **Swiftide** | RAG pipelines, agent building blocks | Experimental agent layer, no subprocess |
| **AutoAgents** | Actor-model multi-agent, OpenRouter native | Very new (Dec 2025), no skill files |
| **rmcp** | Official MCP stdio transport, subprocess spawning | Not a harness, protocol only |
| **ai-agents.rs** | YAML agent definitions | Limited delegation, unknown maturity |

**Critical gap:** No existing Rust framework provides a PM-orchestrator pattern with subprocess delegation, file-based skill/agent definitions, and NDJSON message-passing IPC in the claude-mpm style. **open-mpm would be novel.**

### 2. OpenRouter API

OpenRouter is an excellent backend for open-mpm:
- Full OpenAI-compatible API, single endpoint for all models
- Streaming, tool calling, structured outputs all supported
- Claude Sonnet 4.6: $3/$15 per M tokens, 1M context, 128k output, vision + reasoning
- Model routing and fallback built in
- `async-openai` crate works with OpenRouter via base URL override

### 3. Subprocess IPC

The technical foundation for PM ↔ sub-agent communication is solid:
- `tokio::process::Command` handles subprocess spawning
- NDJSON (newline-delimited JSON) over stdin/stdout is the right protocol
- `tokio::io::AsyncBufReadExt::lines()` for reading; `write_all` + `\n` for writing
- `kill_on_drop(true)` prevents zombie processes
- Separate tokio tasks for reading/writing prevent deadlocks
- The official MCP Rust SDK (`rmcp` v1.5.0) provides a proven reference implementation
- 60-second tool execution timeout, 10-second init timeout are good defaults

### 4. Agent Delegation

Multi-agent delegation is well-understood:
- **Orchestrator-Worker** is the dominant production pattern (~70% of deployments)
- **Agents-as-tools** works with standard tool-calling APIs (OpenAI-compatible)
- **Structured outputs** for result passing ensure reliable PM parsing
- JSON-RPC 2.0 over NDJSON is the natural protocol choice (MCP already uses it)
- Task descriptions must include: objective, output format, tool guidance, boundaries

### 5. OpenAI "Plans" Feature

No dedicated "plans" API exists. However:
- GPT-5.x preambles give visible pre-action plans (prompt-driven)
- o-series reasoning models are purpose-built planners (available via OpenRouter)
- Claude Sonnet 4.6 supports extended thinking (available via OpenRouter)
- A two-phase "plan then execute" PM pattern is straightforward to implement
- Structured outputs can capture plans as typed JSON

---

## Feasibility Assessment

### Can we build a competitive Rust harness matching claude-mpm capabilities?

**Assessment: Yes, with high confidence. Rust provides advantages in several dimensions.**

#### Technical Feasibility: High

| Component | Feasibility | Approach |
|-----------|------------|----------|
| LLM API client | Trivial | `async-openai` + OpenRouter base URL |
| Tool calling loop | Straightforward | Implement agent loop with `reqwest`/`async-openai` |
| Subprocess spawning | Straightforward | `tokio::process::Command` |
| Bidirectional IPC | Proven | NDJSON over stdin/stdout, separate read/write tasks |
| MCP tool server/client | Ready | `rmcp` v1.5.0 (official, mature) |
| Agent definitions | Straightforward | TOML/YAML files, load at runtime |
| Skills system | Straightforward | Markdown files injected into system prompt |
| PM orchestrator | Medium complexity | Implement orchestrator-worker with tool-calling |
| Parallel sub-agents | Straightforward | `tokio::spawn` + `futures::future::join_all` |
| Streaming output | Supported | SSE from OpenRouter, propagate to terminal |

#### Competitive Advantages Over Python claude-mpm

1. **Memory efficiency:** 5x less RAM in benchmarks (1GB vs 5GB typical)
2. **Cold start:** ~4ms vs ~60ms for Python — critical for CI/CD and serverless
3. **CPU efficiency:** ~2x less CPU under load
4. **No GIL:** True parallelism for concurrent sub-agents
5. **Single binary:** No Python environment, no pip, no venv
6. **Memory safety:** Rust ownership prevents the class of bugs that plague subprocess orchestration
7. **Latency:** P95 latency ~1.75x better than LangGraph-style frameworks

#### Gaps to Address

1. **Python ecosystem depth:** claude-mpm has 56+ bundled skills, 47+ agents. This is content work, not engineering work — skills are markdown files.
2. **MCP client/tool ecosystem:** Most MCP tools are Node.js or Python servers. open-mpm needs to spawn them as subprocesses (already supported by `rmcp`).
3. **Community/integrations:** Python frameworks have larger communities. Rust is catching up fast (AutoAgents hit 404 stars/day in 2026 vs 25/day in 2023).

#### Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| API stability of Rig/AutoAgents | Medium | Pin versions; consider building own minimal LLM client |
| Claude-specific features (extended thinking) not in OpenRouter | Low | OpenRouter supports it as of research date |
| NDJSON protocol debugging complexity | Low | Protocol is simple and human-readable |
| Sub-agent context management | Medium | Implement auto-summarization at 70%/85%/95% thresholds (per claude-mpm pattern) |

---

## Recommended Architecture for open-mpm

```
open-mpm binary (single Rust binary)
├── Modes:
│   ├── pm        — PM orchestrator mode (interactive or piped)
│   ├── agent     — Sub-agent mode (subprocess, reads from stdin)
│   └── mcp       — MCP server mode (for tool hosting)
│
├── Core crates:
│   ├── open-mpm-core
│   │   ├── agent_loop.rs     — LLM call loop with tool execution
│   │   ├── tool_registry.rs  — Register + execute tools
│   │   ├── message.rs        — NDJSON message types (JSON-RPC 2.0)
│   │   └── streaming.rs      — SSE streaming handler
│   │
│   ├── open-mpm-pm
│   │   ├── orchestrator.rs   — PM orchestration loop
│   │   ├── delegation.rs     — Sub-agent spawning + management
│   │   ├── skills.rs         — Three-tier skill loading
│   │   └── agent_defs.rs     — Agent definition file loading
│   │
│   └── open-mpm-tools
│       ├── bash.rs           — Shell execution tool
│       ├── file_ops.rs       — Read/write/edit/glob/grep tools
│       └── mcp_client.rs     — MCP server subprocess client
│
├── Dependencies (recommended):
│   ├── tokio                 — Async runtime
│   ├── async-openai          — OpenRouter-compatible LLM client
│   ├── serde + serde_json    — Serialization
│   ├── rmcp                  — MCP protocol (optional, for MCP tools)
│   ├── tracing               — Structured logging
│   ├── clap                  — CLI argument parsing
│   └── anyhow / thiserror    — Error handling
│
└── Config files:
    ├── agents/*.toml         — Agent definitions
    ├── skills/*.md           — Skill markdown files
    └── open-mpm.toml         — Project config
```

### IPC Protocol (PM ↔ Sub-Agent)

```
PM process
  │
  ├─[tokio::process]─► Sub-Agent process
  │   stdin  ← NDJSON JSON-RPC requests (tasks, tool results)
  │   stdout → NDJSON JSON-RPC responses + progress events
  │   stderr → PM logger (separate reader task)
  │
  └─[tokio::process]─► Sub-Agent process B (parallel)
```

---

## Implementation Roadmap Suggestion

### Phase 1: Core Infrastructure (MVP)

1. OpenRouter client with streaming + tool calling (`async-openai` + reqwest)
2. Basic agent loop: system prompt → user message → tool call loop → response
3. Built-in tools: bash, read_file, write_file, edit_file
4. Single-agent mode working (no orchestration yet)

### Phase 2: Subprocess IPC

1. NDJSON message framing (`tokio::io::BufReader::lines()` + JSON-RPC 2.0)
2. Sub-agent spawning (`tokio::process::Command` + `kill_on_drop`)
3. Bidirectional IPC (separate read/write tasks, deadlock prevention)
4. Process lifecycle management (timeouts, graceful shutdown, error detection)

### Phase 3: PM Orchestrator

1. PM agent with planning prompt
2. Agent-as-tool delegation pattern
3. Agent definition files (TOML)
4. Parallel sub-agent execution (`tokio::spawn` + `join_all`)
5. Result aggregation

### Phase 4: Skills & Agent Definitions

1. Three-tier skill loading (bundled / user / project)
2. Skills as markdown context injection
3. Agent type library (engineer, QA, security, researcher, etc.)
4. PM domain routing logic

### Phase 5: MCP Integration

1. MCP client via `rmcp` (spawn external MCP servers as subprocesses)
2. MCP tool execution proxy
3. Optional: expose open-mpm tools as an MCP server

---

## Bottom Line

Building open-mpm is technically straightforward with Rust's mature async ecosystem. The engineering work is real but well-scoped. The Rust performance advantages are genuine and measurable. The biggest investment is content (agent definitions, skills), not infrastructure.

The absence of a competing Rust harness with claude-mpm's feature set represents a real opportunity. The Rust AI agent ecosystem is growing at 16x the rate of 2023, and open-mpm would enter at a strong moment.

**Recommendation: Proceed with implementation.**

---

## Sources Across All Research Files

- [Rig GitHub](https://github.com/0xPlaygrounds/rig)
- [Swiftide](https://swiftide.rs/)
- [AutoAgents](https://github.com/liquidos-ai/AutoAgents)
- [rmcp / Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
- [OpenRouter API](https://openrouter.ai/docs/api/reference/overview)
- [OpenRouter Tool Calling](https://openrouter.ai/docs/guides/features/tool-calling)
- [Claude Sonnet 4.6](https://openrouter.ai/anthropic/claude-sonnet-4.6)
- [tokio::process](https://docs.rs/tokio/latest/tokio/process/)
- [tokio_process_tools](https://docs.rs/tokio-process-tools/latest/tokio_process_tools/)
- [MCP Transports Spec](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [OpenAI Agents SDK Multi-agent](https://openai.github.io/openai-agents-python/multi_agent/)
- [Anthropic Multi-agent Research System](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Benchmarking AI Agent Frameworks 2026](https://dev.to/saivishwak/benchmarking-ai-agent-frameworks-in-2026-autoagents-rust-vs-langchain-langgraph-llamaindex-338h)
- [Rust AI Ecosystem Overview](https://hackmd.io/@Hamze/Hy5LiRV1gg)
- [claude-mpm GitHub](https://github.com/bobmatnyc/claude-mpm)
- [OpenAI Reasoning Best Practices](https://developers.openai.com/api/docs/guides/reasoning-best-practices)
- [NDJSON Specification](https://github.com/ndjson/ndjson-spec)
