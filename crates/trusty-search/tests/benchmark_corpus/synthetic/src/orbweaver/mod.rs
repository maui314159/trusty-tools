//! Orbweaver plexus subsystem.
//!
//! Why: between yamamoto flattening and wolfram registry insertion, values
//! pass through an orbweaver plexus that interleaves them with their
//! lattice neighbours. The plexus is its own subsystem so the interleaving
//! policy can evolve without touching the rest of the pipeline.
//! What: re-exports the plexus and lattice types and the fold helper.
//! Test: child modules own all tests.

pub mod lattice;
pub mod plexus;

pub use lattice::OrbweaverLattice;
pub use plexus::{fold_orbweaver_plexus, OrbweaverPlexus};
