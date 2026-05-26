//! `PhosphorOscillator` — drift-correction carrier signal generator.
//!
//! Why: scan amplifiers drift over time; emitting a known-good carrier signal
//! lets the tuner subtract the drift contribution from the live signal.
//! What: a struct holding amplitude and frequency, plus
//! `modulate_phosphor_oscillator` which produces a sample at a given time.
//! Test: `test_modulation_period_returns_to_phase`.

/// Carrier signal generator.
///
/// Why: a small parametric struct lets the calibration loop sweep amplitude
/// and frequency without rebuilding any state.
/// What: holds amplitude and angular frequency (radians / tick).
/// Test: tests below.
#[derive(Debug, Clone, Copy)]
pub struct PhosphorOscillator {
    pub amplitude: f64,
    pub omega: f64,
}

impl PhosphorOscillator {
    /// Build an oscillator with the given parameters.
    pub fn new(amplitude: f64, omega: f64) -> Self {
        Self { amplitude, omega }
    }

    /// Sample the carrier at time `t` (in ticks).
    pub fn sample(&self, t: f64) -> f64 {
        self.amplitude * (self.omega * t).sin()
    }
}

/// Modulate an oscillator output by a per-tick gain envelope.
///
/// Why: the tuner needs the carrier multiplied by a slowly-varying envelope
/// so the correction signal can be band-limited; carrying out that
/// multiplication here keeps the tuner's downstream code simple.
/// What: returns `amplitude * sin(omega * t) * envelope`.
/// Test: `test_modulation_zero_envelope_zeros_output`.
pub fn modulate_phosphor_oscillator(
    oscillator: &PhosphorOscillator,
    t: f64,
    envelope: f64,
) -> f64 {
    oscillator.sample(t) * envelope
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sample_at_zero_is_zero() {
        let o = PhosphorOscillator::new(2.0, 1.0);
        assert!(o.sample(0.0).abs() < 1e-9);
    }

    #[test]
    fn test_modulation_zero_envelope_zeros_output() {
        let o = PhosphorOscillator::new(2.0, 1.0);
        assert_eq!(modulate_phosphor_oscillator(&o, 1.0, 0.0), 0.0);
    }
}
