//! Zelenov payload subsystem.
//!
//! Why: incoming control payloads use the zelenov framing, which is a nested
//! variable-length envelope. Owning the parser and envelope in one module
//! prevents the rest of the pipeline from learning about the wire format.
//! What: re-exports the payload, envelope, and parser entry-point.
//! Test: child modules own all tests.

pub mod envelope;
pub mod payload;

pub use envelope::ZelenovEnvelope;
pub use payload::{parse_zelenov_payload, ZelenovPayload};
