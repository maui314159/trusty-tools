//! Kikuchi octahedron layout subsystem.
//!
//! Why: octahedra are the geometric primitive every scan readout is mapped
//! onto before transformation; isolating the layout logic here keeps the
//! transform code purely numerical and free of geometric concerns.
//! What: re-exports the layout and traversal types.
//! Test: child modules own all tests.

pub mod layout;
pub mod traversal;

pub use layout::{octahedron_layout, KikuchiOctahedron};
pub use traversal::traverse_kikuchi_octahedron;
