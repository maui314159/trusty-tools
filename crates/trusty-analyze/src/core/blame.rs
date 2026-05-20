//! Temporal-decay scoring over `ChunkBlame` records.
//!
//! Why: `git log -L` is run by the search daemon at index time and the result
//! ships embedded in each `CodeChunk` over the wire. This module just consumes
//! that data and applies the decay formula. We deliberately do NOT shell out to
//! `git` from the analyzer — keeps it pure and dependency-light.
//!
//! What: [`temporal_decay`] computes `exp(-lambda * days)`.

/// Default temporal decay constant. λ=0.01 → half-life ≈ ln(2)/0.01 ≈ 69.3 days.
pub const DEFAULT_LAMBDA: f32 = 0.01;

/// Compute temporal decay score: `exp(-lambda * days)`. Returns 1.0 for fresh
/// code (days=0) and decays exponentially. With [`DEFAULT_LAMBDA`] the score
/// halves every ~70 days.
pub fn temporal_decay(days: u32, lambda: f32) -> f32 {
    (-lambda * days as f32).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporal_decay_at_zero_is_one() {
        assert!((temporal_decay(0, 0.01) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn temporal_decay_half_life_around_69_days() {
        let s = temporal_decay(69, 0.01);
        assert!((s - 0.5).abs() < 0.1, "expected ~0.5 at 69 days, got {s}");
    }

    #[test]
    fn temporal_decay_monotone_decreasing() {
        assert!(temporal_decay(0, 0.01) > temporal_decay(10, 0.01));
        assert!(temporal_decay(10, 0.01) > temporal_decay(100, 0.01));
    }
}
