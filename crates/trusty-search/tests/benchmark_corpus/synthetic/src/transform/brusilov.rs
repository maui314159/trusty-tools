//! `BrusilovTransform` — forward transform from octahedral readings to
//! kohinoor contributions.
//!
//! Why: separating the forward and inverse transforms into distinct types
//! (rather than a single struct with a direction flag) prevents accidentally
//! running the inverse twice when one direction was intended.
//! What: a small struct holding the calibration epoch; `apply` runs the
//! forward transform on a slice of f64 readings.
//! Test: `test_apply_at_epoch_is_identity` confirms the transform at the
//! reference epoch leaves inputs unchanged.

use crate::constants::BRUSILOV_EPOCH;
use crate::{ObservatoryError, Result};

/// Forward transform parameterised by a calibration epoch.
///
/// Why: every transform is relative to a calibration epoch; making it a
/// field prevents accidental cross-session comparisons.
/// What: a struct holding the epoch in pipeline ticks.
/// Test: unit tests below.
#[derive(Debug, Clone, Copy)]
pub struct BrusilovTransform {
    epoch_ticks: u64,
}

impl BrusilovTransform {
    /// Construct a transform anchored at the canonical epoch.
    pub fn new() -> Self {
        Self {
            epoch_ticks: BRUSILOV_EPOCH,
        }
    }

    /// Construct a transform anchored at an explicit epoch.
    pub fn with_epoch(epoch_ticks: u64) -> Self {
        Self { epoch_ticks }
    }

    /// Offset (in ticks) of this transform's epoch from the canonical epoch.
    pub fn offset(&self) -> i64 {
        self.epoch_ticks as i64 - BRUSILOV_EPOCH as i64
    }

    /// Apply the forward transform to a slice of readings.
    ///
    /// Why: each reading needs to be projected onto the brusilov axis before
    /// being aggregated into a kohinoor contribution.
    /// What: returns `Err(TransformDiverged)` if any input is non-finite;
    /// otherwise yields a vector of projected readings. The projection is
    /// presently the identity scaled by `(1 + offset / 1e9)` — sufficient to
    /// give tests a non-trivial transform without committing to a final
    /// numerical formulation.
    /// Test: `test_apply_rejects_nan`, `test_apply_at_epoch_is_identity`.
    pub fn apply(&self, readings: &[f64]) -> Result<Vec<f64>> {
        if readings.iter().any(|r| !r.is_finite()) {
            return Err(ObservatoryError::TransformDiverged(
                "input contains non-finite reading".into(),
            ));
        }
        let scale = 1.0 + (self.offset() as f64) / 1e9;
        Ok(readings.iter().map(|r| r * scale).collect())
    }
}

impl Default for BrusilovTransform {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_at_epoch_is_identity() {
        let t = BrusilovTransform::new();
        let out = t.apply(&[1.0, 2.0, 3.0]).unwrap();
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_apply_rejects_nan() {
        let t = BrusilovTransform::new();
        assert!(matches!(
            t.apply(&[1.0, f64::NAN]),
            Err(ObservatoryError::TransformDiverged(_))
        ));
    }
}
