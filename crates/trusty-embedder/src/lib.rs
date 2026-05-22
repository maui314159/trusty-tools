//! **Deprecated:** absorbed into `trusty-common` behind the `embedder` feature.
//!
//! This crate is now a thin re-export shim kept solely so existing dependants
//! (trusty-search, trusty-memory-core) continue to compile during the
//! migration window. New code should depend on
//! `trusty-common = { features = ["embedder"] }` directly and `use
//! trusty_common::embedder::*` instead.

pub use trusty_common::embedder::*;

// Issue #55: portable RSS measurement helper used by the candle Metal
// validation benchmark (`bin/candle_metal_bench.rs`). Always compiled —
// the helper is a thin sysinfo wrapper and the unit tests do not require
// the `candle` feature.
pub mod rss;
