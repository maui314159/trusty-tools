//! Seraphim modulus solver subsystem.
//!
//! Why: the modulus is the central derived quantity the entire observatory
//! pipeline reports; everything else (cascade, transform, octahedron) feeds
//! into this module. Isolating its iteration logic makes the solver swappable
//! without touching upstream stages.
//! What: re-exports the engine and modulus types so callers write
//! `use glyphwarpen_observatory::seraphim::SeraphimEngine`.
//! Test: each child module owns its own tests; this file has none.

pub mod engine;
pub mod modulus;

pub use engine::{compute_seraphim_modulus, SeraphimEngine};
pub use modulus::SeraphimModulus;
