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
use trusty_memory::commands::service::{handle_service, ServiceAction};
use trusty_memory::commands::setup::handle_setup;
use trusty_memory::{resolve_palace_registry_dir, run_http, run_stdio, AppState};

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

    /// First-time setup: data dir + launchd (macOS) + Claude settings patch.
    Setup,

    /// Manage the macOS launchd LaunchAgent for the daemon.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Monitor the trusty-memory daemon via web UI or terminal dashboard.
    ///
    /// `monitor web` prints the daemon's admin-panel URL; `monitor tui`
    /// launches the trusty-memory-specific ratatui dashboard: a palace list,
    /// a live dream/recall activity log, and a recall query bar.
    #[command(subcommand_required = true)]
    Monitor {
        #[command(subcommand)]
        target: MonitorTarget,
    },
}

/// Target surface for the `monitor` subcommand.
///
/// Why: operators want a quick link to the daemon's web UI, the
/// memory-specific terminal UI, OR the same dashboard data as plain text /
/// JSON so scripts and CI can read it without a TUI (issues #33, #34).
/// What: `Web` prints the daemon's `/ui` URL; `Tui` launches the
/// trusty-memory-specific `trusty_common::monitor::memory_tui` dashboard;
/// `Status` and `Palaces` print scriptable health and per-palace stats.
/// Test: `cargo run -p trusty-memory -- monitor --help` lists every variant.
#[derive(Debug, Subcommand)]
enum MonitorTarget {
    /// Open the web dashboard URL in the terminal (or browser).
    Web,
    /// Launch the trusty-memory terminal UI: palaces, recall, and dream monitor.
    Tui,
    /// Print daemon status: version and aggregate palace/drawer/vector counts.
    ///
    /// Examples:
    ///   trusty-memory monitor status
    ///   trusty-memory monitor status --json
    Status {
        /// Emit the status as a JSON object instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// List every palace, or show one palace's detail when an ID is given.
    ///
    /// Examples:
    ///   trusty-memory monitor palaces
    ///   trusty-memory monitor palaces default
    ///   trusty-memory monitor palaces --json
    Palaces {
        /// Optional palace ID to show detail for (omit to list all).
        id: Option<String>,
        /// Emit the result as JSON instead of a plain-text table.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Issue #35: initialise tracing with an in-memory `LogBuffer` so the HTTP
    // daemon's `GET /api/v1/logs/tail` endpoint can serve recent logs. The
    // buffer-backed subscriber still writes the standard `fmt` layer to
    // stderr, so non-HTTP subcommands (and the MCP stdio path, which must
    // keep stdout clean) are unaffected. The buffer is only wired into the
    // `AppState` on the HTTP serve path.
    let log_buffer = trusty_common::init_tracing_with_buffer(
        cli.verbose,
        trusty_common::log_buffer::DEFAULT_LOG_CAPACITY,
    );

    match cli.command {
        Command::Serve { http, palace } => run_serve(http, palace, log_buffer).await,
        Command::Migrate {
            target,
            dry_run,
            config_only,
        } => handle_migrate(target, dry_run, config_only),
        Command::Setup => handle_setup(),
        Command::Service { action } => handle_service(&action),
        Command::Monitor { target } => run_monitor(target).await,
    }
}

/// Dispatch the `monitor` subcommand.
///
/// Why: keeps `main` focused on parsing while putting the daemon-address
/// discovery and dashboard launch in one place.
/// What: `Web` resolves the live daemon address from the lock file and prints
/// its `/ui` URL (exiting non-zero when no daemon is running); `Tui` launches
/// the trusty-memory-specific `trusty_common::monitor::memory_tui` ratatui
/// dashboard; `Status` and `Palaces` print scriptable health and per-palace
/// stats via the `commands::monitor` handlers.
/// Test: not unit-tested (process-level entry point); `cargo run -p
/// trusty-memory -- monitor --help` lists every target.
async fn run_monitor(target: MonitorTarget) -> Result<()> {
    use trusty_memory::commands::monitor;
    match target {
        MonitorTarget::Web => match trusty_common::read_daemon_addr("trusty-memory")? {
            Some(addr) => {
                println!("{addr}/ui");
                Ok(())
            }
            None => {
                eprintln!("trusty-memory daemon not running (no address found)");
                std::process::exit(1);
            }
        },
        MonitorTarget::Tui => trusty_common::monitor::memory_tui::run().await,
        MonitorTarget::Status { json } => monitor::handle_status(json).await,
        MonitorTarget::Palaces { id, json } => monitor::handle_palaces(id, json).await,
    }
}

/// Dispatch `serve` to either the stdio loop or the HTTP server.
///
/// Why: keeps `main` focused on parsing while putting the `AppState`
/// construction in one place.
/// What: resolves the palace registry directory (descending into the legacy
/// `palaces/` subdirectory when present — see `resolve_palace_registry_dir`),
/// builds an `AppState` rooted there, applies the `--palace` default if any,
/// re-hydrates every persisted palace, and — on the HTTP path — wires the
/// issue-#35 `LogBuffer` so `GET /api/v1/logs/tail` serves captured logs. The
/// stdio path does not need the buffer (no HTTP surface), so it is dropped
/// there.
/// Test: not unit-tested (process-level entry point); exercised manually via
/// `cargo run -p trusty-memory -- serve` and the parent integration tests.
async fn run_serve(
    http: Option<SocketAddr>,
    palace: Option<String>,
    log_buffer: trusty_common::log_buffer::LogBuffer,
) -> Result<()> {
    // Resolve the standard data dir, then descend into `palaces/` if that
    // legacy-layout subdirectory exists. Using the resolved directory as
    // `data_root` keeps every call site (status, palace_list, open_palace,
    // palace_create, load_palaces_from_disk) pointed at the same place.
    let data_dir = trusty_common::resolve_data_dir("trusty-memory")?;
    let data_root = resolve_palace_registry_dir(data_dir);

    if let Some(addr) = http {
        let state = AppState::new(data_root)
            .with_default_palace(palace)
            .with_log_buffer(log_buffer);
        // Re-hydrate every on-disk palace before serving so the dashboard and
        // `palace_list` see the full set immediately after a restart. Without
        // this the registry starts empty and palaces "disappear" until a tool
        // call lazily re-opens them one at a time.
        let count = state.load_palaces_from_disk().await?;
        tracing::info!("Loaded {count} palaces from disk");
        run_http(state, addr).await
    } else {
        let state = AppState::new(data_root).with_default_palace(palace);
        let count = state.load_palaces_from_disk().await?;
        tracing::info!("Loaded {count} palaces from disk");
        run_stdio(state).await
    }
}
