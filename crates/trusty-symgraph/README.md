# trusty-symgraph

Standalone symbol-graph engine: parse source trees with tree-sitter into a
content-addressed `SymbolRegistry`, query the resulting knowledge graph,
edit symbols in place, and deterministically emit them back to disk.

The core idea: **agents should operate in symbol space, not file space**.
Files are a derived artifact of the registry. `trusty-symgraph` bundles
parse → registry → emit, plus editor primitives and a graph query surface,
behind a single library crate so tools and other agents can consume the
substrate without depending on a larger orchestrator binary.

## Installation

```sh
cargo add trusty-symgraph
```

To also pull in the optional HTTP server (and the `trusty-symgraph` binary):

```sh
cargo add trusty-symgraph --features server
```

## Quick Example

```rust
use std::path::PathBuf;
use trusty_symgraph::parser::parse_directory;

fn main() -> anyhow::Result<()> {
    let src = PathBuf::from("./src");
    let registry = parse_directory(&src, &src)?;
    println!("parsed {} symbols", registry.len());

    for (id, entry) in registry.iter().take(5) {
        println!("  {} :: {:?}", id.0, entry.kind);
    }
    Ok(())
}
```

## Supported Languages

Tree-sitter grammars bundled for: **Rust, Python, JavaScript, TypeScript,
Go, Java, C, C++**.

## Feature Flags

- **`server`** *(optional)* — enables the `server` module and builds the
  `trusty-symgraph` binary, which exposes the registry over HTTP
  (`/health`, `/parse`, `/symbols`, `/symbol/{id}`, `/emit`, `/verify`,
  `/graph`) on port 7700 by default.

By default the crate is library-only: no axum, no tokio runtime, no HTTP
surface.

## Running the Server

```sh
cargo install trusty-symgraph --features server
trusty-symgraph --port 7700 --dir ./my-project
curl http://localhost:7700/health
```

## License

Licensed under either of

- Apache License, Version 2.0
- MIT License

at your option.

## Repository

<https://github.com/bobmatnyc/trusty-common>
