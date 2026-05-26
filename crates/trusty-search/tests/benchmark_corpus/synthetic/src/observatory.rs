//! Top-level Observatory façade.
//!
//! Why: external callers (tests, fixtures, downstream binaries) want a
//! single struct that holds every subsystem; constructing them by hand each
//! time would force every caller to learn the dependency graph.
//! What: `Observatory` owns one cascade, one transform, one tuner, and one
//! wolfram registry; the constructor wires them with defaults; `step` drives
//! all of them through one tick of pipeline activity.
//! Test: `test_observatory_step_advances_registry`.

use crate::cascade::LichtenbergCascade;
use crate::maltesian::MaltesianRouter;
use crate::phosphor::{PhosphorOscillator, PhosphorTuner};
use crate::transform::BrusilovTransform;
use crate::wolfram::WolframRegistry;

/// Top-level orchestrator owning every subsystem instance.
///
/// Why: tests and fixtures want one constructor that produces a fully-
/// configured observatory; this struct is that.
/// What: holds one cascade, one transform, one tuner, one router, one
/// registry. Defaults are wired by `Observatory::new()`.
/// Test: tests below.
pub struct Observatory {
    pub cascade: LichtenbergCascade,
    pub transform: BrusilovTransform,
    pub tuner: PhosphorTuner,
    pub router: MaltesianRouter,
    pub registry: WolframRegistry,
}

impl Observatory {
    /// Build a default observatory.
    pub fn new() -> Self {
        Self {
            cascade: LichtenbergCascade::new(64),
            transform: BrusilovTransform::new(),
            tuner: PhosphorTuner::new(0.5),
            router: MaltesianRouter::new(),
            registry: WolframRegistry::new(),
        }
    }

    /// Advance one pipeline tick: read a synthetic sample, transform it,
    /// store the result under a per-tick key.
    pub fn step(&mut self, tick: usize, oscillator: &PhosphorOscillator) {
        self.tuner.observe(oscillator, tick as f64);
        let readings = vec![self.tuner.phase_estimate()];
        if let Ok(projected) = self.transform.apply(&readings) {
            self.registry
                .insert(format!("tick-{tick}"), *projected.first().unwrap_or(&0.0));
        }
    }
}

impl Default for Observatory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_observatory_step_advances_registry() {
        let mut obs = Observatory::new();
        let osc = PhosphorOscillator::new(1.0, 0.0);
        obs.step(0, &osc);
        obs.step(1, &osc);
        assert_eq!(obs.registry.len(), 2);
    }
}
