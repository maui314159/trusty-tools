# trusty-tools

Unified Rust workspace consolidating the entire trusty-* AI tooling ecosystem.
Unified Rust workspace consolidating 24 crates of AI development tooling,
with three flagship MCP servers for code search, memory management, and analysis.

## Three Flagship MCP Servers

### trusty-search — Hybrid Code Search

Machine-wide code search daemon with hybrid BM25 + vector + knowledge-graph fusion, fused via Reciprocal Rank Fusion. One install per machine, unlimited named project indexes.

**What you get:**
- Sub-10ms p50 warm query latency on 100k-chunk indexes
- Intelligent query routing (Definition / Usage / Conceptual / BugDebt intent detection)
- Knowledge graph expansion with caller/callee chains
- Branch-aware search (boost results from your current feature branch)
- Auto-tuned memory tiers (5 tiers from 8 GB to 64+ GB RAM)
- Embedded Svelte 5 admin UI
- OpenRouter-backed chat with auto-injected search context

**Quick start:**
```bash
cargo install trusty-search
trusty-search start
trusty-search index ~/Projects/myproj --name myproj
trusty-search query "fn authenticate" --index myproj
```

**MCP tools:** `search_code`, `search_similar`, `index_file`, `remove_file`, `list_indexes`, `create_index`, `delete_index`, `reindex`, `index_status`, `list_chunks`, `search_health`, `chat`

See [crates/trusty-search/README.md](crates/trusty-search/README.md) for full documentation.

---

### trusty-memory — Memory Palace Storage Engine

Long-term memory storage with semantic search, persistent embedding index, and embedded Svelte UI. Store development context, notes, snippets, and retrieve them via natural language.

**What you get:**
- HNSW vector index (usearch) + SQLite persistent storage + fastembed embeddings
- Semantic search over all stored memories
- Collection organization (notes, snippets, code patterns, decisions)
- Svelte UI for browsing and editing
- MCP server for Claude Code integration
- MIT license (memory preservation is for everyone)

**Quick start:**
```bash
cargo run -p trusty-memory-mcp -- serve
# Or via MCP stdio:
# Add to ~/.claude/claude_desktop_config.json:
# "trusty-memory": {
#   "command": "cargo",
#   "args": ["run", "-p", "trusty-memory-mcp", "--", "serve"]
# }
```

**MCP tools:** `store_memory`, `search_memories`, `update_memory`, `list_collections`, `create_collection`, `get_memory`, `delete_memory`

See [crates/trusty-memory-mcp/README.md](crates/trusty-memory-mcp/README.md) for full documentation.

---

## Full Crate Index (All 24 Crates)

### Core Daemons / MCP Servers

| Crate | Description | License |
|---|---|---|
| `trusty-search` | Hybrid code search (BM25 + vector + KG) + MCP server | Elastic-2.0 |
| `trusty-memory-core` | Memory storage engine (usearch + SQLite + embeddings) | MIT |
| `trusty-memory-mcp` | Memory palace UI + MCP frontend | MIT |

### Shared Libraries

| Crate | Description |
|---|---|
| `trusty-common` | Tracing, daemon helpers, OpenRouter chat client, shared utilities |
| `trusty-embedder` | fastembed wrapper — all-MiniLM-L6-v2 INT8 quantised, 384-dim output |
| `trusty-mcp-core` | MCP primitives, JSON-RPC 2.0 types, stdio/HTTP transport |
| `trusty-symgraph` | Symbol graph engine — tree-sitter AST parser + knowledge graph |
| `trusty-rpc` | RPC helpers and service descriptors |
| `trusty-tickets` | GitHub Issues integration |
| `trusty-gworkspace` | Google Workspace client (Calendar, Tasks, Drive) |
| `trusty-cto-db` | SQLite schema + rusqlite bindings for ops data |
| `tc-services` | Service adapters (CTO DB, Granola, GWorkspace) |

### MPM Platform (Multi-agent Platform Manager)

| Crate | Description |
|---|---|
| `trusty-mpm-core` | Core domain types and traits |
| `trusty-mpm-mcp` | MCP server for MPM (OpenAPI / Swagger) |
| `trusty-mpm-daemon` | Background daemon service |
| `trusty-mpm-client` | API client library |
| `trusty-mpm-cli` | CLI binary — `trusty-mpm` / `tm` |
| `trusty-mpm-tui` | Terminal UI (ratatui + crossterm) |
| `trusty-mpm-telegram` | Telegram bot integration |
| `trusty-mpm-gui` | Desktop GUI (Tauri) |

### Analytics & Orchestration

| Crate | Description |
|---|---|
| `trusty-git-analytics` | Developer productivity analytics from git history |
| `open-mpm` | MPM orchestration platform |
| `cto-assistant` | CTO domain assistant |

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  Flagship Services (User-Facing)                             │
│  trusty-search  |  trusty-memory                             │
│  (search)       |  (storage)                                 │
└──────────────────────────────────────────────────────────────┘
         │                  │
┌────────▼──────────────────▼──────────────────────────────────┐
│  Storage / Engine Libraries                                 │
│  trusty-symgraph  ·  trusty-embedder  ·  trusty-cto-db      │
└────────┬──────────────────────────────────────────────────────┘
         │
┌────────▼──────────────────────────────────────────────────────┐
│  Shared Infrastructure                                       │
│  trusty-common  ·  trusty-mcp-core  ·  trusty-rpc           │
│  trusty-gworkspace  ·  trusty-tickets  ·  tc-services       │
└────────┬──────────────────────────────────────────────────────┘
         │
┌────────▼──────────────────────────────────────────────────────┐
│  Orchestrator / Platform                                    │
│  open-mpm  ·  trusty-mpm-* family  ·  trusty-git-analytics  │
└───────────────────────────────────────────────────────────────┘
```

## Quick Start — All Crates

```bash
git clone https://github.com/bobmatnyc/trusty-tools
cd trusty-tools

# Build all crates
cargo build --release

# Run all tests
cargo test

# Install the CLI tools
cargo install --path crates/trusty-search --locked
# More tools available via: cargo install --path crates/<crate>
```

## Build & Test Commands

```bash
cargo build                                          # all crates, debug
cargo build --release                               # all crates, release/optimized
cargo test                                          # all tests
cargo test -p <crate-name>                          # single crate tests
cargo check                                         # fast compile check
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo fmt
```

## Workspace Info

**Single source of truth:** This monorepo consolidates seven formerly separate repos. All 24 crates are co-located under `crates/` with one workspace root and one `Cargo.lock` — no more `[patch.crates-io]` dances during active development.

**MSRV:** Rust 1.88+ (required for `let-chains` used by `trusty-mpm-*` and `open-mpm` in edition 2024)

**License:** Elastic License 2.0 (most crates), MIT (trusty-memory-core, trusty-memory-mcp, trusty-gworkspace). See each crate's `Cargo.toml` for the authoritative license field.

**Where to start:**
- **I want to search code:** Read [crates/trusty-search/README.md](crates/trusty-search/README.md)
- **I want persistent memory:** Read [crates/trusty-memory-mcp/README.md](crates/trusty-memory-mcp/README.md)
- **I want the full platform:** Read [crates/open-mpm/README.md](crates/open-mpm/README.md)

---

## Planned Components

### trusty-analyze (In Development)

Code analysis daemon providing:
- Cyclomatic complexity calculation
- Code smell detection
- Quality metrics and anti-pattern identification
- MCP server for integration with Claude Code and other tools

Coming soon to `crates/trusty-analyze/`.
