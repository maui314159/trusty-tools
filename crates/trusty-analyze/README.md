# trusty-analyzer

[![CI](https://github.com/bobmatnyc/trusty-analyze/actions/workflows/ci.yml/badge.svg)](https://github.com/bobmatnyc/trusty-analyze/actions/workflows/ci.yml)
[![Publish](https://github.com/bobmatnyc/trusty-analyze/actions/workflows/publish.yml/badge.svg)](https://github.com/bobmatnyc/trusty-analyze/actions/workflows/publish.yml)
[![crates.io: trusty-analyzer-types](https://img.shields.io/crates/v/trusty-analyzer-types.svg?label=trusty-analyzer-types)](https://crates.io/crates/trusty-analyzer-types)
[![crates.io: trusty-analyzer-core](https://img.shields.io/crates/v/trusty-analyzer-core.svg?label=trusty-analyzer-core)](https://crates.io/crates/trusty-analyzer-core)
[![crates.io: trusty-analyzer-lang](https://img.shields.io/crates/v/trusty-analyzer-lang.svg?label=trusty-analyzer-lang)](https://crates.io/crates/trusty-analyzer-lang)
[![crates.io: trusty-analyzer-mcp](https://img.shields.io/crates/v/trusty-analyzer-mcp.svg?label=trusty-analyzer-mcp)](https://crates.io/crates/trusty-analyzer-mcp)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Sidecar code-analysis daemon for [trusty-search](../trusty-search). Fetches chunk
corpora from the trusty-search daemon, runs static analysis, and serves results via
HTTP (port 7879) and MCP stdio.

## Installation

The `trusty-analyze` binary is distributed via GitHub Releases and `cargo install`:

```bash
cargo install --git https://github.com/bobmatnyc/trusty-analyze trusty-analyzer
```

> The crate name on crates.io is `trusty-analyzer`, but the installed binary is
> named `trusty-analyze`.

The library crates (`trusty-analyzer-types`, `trusty-analyzer-core`,
`trusty-analyzer-lang`, `trusty-analyzer-mcp`) are published to crates.io and
can be added directly:

```toml
[dependencies]
trusty-analyzer-core = "0.1"
```

> **Note:** the workspace also contains `trusty-embedder` and
> `trusty-analyzer-service`, which are intentionally workspace-internal
> (`publish = false`). The name `trusty-embedder` collides with an unrelated
> crate already on crates.io; rather than rename our internal type, the crate
> and its dependents are not uploaded. The binary is the supported
> distribution unit for embedded/server functionality.

## Quick Start

```bash
# trusty-search must be running first
trusty-search daemon

# Run the analyzer sidecar
trusty-analyze serve --search-url http://127.0.0.1:7878

# Analyze an index
trusty-analyze analyze <index-id> --top-k 20
```

## Features

- Cyclomatic and cognitive complexity per chunk, file, and index
- Code smell detection with configurable thresholds
- Quality grade aggregation (A–F)
- Git blame temporal decay scoring
- Concept clustering (k-means over embeddings)
- Facts store: `(subject, predicate, object)` knowledge triples
- Full HTTP API + MCP stdio server (tool parity)

## Workspace

```
crates/
  trusty-common/          shared types (also used by trusty-search)
  trusty-analyzer-core/   analysis engines
  trusty-analyzer-service/ axum HTTP daemon
  trusty-analyzer-mcp/    MCP stdio server
src/main.rs               CLI binary
```

## Development

```bash
cargo build
cargo test --workspace
cargo clippy --all-targets --all-features -- -D warnings
```

See [CLAUDE.md](./CLAUDE.md) for full architecture, API reference, and project history.
