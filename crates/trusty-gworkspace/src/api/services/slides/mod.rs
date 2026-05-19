//! Google Slides service.
//!
//! Why: Slides API surface is small for agent workflows: read deck, create
//! deck/slide, add content. We expose those three tools.
//! What: Re-exports `core` sub-module.
//! Test: Per-sub-module.

pub mod core;
