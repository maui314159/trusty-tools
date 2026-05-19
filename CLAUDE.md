# trusty-tools — Claude Code Instructions

Unified Rust workspace consolidating the entire trusty-* AI tooling ecosystem.
22 crates — shared libraries, daemon/MCP servers, the MPM platform, and an
orchestrator — all co-located under one Cargo workspace.

## Project Overview

This is a **Rust workspace** (Cargo workspace, resolver v2, glob members
`crates/*`) under the Elastic License 2.0 (per-crate; a few crates are MIT —
see each `Cargo.toml`). The workspace version field and `rust-version` are
inherited by the `trusty-mpm-*` family; other crates set their own version.

**MSRV**: `1.88` — required for stabilised let-chains used by edition-2024 crates.

## Role & Scope

`trusty-tools` is the **single source of truth** for all trusty-* AI tooling.
It replaces seven formerly separate repos and eliminates the `[patch.crates-io]`
dance for cross-crate development. The authoritative crate list is the
`[workspace.members]` glob in the root `Cargo.toml`; every subdirectory under
`crates/` with a `Cargo.toml` is a member.

Key consumers of the shared libraries:
- **open-mpm** — MPM orchestration platform (lives in `crates/open-mpm`)
- **trusty-search** — hybrid code search daemon + MCP server
- **trusty-memory-core / trusty-memory-mcp** — memory palace storage + MCP frontend
- **trusty-analyze** — code analysis daemon (complexity, smells, quality metrics)

Work touching a shared crate (e.g. `trusty-common`, `trusty-mcp-core`) may
require bumping the dependent crate's version and verifying its tests. Always
run `cargo check` and `cargo test -p <crate>` after modifying a library crate,
then propagate changes to all crates that depend on it before committing.

## Build and Test Commands

🔴 **Single-path workflows — use exactly these commands.**

```bash
# Build all crates (development)
cargo build

# Build all crates (release/optimised)
cargo build --release

# Run all tests
cargo test

# Test a specific crate  (replace <crate-name> with the directory name under crates/)
cargo test -p <crate-name>

# Test with a feature flag
cargo test -p trusty-common --features axum-server

# Check (fast compile check, no codegen)
cargo check

# Lint (workspace-wide, all targets)
cargo clippy --workspace --all-targets -- -D warnings

# Format check
cargo fmt --check

# Format and fix
cargo fmt

# Run ONNX-backed integration tests (slow; skipped in CI)
cargo test -- --include-ignored
```

## Code Structure

```
trusty-tools/               # workspace root
├── Cargo.toml              # workspace manifest — glob members = ["crates/*"]
├── Cargo.lock
├── crates/
│   ├── trusty-common/      # shared utilities, tracing, OpenRouter chat
│   ├── trusty-embedder/    # fastembed wrapper (AllMiniLML6V2Q, 384-dim)
│   ├── trusty-mcp-core/    # MCP primitives and JSON-RPC types
│   ├── trusty-symgraph/    # symbol graph engine (tree-sitter parser)
│   ├── trusty-rpc/         # RPC helpers and service descriptors
│   ├── trusty-tickets/     # GitHub Issues ticketing integration
│   ├── trusty-gworkspace/  # Google Workspace client (Calendar, Tasks, Drive)
│   ├── trusty-cto-db/      # SQLite CTO database (rusqlite-backed)
│   ├── tc-services/        # service-layer adapters: CTO DB, Granola, GWorkspace
│   ├── trusty-search/      # hybrid BM25 + vector + KG search daemon + MCP server
│   ├── trusty-memory-core/ # memory storage engine (usearch + SQLite + fastembed)
│   ├── trusty-memory-mcp/  # MCP server frontend for memory (includes Svelte UI)
│   ├── trusty-analyze/     # code analysis daemon + MCP server
│   ├── trusty-mpm-core/    # MPM core domain types and traits
│   ├── trusty-mpm-mcp/     # MCP server for MPM
│   ├── trusty-mpm-daemon/  # MPM background daemon service
│   ├── trusty-mpm-client/  # MPM API client library
│   ├── trusty-mpm-cli/     # CLI binary (trusty-mpm / tm)
│   ├── trusty-mpm-tui/     # MPM terminal UI
│   ├── trusty-mpm-telegram/ # MPM Telegram bot integration
│   ├── trusty-mpm-gui/     # MPM desktop GUI (Tauri)
│   ├── trusty-git-analytics/ # developer productivity analytics (tga)
│   └── open-mpm/           # MPM orchestration platform
└── .gitignore
```

For the source layout of any crate, read its `README.md` or browse
`crates/<name>/src/`. Each crate owns its own `README.md` covering purpose,
usage, and design notes.

## Key Conventions

🔴 **Why/What/Test doc pattern** — every public item (function, struct, trait,
module) carries three comment sections:

```rust
/// Why: <motivation — the problem this solves, not the mechanics>
/// What: <mechanical description of what the item does>
/// Test: <where coverage lives, or why it is side-effect-only / untestable>
pub fn my_function() { … }
```

Never omit this pattern on public items. It is the primary way future readers
understand design intent without reading git history.

🔴 **No `unwrap()` in library code** — use `?` with `anyhow::Result` for
application/binary code and `thiserror` for library error types. Reserve
`expect()` only for cases that are genuinely programmer errors (invariants that
can never occur at runtime).

🔴 **`thiserror` for libraries, `anyhow` for binaries** — library crates
(`trusty-common`, `trusty-mcp-core`, etc.) define structured error enums with
`#[derive(thiserror::Error)]`. Binary and daemon crates use `anyhow::Result`
throughout.

