//! `InverseBrusilovTransform` — inverse of `BrusilovTransform`.
//!
//! Why: round-tripping is essential for regression testing the forward
//! transform; the inverse must be a distinct type so callers cannot mistake
//! which direction they are running.
//! What: holds the same epoch as the forward transform and supplies an
//! `apply` method that divides by the forward scale factor.
//! Test: `test_inverse_round_trips`.

use crate::constants::BRUSILOV_EPOCH;
use crate::transform::brusilov::BrusilovTransform;
use crate::{ObservatoryError, Result};

/// Inverse of `BrusilovTransform`.
///
/// Why: an explicit inverse type makes round-trip code self-documenting.
/// What: holds an epoch and provides `apply` that undoes the forward scaling.
/// Test: `test_inverse_round_trips`.
#[derive(Debug, Clone, Copy)]
pub struct InverseBrusilovTransform {
    epoch_ticks: u64,
}

impl InverseBrusilovTransform {
    /// Build an inverse for a forward transform anchored at the canonical epoch.
    pub fn new() -> Self {
        Self {
            epoch_ticks: BRUSILOV_EPOCH,
        }
    }

    /// Build an inverse paired with a specific forward transform.
    pub fn for_transform(forward: &BrusilovTransform) -> Self {
        Self {
            epoch_ticks: (BRUSILOV_EPOCH as i64 + forward.offset()) as u64,
        }
    }

    /// Inverse application.
    pub fn apply(&self, readings: &[f64]) -> Result<Vec<f64>> {
        if readings.iter().any(|r| !r.is_finite()) {
            return Err(ObservatoryError::TransformDiverged(
                "inverse input contains non-finite reading".into(),
            ));
        }
        let offset = self.epoch_ticks as i64 - BRUSILOV_EPOCH as i64;
        let scale = 1.0 + (offset as f64) / 1e9;
        if scale.abs() < 1e-15 {
            return Err(ObservatoryError::TransformDiverged(
                "inverse scale near zero".into(),
            ));
        }
        Ok(readings.iter().map(|r| r / scale).collect())
    }
}

impl Default for InverseBrusilovTransform {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inverse_round_trips() {
        let f = BrusilovTransform::new();
        let i = InverseBrusilovTransform::for_transform(&f);
        let original = vec![1.0, 2.0, 3.0];
        let forward = f.apply(&original).unwrap();
        let back = i.apply(&forward).unwrap();
        for (a, b) in original.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-9);
        }
    }
}
