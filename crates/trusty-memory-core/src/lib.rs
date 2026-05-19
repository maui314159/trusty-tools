//! trusty-memory-core: core types, storage, and retrieval primitives.
//!
//! Why: Centralizes the Memory Palace data model and storage abstractions so that
//! any binary (CLI, MCP server, embedded library) reuses the same types.
//! What: Re-exports the palace hierarchy types and the registry.
//! Test: `cargo test -p trusty-memory-core` exercises construction and registry roundtrips.

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
