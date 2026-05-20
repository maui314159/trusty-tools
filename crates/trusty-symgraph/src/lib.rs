//! Deprecated re-export shim.
//!
//! Why: The symbol-graph engine was absorbed into `trusty-common` behind the
//! `symgraph` / `symgraph-parser` features (issue #5 phase 2c). This shim
//! keeps the `trusty_symgraph::*` import path alive so downstream crates can
//! migrate at their own pace.
//! What: Re-exports the entire `trusty_common::symgraph` surface at the
//! crate root. New code should depend directly on `trusty-common` with the
//! appropriate feature flag.
//! Test: `cargo test -p trusty-symgraph` continues to exercise the parser
//! and emitter through this shim.

pub use trusty_common::symgraph::*;

// Re-export submodules so legacy import paths like
// `trusty_symgraph::registry::SymbolKind` continue to resolve.
pub use trusty_common::symgraph::contracts;

#[cfg(feature = "parser")]
pub use trusty_common::symgraph::{
    editor, emitter, graph, locality, parser, registry, strategy, symbol, test_colocation,
};

#[cfg(feature = "server")]
pub use trusty_common::symgraph::server;
