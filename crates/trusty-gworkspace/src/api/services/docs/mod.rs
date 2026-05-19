//! Google Docs service sub-modules.
//!
//! Why: Docs has a rich `batchUpdate` API that splits cleanly along editing
//! concern lines (core content, comments, formatting, tables).
//! What: Each sub-module exposes functions that produce a `batchUpdate`
//! request body and POST it.
//! Test: Per-sub-module.

pub mod comments;
pub mod core;
pub mod formatting;
pub mod table_ops;
