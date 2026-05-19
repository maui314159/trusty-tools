//! Google Drive service sub-modules.
//!
//! Why: Drive surface splits cleanly into file operations and sharing.
//! What: Re-exports `files` and `sharing` sub-modules.
//! Test: Per-sub-module.

pub mod files;
pub mod sharing;
