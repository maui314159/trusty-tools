# trusty-common

Shared utility surface for the `trusty-*` family of CLI tools and daemons (trusty-memory, trusty-search, etc.).

Centralizes port auto-detection, data-directory resolution, tracing/CLI init, `NO_COLOR` handling, and a minimal OpenRouter chat-completions client so each tool doesn't reinvent (and subtly diverge on) the basics.

## Installation

```sh
cargo add trusty-common
```

To pull in the optional axum HTTP-server middleware helpers:

```sh
cargo add trusty-common --features axum-server
```

## Quick Example

```rust
use trusty_common::{bind_with_auto_port, data_dir};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Find the user's per-app data directory (creates it if missing).
    let dir = data_dir("trusty-example")?;
    println!("data dir: {}", dir.display());

    // Bind 127.0.0.1:7777, walking forward up to 10 ports if busy.
    let addr = "127.0.0.1:7777".parse()?;
    let listener = bind_with_auto_port(addr, 10).await?;
    println!("listening on {}", listener.local_addr()?);

    Ok(())
}
```

## Feature Flags

- **`axum-server`** *(optional)* — enables the `server` module: standard axum
  middleware stack (CORS, tracing, gzip with SSE carve-out) plus a fast-fail
  `reqwest::Client` configured for daemon-to-daemon calls.

By default the crate is dependency-light: only `tokio`, `serde`, `reqwest`,
`tracing`, etc. Axum is pulled in only when the `axum-server` feature is on.

## What's Included

- **Port binding** — `bind_with_auto_port` walks forward through ports when the
  requested one is busy, so restarts don't fail noisily.
- **Data directory** — `data_dir(app_name)` resolves an OS-appropriate
  per-application directory and creates it if missing.
- **Tracing/CLI init** — opinionated `tracing_subscriber` setup with
  `RUST_LOG` and `NO_COLOR` respected out of the box.
- **OpenRouter client** — a small typed wrapper over OpenRouter's
  chat-completions API for LLM-backed tooling.
- **Server middleware** *(feature-gated)* — shared axum middleware stack so
  every `trusty-*` daemon gets identical CORS/trace/gzip behavior.

## License

Licensed under the [Elastic License 2.0](https://www.elastic.co/licensing/elastic-license).

## Repository

<https://github.com/bobmatnyc/trusty-common>
