//! **Deprecated:** This crate has been absorbed into `trusty-common`.
//!
//! Use `trusty-common` with the `mcp` feature instead:
//!
//! ```toml
//! [dependencies]
//! trusty-common = { version = "0.3", features = ["mcp"] }
//! ```
//!
//! ```rust,ignore
//! use trusty_common::mcp::{Request, Response, error_codes, initialize_response, run_stdio_loop};
//! ```
//!
//! Why: every MCP server in the workspace was already importing the same
//! primitives twice (once via this crate, once transitively via
//! `trusty-common`). Consolidating into one feature-gated module under
//! `trusty-common` keeps the dependency surface flat (issue #5 phase 2a).
//!
//! What: this crate is now a thin re-export shim. All types, constants, and
//! helpers live in [`trusty_common::mcp`] and are re-exported here at the
//! same public paths so existing `use trusty_mcp_core::...` imports keep
//! working during the deprecation window.
//!
//! Test: the canonical tests live in `trusty_common::mcp`. This crate has no
//! tests of its own.

#![deprecated(
    since = "0.1.3",
    note = "absorbed into `trusty-common` behind the `mcp` feature; \
            depend on `trusty-common = { features = [\"mcp\"] }` and \
            import from `trusty_common::mcp` instead"
)]

pub use trusty_common::mcp::{
    JsonRpcError, Request, Response, ServiceDescriptor, error_codes, initialize_response,
    run_stdio_loop,
};

pub mod openrpc {
    //! Re-export of [`trusty_common::mcp::openrpc`] for backward compatibility.
    pub use trusty_common::mcp::openrpc::*;
}

pub mod service {
    //! Re-export of [`trusty_common::mcp::service`] for backward compatibility.
    pub use trusty_common::mcp::service::*;
}
