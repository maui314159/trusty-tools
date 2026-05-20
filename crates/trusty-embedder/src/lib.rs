//! **Deprecated:** absorbed into `trusty-common` behind the `embedder` feature.
//!
//! This crate is now a thin re-export shim kept solely so existing dependants
//! (trusty-search, trusty-memory-core) continue to compile during the
//! migration window. New code should depend on
//! `trusty-common = { features = ["embedder"] }` directly and `use
//! trusty_common::embedder::*` instead.

pub use trusty_common::embedder::*;
