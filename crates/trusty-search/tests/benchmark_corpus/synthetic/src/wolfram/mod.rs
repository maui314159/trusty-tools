//! Wolfram registry subsystem.
//!
//! Why: the registry is the durable sink for everything the pipeline
//! produces; isolating it from production lets the storage format evolve
//! (today it's an in-memory map) without touching upstream code.
//! What: re-exports the registry and inventory types.
//! Test: child modules own all tests.

pub mod inventory;
pub mod registry;

pub use inventory::WolframInventory;
pub use registry::WolframRegistry;
