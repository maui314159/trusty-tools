//! CLI entry point for the `trusty-memory` binary.
//!
//! Why: ship a thin clap-to-handler shim so users can `cargo install
//! trusty-memory` and invoke either `trusty-memory serve` (the MCP stdio
//! server consumed by Claude Code) or `trusty-memory migrate kuzu-memory`
//! (which rewrites Claude settings files that still reference the legacy
//! kuzu-memory MCP server). All real logic lives in the library and the
//! `commands::migrate` module — this file does CLI parsing and dispatch only.
//! What: defines a `clap::Parser` with `serve` and `migrate` subcommands.
//! `serve` defers to `trusty_memory::run_stdio` (or `run_http` when `--http`
//! is supplied); `migrate` defers to `commands::migrate::handle_migrate`.
//! Test: `cargo run -p trusty-memory -- --help` lists both subcommands.
//! `cargo run -p trusty-memory -- migrate kuzu-memory --dry-run` exercises
//! the migrate path end-to-end without modifying any files.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use trusty_memory::commands::migrate::{handle_migrate, MigrateTarget};
use trusty_memory::{run_http, run_stdio, AppState};

/// Top-level CLI for `trusty-memory`.
#[derive(Debug, Parser)]
#[command(
    name = "trusty-memory",
    version,
    about = "Memory palace MCP server + migration utility",
    long_about = "MCP server (stdio + HTTP/SSE) for trusty-memory, plus a \
                  `migrate kuzu-memory` subcommand that rewrites Claude \
                  settings files referencing the legacy kuzu-memory server."
)]
struct Cli {
    /// Increase tracing verbosity (`-v` = debug, `-vv` = trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

/// Top-level subcommands.
///
/// Why: keep the surface small and mirror the `trusty-search` pattern so
/// users moving between the two tools have a consistent experience.
/// What: `serve` runs the MCP server; `migrate` rewrites Claude settings.
/// Test: clap's `--help` output enumerates both.
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the MCP server (stdio by default, HTTP/SSE with `--http`).
    Serve {
        /// Bind an HTTP/SSE server instead of speaking MCP over stdio.
        #[arg(long, value_name = "ADDR")]
        http: Option<SocketAddr>,

        /// Bind every MCP tool call to this palace when the caller omits the
        /// `palace` argument.
        #[arg(long, value_name = "NAME")]
        palace: Option<String>,
    },

    /// Migrate from another memory MCP server to trusty-memory.
    Migrate {
        /// What to migrate from.
        #[arg(value_enum)]
        target: MigrateTarget,

        /// Print what would change without writing any files.
        #[arg(long)]
        dry_run: bool,

        /// Accepted for parity with `trusty-search migrate`. Today the
        /// migration only has a config phase, so this flag is a no-op.
        #[arg(long)]
        config_only: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    trusty_common::init_tracing(cli.verbose);

    match cli.command {
        Command::Serve { http, palace } => run_serve(http, palace).await,
        Command::Migrate {
            target,
            dry_run,
            config_only,
        } => handle_migrate(target, dry_run, config_only),
    }
}

/// Dispatch `serve` to either the stdio loop or the HTTP server.
///
/// Why: keeps `main` focused on parsing while putting the `AppState`
/// construction in one place.
/// What: builds an `AppState` rooted at the standard data dir, applies the
/// `--palace` default if any, then routes to `run_stdio` or `run_http`.
/// Test: not unit-tested (process-level entry point); exercised manually via
/// `cargo run -p trusty-memory -- serve` and the parent integration tests.
async fn run_serve(http: Option<SocketAddr>, palace: Option<String>) -> Result<()> {
    let data_root = trusty_common::resolve_data_dir("trusty-memory")?;
    let state = AppState::new(data_root).with_default_palace(palace);

    if let Some(addr) = http {
        run_http(state, addr).await
    } else {
        run_stdio(state).await
    }
}
