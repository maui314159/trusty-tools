//! Memory Palace core types, storage, and retrieval (formerly the
//! `trusty-memory-core` crate).
//!
//! Why: Centralises the Memory Palace data model and storage abstractions
//! so every binary (CLI, MCP server, embedded library) reuses the same
//! types. Absorbed into `trusty-common` (issue #5 phase 2d) so the trusty-*
//! toolchain links a single internal library and we ship one fewer
//! published crate.
//! What: Re-exports the palace hierarchy (`Palace`, `Wing`, `Room`,
//! `Drawer`), the registry, and the retrieval handle. Gated behind the
//! `memory-core` feature because it pulls in heavy storage deps
//! (`usearch`, `rusqlite`, `r2d2`, `tiktoken-rs`, `git2`).
//! Test: Each submodule keeps its existing unit tests; `cargo test -p
//! trusty-common --features memory-core` exercises the full surface.

pub mod analytics;
pub mod decay;
pub mod dream;
pub mod embed;
pub mod git;
pub mod palace;
pub mod registry;
pub mod retrieval;
pub mod store;

pub use palace::{Drawer, Palace, PalaceId, Room, RoomType, Wing};
pub use registry::PalaceRegistry;
pub use retrieval::PalaceHandle;
