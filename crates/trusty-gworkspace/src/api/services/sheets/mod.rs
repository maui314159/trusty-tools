//! Google Sheets service.
//!
//! Why: Sheets v4 surface is large but agent workflows typically need
//! get/create/read values/write values/format — exactly the four tools we
//! expose.
//! What: Re-exports `core` sub-module.
//! Test: Per-sub-module.

pub mod core;
