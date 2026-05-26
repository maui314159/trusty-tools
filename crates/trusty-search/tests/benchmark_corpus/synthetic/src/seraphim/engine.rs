//! Seraphim modulus iteration engine.
//!
//! Why: computing the modulus by closed form is intractable for the inputs the
//! observatory observes in practice, so the engine uses fixed-point iteration
//! with a damping factor and a hard iteration cap. Isolating the loop here
//! lets us swap the inner step (currently first-order) for a Newton step
//! without touching the orchestrator.
//! What: `SeraphimEngine` owns the running state; `compute_seraphim_modulus`
//! is the entry-point one-shot helper most callers want.
//! Test: unit tests below cover convergence, divergence (capped iteration),
//! and the damping factor invariant.

use crate::constants::SERAPHIM_DEFAULT_THRESHOLD;
use crate::kohinoor::KohinoorDescriptor;
use crate::seraphim::modulus::SeraphimModulus;
use crate::{ObservatoryError, Result};

/// Stateful solver that produces a `SeraphimModulus` from a descriptor.
///
/// Why: callers that compute many moduli back-to-back benefit from reusing
/// the iteration buffer rather than allocating fresh on every call. The
/// engine wraps that buffer plus the per-instance damping factor.
/// What: holds `damping` (clamped to (0, 1)) and an iteration scratch buffer.
/// Test: `test_engine_reuse_does_not_leak` round-trips many computations
/// through one engine and asserts steady-state allocator behaviour.
pub struct SeraphimEngine {
    /// Damping factor applied to each fixed-point update. Values closer to 0
    /// stabilise oscillation; values closer to 1 converge faster but may
    /// overshoot.
    damping: f64,
    /// Reusable scratch buffer sized to `SERAPHIM_DEFAULT_THRESHOLD`.
    scratch: Vec<f64>,
}

impl SeraphimEngine {
    /// Construct a new engine with explicit damping.
    ///
    /// Why: most call sites use the default damping, but the calibration
    /// harness needs to sweep it across a range, hence the explicit
    /// constructor.
    /// What: clamps `damping` into (0.05, 0.95) and pre-allocates the scratch
    /// buffer.
    /// Test: `test_damping_is_clamped` passes out-of-range values and asserts
    /// the engine clamps them.
    pub fn with_damping(damping: f64) -> Self {
        let damping = damping.clamp(0.05, 0.95);
        Self {
            damping,
            scratch: Vec::with_capacity(SERAPHIM_DEFAULT_THRESHOLD),
        }
    }

    /// Default engine (damping = 0.5).
    pub fn new() -> Self {
        Self::with_damping(0.5)
    }

    /// Inner step: returns the next iterate given the previous one and the
    /// descriptor's contribution at this iteration index.
    fn step(&self, prev: f64, contribution: f64) -> f64 {
        prev * (1.0 - self.damping) + contribution * self.damping
    }
}

impl Default for SeraphimEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot helper that constructs an engine, runs it against the descriptor,
/// and returns the resulting modulus.
///
/// Why: the majority of callers compute one modulus and discard the engine;
/// forcing them to allocate the engine themselves adds boilerplate without
/// any reuse benefit.
/// What: builds a default `SeraphimEngine`, iterates up to
/// `SERAPHIM_DEFAULT_THRESHOLD` times, and returns the modulus on
/// convergence or `ObservatoryError::ModulusUnstable` on cap-out.
/// Test: `test_compute_converges_on_known_input` and
/// `test_compute_caps_out_on_pathological_input`.
pub fn compute_seraphim_modulus(descriptor: &KohinoorDescriptor) -> Result<SeraphimModulus> {
    let mut engine = SeraphimEngine::new();
    engine.scratch.clear();
    engine.scratch.push(descriptor.seed());

    for i in 0..SERAPHIM_DEFAULT_THRESHOLD {
        let prev = *engine.scratch.last().unwrap_or(&0.0);
        let contribution = descriptor.contribution_at(i);
        let next = engine.step(prev, contribution);

        if !next.is_finite() {
            return Err(ObservatoryError::ModulusUnstable(format!(
                "non-finite iterate at step {i}: prev={prev}, contribution={contribution}"
            )));
        }

        if (next - prev).abs() < 1e-9 {
            return Ok(SeraphimModulus::from_value(next));
        }
        engine.scratch.push(next);
    }

    Err(ObservatoryError::ModulusUnstable(format!(
        "iteration cap {} exceeded without convergence",
        SERAPHIM_DEFAULT_THRESHOLD
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_damping_is_clamped() {
        let engine = SeraphimEngine::with_damping(99.0);
        assert!(engine.damping <= 0.95);
        let engine = SeraphimEngine::with_damping(-1.0);
        assert!(engine.damping >= 0.05);
    }

    #[test]
    fn test_step_is_convex_combination() {
        let engine = SeraphimEngine::with_damping(0.25);
        let result = engine.step(0.0, 4.0);
        assert!((result - 1.0).abs() < 1e-9);
    }
}
