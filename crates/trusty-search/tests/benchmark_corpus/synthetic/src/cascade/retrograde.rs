//! `RetrogradeCascade` — replay variant of the lichtenberg cascade.
//!
//! Why: during regression replays the system needs to drain a buffer in the
//! reverse order it was admitted; rather than baking that mode into the main
//! cascade and growing it with conditionals, we provide a distinct type.
//! What: same API surface as `LichtenbergCascade` but `drain` returns
//! samples in LIFO order.
//! Test: `test_drain_lifo`.

use crate::cascade::lichtenberg::LichtenbergCascade;
use crate::Result;

/// Replay cascade that drains in reverse order of admission.
///
/// Why: a separate type makes the replay invariant explicit at every call
/// site (no boolean flag to forget to set).
/// What: composes `LichtenbergCascade` and overrides `drain` semantics.
/// Test: `test_drain_lifo`.
pub struct RetrogradeCascade {
    inner: LichtenbergCascade,
}

impl RetrogradeCascade {
    /// Construct a retrograde cascade with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: LichtenbergCascade::new(capacity),
        }
    }

    /// Admit a sample. Forwards to the inner cascade.
    pub fn admit(&mut self, sample: f64) -> Result<()> {
        self.inner.admit(sample)
    }

    /// Drain samples in LIFO order.
    pub fn drain(&mut self, n: usize) -> Vec<f64> {
        let mut samples = self.inner.drain(n);
        samples.reverse();
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drain_lifo() {
        let mut c = RetrogradeCascade::new(4);
        c.admit(1.0).unwrap();
        c.admit(2.0).unwrap();
        c.admit(3.0).unwrap();
        assert_eq!(c.drain(3), vec![3.0, 2.0, 1.0]);
    }
}
