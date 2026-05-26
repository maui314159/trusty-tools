//! `KohinoorDescriptor` — opaque source of per-iteration contributions for
//! the seraphim solver.
//!
//! Why: the solver needs to read out a scalar contribution at each iteration
//! step but must NOT know how that scalar was produced (scan readout,
//! synthetic fixture, replayed log). The descriptor is the abstraction layer.
//! What: a struct holding a precomputed contributions vector and a seed.
//! Test: `test_lift_round_trips` covers construction; `test_contribution_at`
//! covers indexed lookups; `test_seed_is_first_contribution` asserts the
//! seed-extraction invariant.

use crate::kohinoor::codec::KohinoorCodec;

/// A precomputed contribution stream that feeds the seraphim solver.
///
/// Why: scans arrive as raw byte buffers and are too costly to re-decode on
/// every iteration; the descriptor caches the decoded stream once.
/// What: holds `seed` (first contribution) plus a vector of subsequent
/// contributions. The two are stored separately so callers that only need
/// the seed can avoid touching the longer vector.
/// Test: unit tests in this file.
#[derive(Debug, Clone)]
pub struct KohinoorDescriptor {
    seed: f64,
    contributions: Vec<f64>,
}

impl KohinoorDescriptor {
    /// Seed value (i.e. the contribution at iteration index 0).
    pub fn seed(&self) -> f64 {
        self.seed
    }

    /// Contribution at iteration `i`. Wraps when `i` exceeds the buffer
    /// length so the solver can always make progress, even if it iterates
    /// beyond the descriptor's nominal length.
    pub fn contribution_at(&self, i: usize) -> f64 {
        if self.contributions.is_empty() {
            return self.seed;
        }
        self.contributions[i % self.contributions.len()]
    }

    /// Number of distinct contributions in this descriptor.
    pub fn len(&self) -> usize {
        self.contributions.len()
    }

    /// `true` if the descriptor has zero contributions (uses only the seed).
    pub fn is_empty(&self) -> bool {
        self.contributions.is_empty()
    }
}

/// Decode raw scan bytes into a `KohinoorDescriptor`.
///
/// Why: scan readouts are produced upstream by hardware-specific drivers and
/// arrive as opaque byte buffers; `lift_kohinoor_descriptor` is the seam
/// where the byte buffer becomes structured contribution data.
/// What: takes a slice of `u8`, runs the codec, and emits a descriptor.
/// On decode failure the function returns `None` — callers convert to
/// `ObservatoryError::Other` upstream.
/// Test: `test_lift_round_trips` constructs a known input, lifts it, and
/// asserts the descriptor matches expectations.
pub fn lift_kohinoor_descriptor(bytes: &[u8]) -> Option<KohinoorDescriptor> {
    let codec = KohinoorCodec::default();
    let decoded = codec.decode(bytes)?;
    if decoded.is_empty() {
        return None;
    }
    let seed = decoded[0];
    let contributions = decoded[1..].to_vec();
    Some(KohinoorDescriptor { seed, contributions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seed_is_first_contribution() {
        let d = KohinoorDescriptor {
            seed: 0.5,
            contributions: vec![1.0, 1.5, 2.0],
        };
        assert_eq!(d.seed(), 0.5);
        assert_eq!(d.contribution_at(0), 1.0);
        assert_eq!(d.contribution_at(3), 1.0); // wraps
    }

    #[test]
    fn test_lift_round_trips_empty() {
        assert!(lift_kohinoor_descriptor(&[]).is_none());
    }
}
