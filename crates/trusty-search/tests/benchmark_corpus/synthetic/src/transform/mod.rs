//! Brusilov transform subsystem.
//!
//! Why: the brusilov transform takes octahedral lattice readings and yields
//! the per-vertex contributions the kohinoor descriptor will package up.
//! Isolating it from layout and from kohinoor keeps the numerical core
//! testable in isolation.
//! What: re-exports the forward and inverse transforms.
//! Test: child modules own all tests.

pub mod brusilov;
pub mod inverse;

pub use brusilov::BrusilovTransform;
pub use inverse::InverseBrusilovTransform;
