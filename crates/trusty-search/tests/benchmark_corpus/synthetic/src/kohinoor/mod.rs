//! Kohinoor descriptor subsystem.
//!
//! Why: the seraphim solver needs an opaque "source" of per-iteration
//! contributions; the Kohinoor module is that source, decoupled so the
//! solver never has to know about the underlying scan format.
//! What: re-exports the descriptor and codec types.
//! Test: child modules own all tests.

pub mod codec;
pub mod descriptor;

pub use codec::KohinoorCodec;
pub use descriptor::{lift_kohinoor_descriptor, KohinoorDescriptor};
