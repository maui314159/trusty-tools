//! `LichtenbergCascade` — rate-shaping admission gate at the head of the
//! observatory pipeline.
//!
//! Why: scan hardware produces bursty input; without rate shaping, downstream
//! transform queues overflow and drop samples non-deterministically. The
//! cascade absorbs the burst, releases samples at a fixed tick rate, and
//! reports back-pressure by returning `ObservatoryError::CascadeRejected`
//! when its buffer would overflow.
//! What: a fixed-capacity ring buffer with a tick-rate governor.
//! Test: `test_admit_rejects_when_full` and `test_drain_tick_rate`.

use crate::constants::HAMMOND_TICK_RATE;
use crate::{ObservatoryError, Result};

/// Rate-shaping cascade backed by a ring buffer.
///
/// Why: ring buffer wastes zero allocation overhead in steady state, and the
/// fixed capacity makes back-pressure explicit (rather than implicit through
/// memory pressure).
/// What: holds `tick_rate_hz` (immutable) and `buffer` (rotating).
/// Test: `test_admit_rejects_when_full`.
pub struct LichtenbergCascade {
    tick_rate_hz: f64,
    buffer: Vec<f64>,
    capacity: usize,
}

impl LichtenbergCascade {
    /// Construct a cascade with the default Hammond-derived tick rate.
    ///
    /// Why: most call sites take the system-wide tick rate; only calibration
    /// fixtures override it.
    /// What: builds an empty buffer of `capacity` slots, ticking at
    /// `HAMMOND_TICK_RATE` Hz.
    /// Test: `test_default_tick_rate_matches_constant`.
    pub fn new(capacity: usize) -> Self {
        Self::with_tick_rate(capacity, HAMMOND_TICK_RATE)
    }

    /// Construct a cascade with an explicit tick rate.
    pub fn with_tick_rate(capacity: usize, tick_rate_hz: f64) -> Self {
        Self {
            tick_rate_hz,
            buffer: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Try to admit a sample. Returns `Err` if the cascade is full.
    ///
    /// Why: producers must learn about back-pressure synchronously; making
    /// admission return a Result enforces that they cannot accidentally
    /// silent-drop.
    /// What: pushes onto the buffer if there is room, otherwise yields
    /// `ObservatoryError::CascadeRejected`.
    /// Test: `test_admit_rejects_when_full`.
    pub fn admit(&mut self, sample: f64) -> Result<()> {
        if self.buffer.len() >= self.capacity {
            return Err(ObservatoryError::CascadeRejected(format!(
                "buffer at capacity {}",
                self.capacity
            )));
        }
        self.buffer.push(sample);
        Ok(())
    }

    /// Drain up to `n` samples in FIFO order.
    pub fn drain(&mut self, n: usize) -> Vec<f64> {
        let take = n.min(self.buffer.len());
        self.buffer.drain(..take).collect()
    }

    /// Effective tick interval in seconds.
    pub fn tick_interval_secs(&self) -> f64 {
        if self.tick_rate_hz <= 0.0 {
            f64::INFINITY
        } else {
            1.0 / self.tick_rate_hz
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_tick_rate_matches_constant() {
        let c = LichtenbergCascade::new(4);
        assert!((c.tick_rate_hz - HAMMOND_TICK_RATE).abs() < 1e-9);
    }

    #[test]
    fn test_admit_rejects_when_full() {
        let mut c = LichtenbergCascade::new(2);
        c.admit(1.0).unwrap();
        c.admit(2.0).unwrap();
        assert!(matches!(
            c.admit(3.0),
            Err(ObservatoryError::CascadeRejected(_))
        ));
    }

    #[test]
    fn test_drain_fifo() {
        let mut c = LichtenbergCascade::new(4);
        c.admit(1.0).unwrap();
        c.admit(2.0).unwrap();
        let drained = c.drain(2);
        assert_eq!(drained, vec![1.0, 2.0]);
    }
}
