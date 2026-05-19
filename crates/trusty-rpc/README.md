# trusty-rpc

General-purpose JSON-RPC CLI for the Trusty suite. Ships the `trpc` binary —
a small but ergonomic client for poking at MCP servers and any other
JSON-RPC 2.0 endpoint over either **stdio subprocess** or **HTTP POST**.

Use it to:

- Drive an MCP server by hand (`tools/list`, `tools/call`, `initialize`).
- Debug a JSON-RPC service exposed over HTTP without writing a curl chain.
- Script ad-hoc RPC calls with httpie-style argument syntax.

## Installation

```sh
cargo install --path crates/trusty-rpc
```

This installs the `trpc` binary into `~/.cargo/bin`.

## Quick Start

```sh
# Spawn an MCP server over stdio and list its tools.
trpc --cmd "trusty-search mcp" tools list

# Call a tool with httpie-style args.
trpc --cmd "trusty-memory mcp" tools call memory_recall query="rate limits" limit:=5

# Send an arbitrary JSON-RPC request.
trpc --cmd "trusty-analyze mcp" request resources/list

# Talk to a JSON-RPC HTTP endpoint instead.
trpc --url http://localhost:7700/rpc request ping

# Raw JSON output for piping into jq.
trpc --cmd "trusty-search mcp" --raw tools list | jq '.tools | length'
```

## Commands

| Command                          | Purpose                                              |
|----------------------------------|------------------------------------------------------|
| `init`                           | Send `initialize` and print server info              |
| `tools list`                     | Send `tools/list` and pretty-print the result        |
| `tools call NAME [KEY=VAL ...]`  | Send `tools/call` with `arguments`                   |
| `request METHOD [KEY=VAL ...]`   | Send an arbitrary JSON-RPC request                   |

### Argument syntax (httpie-style)

- `KEY=VALUE` — inserts `{"KEY": "VALUE"}` as a JSON string.
- `KEY:=JSON` — parses `JSON` with serde and inserts the resulting value.
  Use for numbers, booleans, arrays, and objects: `limit:=10`,
  `flags:=true`, `tags:='["a","b"]'`.
- `--params '<json>'` — override `params` wholesale (only on `request`).

Examples:

```sh
trpc --cmd "..." tools call query name=foo limit:=10 enabled:=true
trpc --url ... request my/method --params '{"a":1,"b":[2,3]}'
```

## Transports

| Flag           | Behaviour                                                                |
|----------------|--------------------------------------------------------------------------|
| `--cmd "<CMD>"` | Spawn `<CMD>` as a subprocess. Sends requests as newline-delimited JSON on stdin and reads responses from stdout. Suitable for any MCP stdio server. |
| `--url <URL>`  | POST JSON-RPC requests to `<URL>`. Suitable for HTTP RPC endpoints.       |

Exactly one of `--cmd` or `--url` is required.

## MCP Handshake

`trpc` always sends `initialize` once at startup. For stdio MCP servers
this primes the session before any tool call. For non-MCP HTTP endpoints
the resulting `METHOD_NOT_FOUND` is silently swallowed (unless you ran
`trpc init`, which surfaces the error).

## Output Modes

| Flag      | Behaviour                                                       |
|-----------|-----------------------------------------------------------------|
| (default) | Pretty-printed, colourised summaries (tool tables, server info) |
| `--raw`   | Emit the raw JSON-RPC `result` for piping into `jq` / scripts   |
| `-v`      | Enable `tracing` to stderr (level `debug`)                      |

## Architecture

- **`transport`** — `Transport` trait + two implementations: `StdioTransport`
  (manages a child process with `tokio::process::Command`) and
  `HttpTransport` (`reqwest::Client` wrapper). Both speak JSON-RPC 2.0.
- **`client`** — `RpcClient` wraps an `Arc<dyn Transport>` and exposes the
  three MCP convenience methods (`initialize`, `tools_list`, `tools_call`)
  plus a generic `request`. Generates UUID-v4 request IDs and surfaces
  `error` objects as `anyhow::Error`.
- **`output`** — pretty-printers for tool lists, tool call results, and
  server info. Uses `colored` for terminal-aware ANSI.
- **`main`** — `clap` CLI, argument parsing (`parse_args` implements the
  httpie syntax), and command dispatch.

## Design Notes

- **Single-shot per invocation.** `trpc` opens the transport, sends the
  request(s), prints the result, and exits. State (subprocess lifetime,
  HTTP client) is scoped to one CLI invocation by design.
- **No retry policy.** If the upstream fails, you see the error. Retries
  are the caller's responsibility.
- **Explicit transport selection.** `--cmd` and `--url` are mutually
  exclusive; there's no auto-detection. Keeps behaviour predictable for
  scripts.

## Testing

```sh
cargo test -p trusty-rpc
```

23 tests cover argument parsing, JSON-RPC envelope handling, error
extraction, and end-to-end behaviour against a mock stdio server.

## License

Licensed under the [Elastic License 2.0](https://www.elastic.co/licensing/elastic-license).

## Repository

<https://github.com/bobmatnyc/trusty-common>
