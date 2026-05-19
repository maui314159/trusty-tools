# trusty-mcp-core

Shared JSON-RPC 2.0 / Model Context Protocol (MCP) primitives for the
`trusty-*` ecosystem.

Provides the `Request` / `Response` / `JsonRpcError` envelopes, standard
JSON-RPC error codes, an `initialize`-payload builder, and a small async
stdio dispatch loop suitable for building MCP servers that speak over
stdin/stdout.

## Installation

```sh
cargo add trusty-mcp-core
```

## Quick Example

```rust
use trusty_mcp_core::{run_stdio, Request, Response};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_stdio(|req: Request| async move {
        match req.method.as_str() {
            "ping" => Response::ok(req.id, serde_json::json!({"pong": true})),
            _ => Response::method_not_found(req.id, &req.method),
        }
    })
    .await
}
```

The dispatcher accepts any
`Fn(Request) -> impl Future<Output = Response>`, so handlers can be plain
async functions, closures, or routed through a `match` on `req.method`.

## What's Included

- **`error_codes`** — JSON-RPC 2.0 standard error constants
  (`PARSE_ERROR`, `INVALID_REQUEST`, `METHOD_NOT_FOUND`, etc.).
- **`Request` / `Response` / `JsonRpcError`** — serde-typed envelopes.
- **`Response::ok` / `Response::error` / `Response::method_not_found`** —
  ergonomic constructors that match the JSON-RPC 2.0 wire shape.
- **`run_stdio`** — an async stdio loop that reads newline-delimited JSON
  requests, dispatches each to your handler, and writes responses back.
- **`initialize_response`** — helper for the MCP `initialize` payload.

## License

Licensed under the [Elastic License 2.0](https://www.elastic.co/licensing/elastic-license).

## Repository

<https://github.com/bobmatnyc/trusty-common>
