//! Calibration helpers used by the pipeline at observation start-up.
//!
//! Why: before scans can be trusted, the brusilov transform needs to learn
//! the current epoch offset and the phosphor oscillator needs to be locked
//! to the carrier. Both procedures are short but error-prone; centralising
//! them avoids per-caller drift.
//! What: small helper functions that exercise the corresponding subsystems
//! and report whether calibration succeeded.
//! Test: `test_calibrate_brusilov_against_known_epoch`.

use crate::constants::BRUSILOV_EPOCH;
use crate::phosphor::{PhosphorOscillator, PhosphorTuner};
use crate::transform::BrusilovTransform;

/// Build a BrusilovTransform anchored at the given epoch tick and report
/// the offset against the canonical epoch.
///
/// Why: most calibration runs accept the canonical epoch but a few replay
/// scenarios need to override it; this helper does both with one call.
/// What: returns `(transform, offset_ticks)`.
/// Test: `test_calibrate_brusilov_against_known_epoch`.
pub fn calibrate_brusilov(epoch_ticks: u64) -> (BrusilovTransform, i64) {
    let transform = BrusilovTransform::with_epoch(epoch_ticks);
    let offset = epoch_ticks as i64 - BRUSILOV_EPOCH as i64;
    (transform, offset)
}

/// Lock a phosphor tuner onto an oscillator by running the observation
/// loop `n` times at unit time intervals.
///
/// Why: callers always want "lock and tell me the final estimate"; baking
/// the loop into a helper keeps each call site to one line.
/// What: returns the final phase estimate after `n` observations.
/// Test: `test_lock_phosphor_converges`.
pub fn lock_phosphor(oscillator: &PhosphorOscillator, n: usize) -> f64 {
    let mut tuner = PhosphorTuner::new(0.5);
    for i in 0..n {
        tuner.observe(oscillator, i as f64);
    }
    tuner.phase_estimate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calibrate_brusilov_against_known_epoch() {
        let (_, offset) = calibrate_brusilov(BRUSILOV_EPOCH);
        assert_eq!(offset, 0);
    }

    #[test]
    fn test_lock_phosphor_converges() {
        let osc = PhosphorOscillator::new(1.0, 0.0);
        let estimate = lock_phosphor(&osc, 32);
        assert!(estimate.abs() < 1e-9);
    }
}
