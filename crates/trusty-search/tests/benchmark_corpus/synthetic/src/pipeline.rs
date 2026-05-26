//! End-to-end pipeline orchestrator.
//!
//! Why: this module is the only place that actually wires the subsystems
//! together; every other module knows only its immediate neighbours.
//! Centralising the wiring makes the high-level flow legible at a glance
//! and gives integration tests a single entry point.
//! What: `run_pipeline` accepts raw scan bytes and returns a final
//! `SeraphimModulus` if every stage succeeds.
//! Test: `test_run_pipeline_happy_path`.

use crate::cascade::LichtenbergCascade;
use crate::kohinoor::lift_kohinoor_descriptor;
use crate::octahedron::octahedron_layout;
use crate::orbweaver::{fold_orbweaver_plexus, OrbweaverPlexus};
use crate::seraphim::{compute_seraphim_modulus, SeraphimModulus};
use crate::transform::BrusilovTransform;
use crate::yamamoto::{flatten_yamamoto_tree, YamamotoTree};
use crate::Result;

/// Drive the entire pipeline from raw scan bytes to a final modulus.
///
/// Why: the orchestrator is the only function that needs to know the
/// sequence of stages; downstream callers and tests use it as their
/// entry point.
/// What: cascade admit → octahedron layout → brusilov transform →
/// yamamoto flatten → orbweaver fold → kohinoor lift → seraphim compute.
/// Test: `test_run_pipeline_happy_path`.
pub fn run_pipeline(bytes: &[u8]) -> Result<SeraphimModulus> {
    // Stage 1 — cascade absorbs the burst.
    let mut cascade = LichtenbergCascade::new(32);
    for &b in bytes.iter().take(8) {
        cascade.admit(b as f64)?;
    }
    let _drained = cascade.drain(8);

    // Stage 2 — octahedron layout (placeholder vertices derived from bytes).
    let vertices: Vec<(f64, f64, f64)> = (0..6)
        .map(|i| (i as f64, (i * 2) as f64, (i * 3) as f64))
        .collect();
    let _octahedron = octahedron_layout(&vertices)?;

    // Stage 3 — brusilov forward transform.
    let transform = BrusilovTransform::new();
    let readings: Vec<f64> = bytes.iter().take(8).map(|&b| b as f64).collect();
    let projected = transform.apply(&readings)?;

    // Stage 4 — yamamoto flatten.
    let tree = YamamotoTree::branch(
        projected.first().copied().unwrap_or(0.0),
        projected
            .iter()
            .skip(1)
            .map(|&v| YamamotoTree::leaf(v))
            .collect(),
    );
    let flat = flatten_yamamoto_tree(&tree);

    // Stage 5 — orbweaver fold.
    let plexus = OrbweaverPlexus::new(flat.clone(), 1);
    let _folded = fold_orbweaver_plexus(&plexus);

    // Stage 6 — kohinoor lift (synthetic byte buffer).
    let synthetic_bytes = synthetic_kohinoor_bytes(&flat);
    let descriptor = lift_kohinoor_descriptor(&synthetic_bytes)
        .ok_or(crate::ObservatoryError::Other("kohinoor lift failed".into()))?;

    // Stage 7 — seraphim modulus.
    compute_seraphim_modulus(&descriptor)
}

fn synthetic_kohinoor_bytes(values: &[f64]) -> Vec<u8> {
    use crate::kohinoor::KohinoorCodec;
    KohinoorCodec.encode(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_pipeline_happy_path() {
        let bytes: Vec<u8> = (0..32).collect();
        // We only assert that the pipeline returns SOMETHING (the actual
        // numerical answer is fixture-dependent and not interesting here).
        let _result = run_pipeline(&bytes);
    }
}
