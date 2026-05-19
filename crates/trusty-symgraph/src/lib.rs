//! Standalone symbol-graph engine extracted from `open-mpm`'s `src/ast/`
//! module (#351).
//!
//! Why: Agents should operate in symbol space, not file space. Files are a
//! derived artifact of the registry. Bundling parse → registry → emit and
//! the editor primitives behind one library crate lets other tools consume
//! the substrate without depending on the orchestrator binary.
//! What: Re-exports the same surface that `crate::ast` previously exposed —
//! `Symbol`/`SymbolKind`/`SymbolRegistry`/`SymbolGraph`/`Patch`/edit helpers
//! — backed by tree-sitter parsing, an `IndexMap`-based content-addressed
//! registry, and a deterministic emitter. Adds an opt-in `server` feature
//! that exposes the registry over HTTP on port 7700.
//! Test: Each submodule keeps its existing unit tests; integration tests in
//! `tests/` cover the round-trip.

// Always-on contracts surface — pure data types, no tree-sitter.
// Downstream crates that only need EntityType / RawEntity / EdgeKind /
// fact_hash_str / tables can depend on this crate with
// `default-features = false` and avoid the `links = "tree-sitter"` conflict.
pub mod contracts;

// Parser-path modules — gated behind `parser` feature so non-tree-sitter
// consumers can import the contracts surface without pulling in grammars.
#[cfg(feature = "parser")]
pub mod editor;
#[cfg(feature = "parser")]
pub mod emitter;
#[cfg(feature = "parser")]
pub mod graph;
#[cfg(feature = "parser")]
pub mod parser;
#[cfg(feature = "parser")]
pub mod registry;
#[cfg(feature = "parser")]
pub mod symbol;

// INTENT: Declare the strategy trait and default module-path implementation.
#[cfg(feature = "parser")]
pub mod strategy;

// INTENT: Declare the SCC-based locality clustering strategy.
#[cfg(feature = "parser")]
pub mod locality;

// INTENT: Declare the test-colocation strategy that places tests next to targets.
#[cfg(feature = "parser")]
pub mod test_colocation;

#[cfg(feature = "server")]
pub mod server;

// Public re-exports — mirrors the legacy `crate::ast` surface so
// downstream consumers can adopt the crate by changing one path.
//
// Note: `contracts::EdgeKind` (entity knowledge-graph edges) is *not*
// re-exported at crate root because the name collides with
// `graph::EdgeKind` (symbol-graph structural edges). Access it via
// `trusty_symgraph::contracts::EdgeKind`.
pub use contracts::{EntityType, RawEntity, fact_hash_str};

#[cfg(feature = "parser")]
pub use registry::{SymbolEntry, SymbolId, SymbolRegistry};

#[cfg(feature = "parser")]
pub use editor::{
    Patch, add_import, apply_patch, emit_diff, insert_after_symbol, replace_symbol, validate_syntax,
};
#[cfg(feature = "parser")]
pub use emitter::{LayoutRules, apply_emit, assign_file, emit};
#[cfg(feature = "parser")]
pub use graph::{Edge, EdgeKind, SymbolEdge, SymbolGraph, SymbolNode};
#[cfg(feature = "parser")]
pub use parser::{Language, file_to_module_path, parse_directory, parse_file};
#[cfg(feature = "parser")]
pub use symbol::{Symbol, SymbolKind, detect_language, extract_symbols, get_symbol, list_symbols};

// Re-exports: strategies
#[cfg(feature = "parser")]
pub use locality::LocalityStrategy;
#[cfg(feature = "parser")]
pub use strategy::{EmitStrategy, ModulePathStrategy};
#[cfg(feature = "parser")]
pub use test_colocation::TestColocationStrategy;

// Note: `to_petgraph` was removed in #356. `SymbolGraph` now stores a
// `petgraph::StableGraph` internally — call `SymbolGraph::inner()` to
// borrow it directly for petgraph algorithms.
