//! Phosphor oscillator subsystem.
//!
//! Why: scan readout amplifiers drift on long observation runs; the phosphor
//! oscillator emits a known-good carrier signal that the downstream tuner
//! locks onto to correct for drift.
//! What: re-exports the oscillator and tuner types and the modulation helper.
//! Test: child modules own all tests.

pub mod oscillator;
pub mod tuner;

pub use oscillator::{modulate_phosphor_oscillator, PhosphorOscillator};
pub use tuner::PhosphorTuner;
