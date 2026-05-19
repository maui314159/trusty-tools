//! `symgraph` HTTP server binary entrypoint (#351).
//!
//! Why: External tools and other agents need a runnable command that boots
//! the symbol-registry HTTP surface without linking against the library.
//! Producing a thin `[[bin]]` keeps the heavy axum/tokio tree behind the
//! `server` feature while still shipping a usable CLI.
//! What: Parses `--port <u16>` (default 7700) and `--dir <path>` (default `.`)
//! from `std::env::args`, parses the directory into a `SymbolRegistry`, and
//! hands it to `symgraph::server::serve`.
//! Test: Run `symgraph --dir <some-src-dir>` and curl `/health` — see the
//! smoke test in the PR description for #351.

use std::path::PathBuf;
use std::process::ExitCode;

use trusty_symgraph::parser::parse_directory;
use trusty_symgraph::server::{AppState, DEFAULT_PORT, serve};

const USAGE: &str = "\
symgraph — symbol-graph HTTP server

USAGE:
    symgraph [--port <port>] [--dir <path>]

OPTIONS:
    --port <port>   TCP port to bind on 0.0.0.0 (default: 7700)
    --dir <path>    Directory to parse on startup (default: .)
    -h, --help      Print this help and exit
";

struct Args {
    port: u16,
    dir: PathBuf,
}

/// Why: Keep the CLI parser tiny and dependency-free so the `server`
/// feature doesn't pull in `clap` just for two flags.
/// What: Walks `std::env::args` and recognizes `--port`, `--dir`, and
/// `-h/--help`. Returns `Err(message)` for anything malformed.
/// Test: Pass `--port 8080 --dir /tmp` and assert the parsed struct;
/// pass `--port` with no value and assert the error message.
fn parse_args<I: IntoIterator<Item = String>>(iter: I) -> Result<Args, String> {
    let mut port: u16 = DEFAULT_PORT;
    let mut dir: PathBuf = PathBuf::from(".");
    let mut it = iter.into_iter();
    // Skip program name.
    let _ = it.next();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err("__help__".into()),
            "--port" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--port requires a value".to_string())?;
                port = v
                    .parse::<u16>()
                    .map_err(|e| format!("invalid --port value '{v}': {e}"))?;
            }
            "--dir" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--dir requires a value".to_string())?;
                dir = PathBuf::from(v);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args { port, dir })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args()) {
        Ok(a) => a,
        Err(msg) if msg == "__help__" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("error: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let dir = match args.dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot resolve --dir {}: {e}", args.dir.display());
            return ExitCode::FAILURE;
        }
    };

    eprintln!("symgraph: parsing directory {}", dir.display());
    let registry = match parse_directory(&dir, &dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: failed to parse {}: {e:#}", dir.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!("symgraph: registry built ({} symbols)", registry.len());

    let state = AppState::new(registry);
    println!("symgraph serving on http://0.0.0.0:{}", args.port);
    if let Err(e) = serve(state, args.port).await {
        eprintln!("error: server exited: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
