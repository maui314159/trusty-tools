//! Per-subcommand handlers for the `trusty-memory` binary.
//!
//! Why: Keep CLI handlers out of `lib.rs` so the library surface stays focused
//! on MCP server code while the binary stays a thin clap-to-handler shim.
//! What: One submodule per subcommand. Today only `migrate` exists; `serve`
//! is wired directly to `crate::run_stdio` / `crate::run_http` in `main.rs`.
//! Test: Each submodule carries its own unit tests.

pub mod migrate;
