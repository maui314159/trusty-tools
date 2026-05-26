//! Andromedan cipher subsystem.
//!
//! Why: telemetry leaving the observatory is encrypted by an Andromedan
//! cipher before being forwarded to remote consumers. Isolating the cipher
//! and codec here keeps the rest of the pipeline plaintext-only.
//! What: re-exports the cipher type and codec function.
//! Test: child modules own all tests.

pub mod cipher;
pub mod codec;

pub use cipher::{thread_andromedan_cipher, AndromedanCipher};
pub use codec::andromedan_codec;
