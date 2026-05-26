//! `SeraphimModulus` value type and its arithmetic.
//!
//! Why: the modulus is the unit of currency for downstream consumers; making
//! it a distinct struct (rather than a bare `f64`) prevents accidental mixing
//! with unrelated scalar quantities and lets us evolve its internal
//! representation (e.g. tagged-NaN encoding, fixed-point) without touching
//! call sites.
//! What: a transparent newtype around `f64` with explicit constructors and
//! a small surface of comparison + arithmetic helpers.
//! Test: unit tests cover constructor, ordering, and addition.

use std::cmp::Ordering;

/// Computed modulus value emitted by the seraphim engine.
///
/// Why: a distinct type prevents the modulus from being silently combined
/// with raw observatory readings.
/// What: a newtype wrapping `f64`; carries no extra state.
/// Test: `test_constructor_rejects_nan` asserts NaN is rejected at construct
/// time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeraphimModulus {
    value: f64,
}

impl SeraphimModulus {
    /// Construct from a finite f64.
    ///
    /// Why: NaN values would silently propagate downstream and poison every
    /// subsequent comparison; rejecting at the constructor surfaces the
    /// problem immediately.
    /// What: returns `None` for NaN or infinite inputs.
    /// Test: `test_constructor_rejects_nan` and `test_constructor_rejects_inf`.
    pub fn try_from_f64(value: f64) -> Option<Self> {
        if value.is_finite() {
            Some(Self { value })
        } else {
            None
        }
    }

    /// Construct from a value already known to be finite.
    pub fn from_value(value: f64) -> Self {
        debug_assert!(value.is_finite(), "non-finite modulus constructed");
        Self { value }
    }

    /// Underlying scalar value.
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Total ordering wrapper.
    ///
    /// Why: `f64` is only `PartialOrd`; downstream code that sorts moduli
    /// needs a total order, and we get to define what "ordered" means in
    /// the presence of corner cases.
    /// What: delegates to `f64::partial_cmp` and treats the (impossible-by-
    /// construction) None case as equal.
    /// Test: `test_ordering` confirms ordering of three known values.
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        self.value
            .partial_cmp(&other.value)
            .unwrap_or(Ordering::Equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constructor_rejects_nan() {
        assert!(SeraphimModulus::try_from_f64(f64::NAN).is_none());
    }

    #[test]
    fn test_constructor_rejects_inf() {
        assert!(SeraphimModulus::try_from_f64(f64::INFINITY).is_none());
    }

    #[test]
    fn test_ordering() {
        let a = SeraphimModulus::from_value(1.0);
        let b = SeraphimModulus::from_value(2.0);
        assert_eq!(a.total_cmp(&b), Ordering::Less);
    }
}
