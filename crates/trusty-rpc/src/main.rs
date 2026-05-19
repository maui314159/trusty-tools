//! `trpc` — general-purpose CLI for JSON-RPC services (stdio + HTTP).
//!
//! Why: across the trusty-* ecosystem we frequently need to poke MCP servers
//! and other JSON-RPC endpoints by hand. Shelling out `printf '{...}' | server`
//! is fiddly and error-prone; `trpc` provides a uniform interface with sane
//! defaults (auto `initialize`, pretty-printed output, httpie-style args).
//! What: a `clap`-based CLI that builds either a `StdioTransport` (`--cmd`) or
//! an `HttpTransport` (`--url`), wraps it in `RpcClient`, dispatches the
//! subcommand, and pretty-prints the response.
//! Test: argument parsing has unit tests below; transports and client logic
//! are covered in their own modules.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;

mod client;
mod output;
mod transport;

use client::RpcClient;
use transport::{HttpTransport, StdioTransport, Transport};

#[derive(Parser, Debug)]
#[command(name = "trpc", version, about = "JSON-RPC service CLI (stdio + HTTP)")]
struct Cli {
    /// Launch subprocess; communicate via stdin/stdout (newline-delimited JSON).
    #[arg(long, conflicts_with = "url", value_name = "CMD")]
    cmd: Option<String>,

    /// HTTP endpoint URL to POST JSON-RPC requests to.
    #[arg(long, conflicts_with = "cmd", value_name = "URL")]
    url: Option<String>,

    /// Enable verbose tracing to stderr.
    #[arg(short, long)]
    verbose: bool,

    /// Print raw JSON response instead of pretty-printed output.
    #[arg(long)]
    raw: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Send initialize handshake and print server info.
    Init,

    /// Tool-related commands.
    Tools {
        #[command(subcommand)]
        subcommand: ToolsCommand,
    },

    /// Send an arbitrary JSON-RPC request.
    Request {
        /// Method name (e.g. "ping", "resources/list").
        method: String,
        /// Params as a JSON string (overrides any KEY= positional args).
        #[arg(long)]
        params: Option<String>,
        /// KEY=VALUE (string) or KEY:=JSON (raw JSON) arguments.
        args: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ToolsCommand {
    /// List all available tools.
    List,
    /// Call a tool by name.
    Call {
        /// Tool name.
        name: String,
        /// KEY=VALUE (string) or KEY:=JSON arguments (httpie-style).
        args: Vec<String>,
    },
}

/// Parse httpie-style positional args into a JSON object.
///
/// Why: typing nested JSON on the shell is painful; the `key=val` /
/// `key:=jsonval` convention from httpie/jq is fast and unambiguous.
/// What: each `KEY=VALUE` becomes `{"KEY": "VALUE"}` (string); each
/// `KEY:=JSON` parses `JSON` with serde and inserts the resulting `Value`.
/// `:=` is matched before `=` so values containing `=` after the operator
/// work correctly.
/// Test: `parse_args_*` unit tests below.
fn parse_args(args: &[String]) -> Result<Value> {
    let mut map = serde_json::Map::new();
    for arg in args {
        if let Some((k, v)) = arg.split_once(":=") {
            let val: Value = serde_json::from_str(v)
                .with_context(|| format!("invalid JSON value for '{k}': {v}"))?;
            map.insert(k.to_string(), val);
        } else if let Some((k, v)) = arg.split_once('=') {
            map.insert(k.to_string(), Value::String(v.to_string()));
        } else {
            anyhow::bail!("argument '{arg}' must be KEY=VALUE or KEY:=JSON");
        }
    }
    Ok(Value::Object(map))
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let default = if verbose { "debug" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let transport: Arc<dyn Transport> = match (&cli.cmd, &cli.url) {
        (Some(cmd), None) => Arc::new(
            StdioTransport::new(cmd)
                .await
                .context("starting stdio transport")?,
        ),
        (None, Some(url)) => Arc::new(HttpTransport::new(url.clone())),
        _ => anyhow::bail!("exactly one of --cmd or --url is required"),
    };
    let client = RpcClient::new(transport);

    // Always attempt the MCP handshake before dispatching. For non-MCP HTTP
    // JSON-RPC endpoints, the server will return METHOD_NOT_FOUND, which we
    // silently swallow (except for the `init` subcommand, which surfaces it).
    let init_result = client.initialize().await;
    let is_init_cmd = matches!(cli.command, Command::Init);
    let init_value = match &init_result {
        Ok(v) => Some(v.clone()),
        Err(e) => {
            if is_init_cmd {
                return Err(anyhow::anyhow!("initialize failed: {e}"));
            }
            tracing::debug!("initialize swallowed (non-MCP transport?): {e}");
            None
        }
    };

    match cli.command {
        Command::Init => {
            let v = init_value.expect("init_value is Some when init succeeded");
            if cli.raw {
                output::print_json(&v);
            } else {
                output::print_server_info(&v);
            }
        }
        Command::Tools { subcommand } => match subcommand {
            ToolsCommand::List => {
                let v = client.tools_list().await?;
                if cli.raw {
                    output::print_json(&v);
                } else {
                    output::print_tools_list(&v);
                }
            }
            ToolsCommand::Call { name, args } => {
                let arguments = parse_args(&args)?;
                let v = client.tools_call(&name, arguments).await?;
                if cli.raw {
                    output::print_json(&v);
                } else {
                    output::print_tool_result(&v);
                }
            }
        },
        Command::Request {
            method,
            params,
            args,
        } => {
            let params_val = if let Some(p) = params {
                Some(
                    serde_json::from_str::<Value>(&p)
                        .with_context(|| format!("invalid --params JSON: {p}"))?,
                )
            } else if !args.is_empty() {
                Some(parse_args(&args)?)
            } else {
                None
            };
            let v = client.request(&method, params_val).await?;
            output::print_json(&v);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_args_string_value() {
        let args = vec!["query=is:unread".to_string()];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"query": "is:unread"}));
    }

    #[test]
    fn parse_args_raw_json_number() {
        let args = vec!["max_results:=10".to_string()];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"max_results": 10}));
    }

    #[test]
    fn parse_args_raw_json_object() {
        let args = vec![r#"filter:={"unread":true}"#.to_string()];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"filter": {"unread": true}}));
    }

    #[test]
    fn parse_args_raw_json_array() {
        let args = vec!["ids:=[1,2,3]".to_string()];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"ids": [1, 2, 3]}));
    }

    #[test]
    fn parse_args_mixed() {
        let args = vec![
            "name=alice".to_string(),
            "age:=42".to_string(),
            "admin:=true".to_string(),
        ];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"name": "alice", "age": 42, "admin": true}));
    }

    #[test]
    fn parse_args_value_with_equals() {
        // The first `=` is the separator; remainder preserved.
        let args = vec!["query=key=value".to_string()];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"query": "key=value"}));
    }

    #[test]
    fn parse_args_colon_equals_takes_precedence_over_equals() {
        // `:=` must be matched before `=` so this is parsed as raw JSON.
        let args = vec!["x:=5".to_string()];
        let v = parse_args(&args).unwrap();
        assert_eq!(v, json!({"x": 5}));
    }

    #[test]
    fn parse_args_empty() {
        let v = parse_args(&[]).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn parse_args_rejects_bareword() {
        let args = vec!["bareword".to_string()];
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_args_rejects_invalid_json() {
        let args = vec!["x:={not json".to_string()];
        assert!(parse_args(&args).is_err());
    }
}
