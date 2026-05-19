# trusty-tools

Unified Rust workspace consolidating the entire trusty-* AI tooling ecosystem.
Previously spread across 7 separate repos, all 22 crates are now co-located for
atomic cross-crate changes, a single lockfile, and no `[patch.crates-io]` dance
during development.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Orchestrator                             │
│   open-mpm — MPM orchestration platform                         │
└───────────────┬─────────────────┬───────────────────────────────┘
                │                 │
┌───────────────▼─────────────────▼───────────────────────────────┐
│                   Daemons / MCP servers                         │
│  trusty-search     trusty-memory-mcp    trusty-analyze          │
│  (hybrid search)   (memory palace UI)   (code quality)          │
└───────────────┬────────────┬────────────────────────────────────┘
                │            │
┌───────────────▼────────────▼───────────────────────────────────┐
│                   Storage / Engine layer                        │
│  trusty-memory-core   trusty-symgraph   trusty-cto-db          │
│  (usearch + SQLite)   (tree-sitter KG)  (ops SQLite)           │
└───────────────┬────────────────────────────────────────────────┘
                │
┌───────────────▼────────────────────────────────────────────────┐
│                   MPM Platform (trusty-mpm-*)                  │
│  core · mcp · daemon · client · cli · tui · telegram · gui     │
└───────────────┬────────────────────────────────────────────────┘
                │
┌───────────────▼────────────────────────────────────────────────┐
│                   Shared libraries                              │
│  trusty-common   trusty-mcp-core   trusty-embedder             │
│  trusty-rpc      trusty-tickets    trusty-gworkspace            │
│  tc-services     trusty-git-analytics                          │
└────────────────────────────────────────────────────────────────┘
```

## Quick Start

```bash
git clone https://github.com/bobmatnyc/trusty-tools
cd trusty-tools
cargo build          # compile all 22 crates
cargo test           # run all tests
```

## Crate Index

### Shared Libraries

| Crate | Description |
|---|---|
| `trusty-common` | Port walking, daemon addr, tracing init, OpenRouter chat, shared utilities |
| `trusty-embedder` | fastembed wrapper — `AllMiniLML6V2Q` INT8 quantised, 384-dim output |
| `trusty-mcp-core` | MCP (Model Context Protocol) primitives, JSON-RPC 2.0 types, stdio runner |
| `trusty-symgraph` | Symbol graph engine — tree-sitter AST to `SymbolRegistry` to emit |
| `trusty-rpc` | RPC helpers, service descriptors, and `trpc` CLI |
| `trusty-tickets` | GitHub Issues ticketing integration |
| `trusty-gworkspace` | Google Workspace client — Calendar, Tasks, Drive |
| `trusty-cto-db` | SQLite CTO database (rusqlite 0.39, bundled) |
| `tc-services` | Service-layer adapters: CTO DB, Granola native client, GWorkspace bridge |

### Daemons / MCP Servers

| Crate | Description |
|---|---|
| `trusty-search` | Machine-wide hybrid BM25 + vector + knowledge-graph code search + MCP server |
| `trusty-memory-core` | Memory storage engine — usearch ANN index + SQLite + fastembed (MIT) |
| `trusty-memory-mcp` | MCP server + HTTP/SSE frontend for trusty-memory; embeds compiled Svelte UI (MIT) |
| `trusty-analyze` | Code analysis daemon — complexity, smells, quality metrics + MCP server |

### MPM (Multi-agent Platform Manager)

| Crate | Description |
|---|---|
| `trusty-mpm-core` | Core domain types and traits |
| `trusty-mpm-mcp` | MCP server for MPM (OpenAPI / Swagger via utoipa) |
| `trusty-mpm-daemon` | Background daemon service |
| `trusty-mpm-client` | API client library |
| `trusty-mpm-cli` | CLI binary — `trusty-mpm` / `tm` |
| `trusty-mpm-tui` | Terminal UI (ratatui + crossterm) |
| `trusty-mpm-telegram` | Telegram bot integration (teloxide) |
| `trusty-mpm-gui` | Desktop GUI (Tauri) |

### Analytics

| Crate | Description |
|---|---|
| `trusty-git-analytics` | Developer productivity analytics from git history (`tga` binary) |

### Orchestrator

| Crate | Description |
|---|---|
| `open-mpm` | MPM orchestration platform; consumes trusty-search, trusty-memory, trusty-symgraph |

## Build Commands

```bash
cargo build                                          # all crates, debug
cargo build --release                               # all crates, release
cargo test                                          # all tests
cargo test -p <crate-name>                          # single crate
cargo check                                         # fast compile check
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo fmt
```

## License

Elastic License 2.0 throughout, except `trusty-memory-core` and
`trusty-memory-mcp` which are MIT. See each crate's `Cargo.toml` for the
authoritative license field.
