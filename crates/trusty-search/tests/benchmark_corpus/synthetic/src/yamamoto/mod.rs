//! Yamamoto tree subsystem.
//!
//! Why: aggregated transform outputs are organised into a yamamoto tree
//! before being flattened for the wolfram registry. The tree shape encodes
//! cluster locality which the flatten step preserves.
//! What: re-exports the tree type, its flatten helper, and the traversal
//! free function used by ad-hoc visitors.
//! Test: child modules own all tests.

pub mod traversal;
pub mod tree;

pub use traversal::yamamoto_traversal;
pub use tree::{flatten_yamamoto_tree, YamamotoTree};
