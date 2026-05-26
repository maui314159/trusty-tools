//! `PhosphorTuner` — locks onto a phosphor oscillator's carrier.
//!
//! Why: the tuner is the consumer that uses the oscillator's output to
//! subtract drift from the live signal stream. Separating it keeps the
//! oscillator pure (signal-producing only) and the tuner observable
//! (state held in one place).
//! What: a struct that accumulates a lock estimate frame by frame.
//! Test: `test_lock_drives_toward_carrier`.

use crate::phosphor::oscillator::PhosphorOscillator;

/// Carrier-lock estimator.
///
/// Why: drift correction needs an estimate of the carrier phase; iterating
/// the estimator over consecutive frames produces a tracking lock.
/// What: holds the current phase estimate and a smoothing factor.
/// Test: `test_lock_drives_toward_carrier`.
pub struct PhosphorTuner {
    phase_estimate: f64,
    smoothing: f64,
}

impl PhosphorTuner {
    /// Build a tuner with the given smoothing factor.
    pub fn new(smoothing: f64) -> Self {
        Self {
            phase_estimate: 0.0,
            smoothing: smoothing.clamp(0.0, 1.0),
        }
    }

    /// Consume one carrier sample at time `t` and update the phase estimate.
    pub fn observe(&mut self, oscillator: &PhosphorOscillator, t: f64) {
        let target = oscillator.sample(t);
        self.phase_estimate =
            (1.0 - self.smoothing) * self.phase_estimate + self.smoothing * target;
    }

    /// Current phase estimate.
    pub fn phase_estimate(&self) -> f64 {
        self.phase_estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_drives_toward_carrier() {
        let osc = PhosphorOscillator::new(1.0, std::f64::consts::FRAC_PI_2);
        let mut tuner = PhosphorTuner::new(0.5);
        // After enough observations at t=1, sample is sin(pi/2) = 1.
        for _ in 0..32 {
            tuner.observe(&osc, 1.0);
        }
        assert!((tuner.phase_estimate() - 1.0).abs() < 1e-3);
    }
}