🔴 **Feature flags** — `trusty-common` gates `axum` and `tower-http` behind the
`axum-server` feature flag. Do not add axum as an unconditional dependency in
any library crate. Enable it explicitly in crates that serve HTTP.

🟡 **Rust editions** — `edition = "2024"` for `trusty-mpm-*` and `open-mpm`
(they use let-chains); `edition = "2021"` for all other crates. Check the crate
`Cargo.toml` before assuming an edition.

🟡 **No global state** — all helpers are free functions or small structs. No
`lazy_static!` or `once_cell::sync::Lazy` except the tracing subscriber (which
uses `try_init` to be idempotent across test binaries).

🟡 **Logs to stderr** — `init_tracing` always writes to stderr so stdout stays
clean for MCP JSON-RPC framing. Never log to stdout in a daemon or MCP server.

🟡 **Ignore-tagged integration tests** — ONNX-backed embedder tests are marked
`#[ignore]` to keep CI fast. Run with `cargo test -- --include-ignored` when
you need local validation against the model.

🟢 **Workspace dependency sharing** — all shared external crates (`anyhow`,
`serde`, `tokio`, `axum`, etc.) are declared once in `[workspace.dependencies]`
in the root `Cargo.toml` and referenced as `dep = { workspace = true }` in
crate manifests. Never pin a dependency locally if it is already in the
workspace table.

🟢 **Internal path deps** — reference sibling crates as:
```toml
trusty-common = { workspace = true }
```
The workspace manifest declares the path so every member resolves from the
in-tree source automatically. The `[patch.crates-io]` block in the root
`Cargo.toml` also redirects any crates that still reference the old published
versions by version number.

## Git Tag / Release Convention

Each crate is tagged independently using the pattern `<crate-name>-v<version>`,
e.g. `trusty-mcp-core-v0.2.0`. The version comes from the crate's `Cargo.toml`.

Release workflow:
1. Bump the crate version in `crates/<name>/Cargo.toml`.
2. Update any dependent crates that pin that version.
3. Run `cargo test -p <name>` and `cargo clippy --workspace -- -D warnings`.
4. Commit the version bump.
5. Create the tag: `git tag <crate-name>-v<version>`.
6. Push the tag: `git push origin <crate-name>-v<version>`.
7. Publish: `cargo publish -p <crate-name>`.

The `trusty-mpm-*` family shares a single workspace version (declared under
`[workspace.package]`) and is bumped together.

## Cross-Crate Development Workflow

Because all crates are in the same workspace, the `[patch.crates-io]` dance
that was required with separate repos is no longer necessary for active
development. Cargo resolves internal crates via path automatically.

When you modify a library crate:
1. Edit the crate under `crates/<lib>/`.
2. Run `cargo check` to catch compilation errors across the entire workspace.
3. Run `cargo test -p <lib>` for the modified library.
4. Run `cargo test -p <consumer>` for each crate that depends on the library.
5. Commit all changes together — workspace builds are atomic.

When publishing a crate to crates.io:
- The path dep in `[workspace.dependencies]` coexists with the version field,
  so `cargo publish` sees the version and uploads correctly.
- The `[patch.crates-io]` block in the root `Cargo.toml` ensures the in-tree
  crates are preferred during local builds even if a published version exists.

## Crate-Specific Notes

### trusty-common
- Provides `openrouter_chat` (one-shot) and `openrouter_chat_stream` (SSE /
  tokio mpsc). Both require `OPENROUTER_API_KEY` to be passed by the caller —
  the library never reads environment variables directly.
- Provides `init_tracing`, port-walking helpers, and daemon address utilities.
- `axum-server` feature gates axum, tower, and tower-http. Do not enable unless
  the crate serves HTTP.

### trusty-embedder
- `FastEmbedder` defaults to `AllMiniLML6V2Q` (INT8 quantised, ~22 MB, 384-dim).
- Falls back to full-precision `AllMiniLML6V2` (~86 MB) when the quantised
  model is unavailable.
- Output dimension: **384**.

### trusty-common axum Middleware Stack
`with_standard_middleware` applies in this order:
1. `CorsLayer` — any origin/methods/headers (for local browser UIs)
2. `TraceLayer` — HTTP request spans
3. `CompressionLayer` — gzip, with `text/event-stream` excluded (SSE compat)

### trusty-memory-core / trusty-memory-mcp
- Licensed **MIT** (not Elastic-2.0). Check before assuming the workspace
  default license applies.
- `trusty-memory-mcp` embeds a compiled Svelte UI via `rust-embed`.

### trusty-mpm-gui
- Tauri-based desktop GUI. Its `src-tauri/` subdirectory contains a nested
  Cargo manifest that is excluded from the workspace glob to prevent Cargo from
  treating it as a second member.

### open-mpm
- The top-level MPM orchestration platform. Consumes `trusty-search`,
  `trusty-memory-core`, and `trusty-symgraph`.
- Uses edition 2024 and let-chains extensively.

## Former Repos Reference

These repos were merged into this monorepo. Use this table when reading old
PRs, issues, or commit messages that reference the former repo names.

| Former repo | Now lives in |
|---|---|
| `bobmatnyc/trusty-common` | `crates/trusty-common` + 8 library crates |
| `bobmatnyc/trusty-search` | `crates/trusty-search` |
| `bobmatnyc/trusty-memory` | `crates/trusty-memory-core`, `crates/trusty-memory-mcp` |
| `bobmatnyc/trusty-analyze` | `crates/trusty-analyze` |
| `bobmatnyc/trusty-git-analytics` | `crates/trusty-git-analytics` |
| `bobmatnyc/trusty-mpm` | `crates/trusty-mpm-{core,mcp,daemon,client,cli,tui,telegram,gui}` |
| `bobmatnyc/open-mpm` | `crates/open-mpm` |
