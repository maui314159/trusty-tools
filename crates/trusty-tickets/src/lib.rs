//! `trusty-tickets` — unified ticketing MCP server.
//!
//! Why: open-mpm and the rest of the trusty-* suite need a single MCP
//! surface that can talk to GitHub Issues, JIRA, and Linear without the
//! caller knowing which backend is configured.
//! What: Re-exports `api::*` plus the `server` and `tools` modules used
//! by the binary.
//! Test: each submodule has its own unit tests; `tests/config.rs` covers
//! end-to-end config loading.

pub mod api;
pub mod server;
pub mod tools;
