//! Re-export shim — `trusty-memory-core` has been absorbed into
//! `trusty-common` under the `memory-core` feature flag (issue #5 phase 2d).
//!
//! Why: Keeps existing call sites (`use trusty_memory_core::palace::Palace`)
//! compiling while the workspace migrates to `trusty_common::memory_core::*`
//! directly. This crate will be removed in a future release once all
//! downstream consumers have switched over.
//! What: Re-exports every public item from `trusty_common::memory_core`.
//! `publish = false` in `Cargo.toml` so this shim never reaches crates.io.
//! Test: `cargo check -p trusty-memory-core` confirms the re-export still
//! resolves; downstream crates exercise it transitively via their own tests.

pub use trusty_common::memory_core::*;
