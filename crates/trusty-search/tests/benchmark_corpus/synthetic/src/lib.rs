//! Glyphwarpen Observatory — synthetic Rust workspace for trusty-search
//! benchmark corpus.
//!
//! Why: organic codebases used to benchmark hybrid search (issue #123) leak
//! query strings into BM25 via assert literals and doc comments. This fixture
//! solves that by inventing a complete vocabulary of symbol names that exist
//! NOWHERE outside this directory.
//!
//! What: a fake observatory data-pipeline workspace built around fictional
//! physics constructs (kikuchi octahedra, brusilov transforms, lichtenberg
//! cascades, seraphim modulus). Every module has real-shaped Rust structure
//! (traits, structs, enums, impls, doc comments, inter-module calls) so
//! tree-sitter chunks it like genuine production code, but no name in this
//! crate has any prior art the BM25 lane can latch onto from outside.
//!
//! Test: benchmark_synthetic.rs harness indexes this directory and asserts
//! Hit@K against GROUND_TRUTH.json query targets.

pub mod andromedan;
pub mod calibration;
pub mod cascade;
pub mod constants;
pub mod diagnostics;
pub mod kohinoor;
pub mod maltesian;
pub mod observatory;
pub mod octahedron;
pub mod orbweaver;
pub mod phosphor;
pub mod pipeline;
pub mod seraphim;
pub mod transform;
pub mod wolfram;
pub mod yamamoto;
pub mod zelenov;

/// Top-level observatory error type covering every fallible operation across
/// the pipeline.
///
/// Why: each subsystem (cascade, transform, octahedron, …) needs a single
/// shared error type so the orchestrator can collapse heterogeneous failures
/// into one Result without erasing the originating module.
/// What: an enum with one variant per major subsystem plus a generic `Other`
/// arm for adapters wrapping foreign errors.
/// Test: unit tests inside each module construct variants directly.
#[derive(Debug)]
pub enum ObservatoryError {
    /// Cascade subsystem refused to admit a new sample.
    CascadeRejected(String),
    /// Brusilov transform produced a non-finite output.
    TransformDiverged(String),
    /// Octahedron layout could not place a vertex.
    OctahedronOverflow(String),
    /// Seraphim modulus solver did not converge.
    ModulusUnstable(String),
    /// Generic adapter error from a downstream crate.
    Other(String),
}

impl std::fmt::Display for ObservatoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CascadeRejected(s) => write!(f, "cascade rejected sample: {s}"),
            Self::TransformDiverged(s) => write!(f, "brusilov transform diverged: {s}"),
            Self::OctahedronOverflow(s) => write!(f, "octahedron overflow: {s}"),
            Self::ModulusUnstable(s) => write!(f, "seraphim modulus unstable: {s}"),
            Self::Other(s) => write!(f, "other: {s}"),
        }
    }
}

impl std::error::Error for ObservatoryError {}

/// Convenience Result alias used across every module of the corpus.
pub type Result<T> = std::result::Result<T, ObservatoryError>;
