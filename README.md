# trusty-tools

Unified Rust workspace for the trusty-* AI tooling suite.

## Crates

Top-level applications:
- **trusty-search** — machine-wide hybrid code search service (BM25 + vector + KG) with MCP server
- **trusty-memory** — machine-wide AI memory service
- **trusty-analyze** — sidecar code-analysis daemon (complexity, smells, quality, facts)

Supporting libraries:
- **trusty-common** — shared utilities, provider-agnostic streaming chat (OpenRouter, Ollama)
- **trusty-mcp-core** — JSON-RPC 2.0 / MCP types and stdio runner
- **trusty-embedder** — fastembed-backed embedding abstraction
- **trusty-symgraph** — standalone symbol-graph engine (AST → SymbolRegistry → emit)
- **trusty-rpc** — general-purpose CLI for JSON-RPC services (`trpc`)
- **trusty-tickets** — unified ticketing MCP server (GitHub/JIRA/Linear)
- **trusty-gworkspace** — Google Workspace MCP server
- **trusty-cto-db** — read-only SQLite tools over the CTO ops database
- **tc-services** — shared service-layer implementations
- **trusty-memory-core** — core types/storage/retrieval for trusty-memory
- **trusty-memory-mcp** — MCP server (stdio + HTTP/SSE) for trusty-memory

## Build

```bash
cargo build              # debug
cargo build --release    # release
cargo test --workspace   # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

## License

Elastic License 2.0 (per-crate; see individual `Cargo.toml` for crate-specific licenses).
