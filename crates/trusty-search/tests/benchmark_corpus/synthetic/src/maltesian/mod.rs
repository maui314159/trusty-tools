//! Maltesian routing subsystem.
//!
//! Why: encrypted telemetry has to be routed to one of several outbound
//! channels depending on its content tag; the maltesian router is that
//! dispatch layer.
//! What: re-exports the router and relay types.
//! Test: child modules own all tests.

pub mod circuit;
pub mod relay;

pub use circuit::{route_maltesian_circuit, MaltesianRouter};
pub use relay::MaltesianRelay;
