//! Cascade admission subsystem.
//!
//! Why: every sample entering the pipeline passes through a Lichtenberg
//! cascade that rate-limits and shapes the stream. Keeping the cascade
//! contract in one module lets us swap the back-pressure policy without
//! touching producers.
//! What: re-exports the Lichtenberg cascade and the retrograde variant
//! used for replay.
//! Test: child modules own all tests.

pub mod lichtenberg;
pub mod retrograde;

pub use lichtenberg::LichtenbergCascade;
pub use retrograde::RetrogradeCascade;
