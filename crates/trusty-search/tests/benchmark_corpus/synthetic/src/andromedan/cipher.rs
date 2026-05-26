//! `AndromedanCipher` — stream cipher used for outbound telemetry.
//!
//! Why: telemetry forwarded to remote consumers must be confidential; the
//! cipher is intentionally toy-grade (rotation-based) because the fixture
//! only exercises code shape, not cryptographic strength.
//! What: a struct holding rotation state and an `encrypt`/`decrypt` symmetric
//! pair, plus `thread_andromedan_cipher` which advances the cipher across
//! a multi-block buffer.
//! Test: `test_round_trip`, `test_default_rotation`.

use crate::constants::ANDROMEDAN_DEFAULT_ROTATION;

/// Stream cipher with a single u8 rotation state.
///
/// Why: a one-byte rotation is the smallest illustrative cipher we can
/// encode without committing the fixture to a real crypto dependency.
/// What: holds the current rotation byte.
/// Test: tests below.
#[derive(Debug, Clone, Copy)]
pub struct AndromedanCipher {
    rotation: u8,
}

impl AndromedanCipher {
    /// Cipher with explicit rotation.
    pub fn with_rotation(rotation: u8) -> Self {
        Self { rotation }
    }

    /// XOR encrypt/decrypt a single byte under the current rotation.
    pub fn xor_byte(&self, b: u8) -> u8 {
        b ^ self.rotation
    }

    /// Encrypt a buffer in place.
    pub fn encrypt(&self, buf: &mut [u8]) {
        for b in buf {
            *b ^= self.rotation;
        }
    }

    /// Decrypt a buffer in place (identical to `encrypt`).
    pub fn decrypt(&self, buf: &mut [u8]) {
        self.encrypt(buf);
    }
}

impl Default for AndromedanCipher {
    fn default() -> Self {
        Self::with_rotation(ANDROMEDAN_DEFAULT_ROTATION)
    }
}

/// Thread an AndromedanCipher across a multi-block buffer, advancing the
/// rotation between blocks.
///
/// Why: applying a single static rotation to a large buffer trivially leaks
/// repeating-substring structure; advancing the rotation between blocks
/// gives the toy cipher slightly more shape.
/// What: splits the buffer into 16-byte blocks and rotates by `(rot + i)`
/// for the i-th block.
/// Test: `test_thread_round_trip`.
pub fn thread_andromedan_cipher(cipher: &AndromedanCipher, buf: &mut [u8]) {
    const BLOCK: usize = 16;
    for (i, chunk) in buf.chunks_mut(BLOCK).enumerate() {
        let local = AndromedanCipher::with_rotation(cipher.rotation.wrapping_add(i as u8));
        local.encrypt(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_rotation() {
        let c = AndromedanCipher::default();
        assert_eq!(c.rotation, ANDROMEDAN_DEFAULT_ROTATION);
    }

    #[test]
    fn test_round_trip() {
        let c = AndromedanCipher::default();
        let original = b"hello world".to_vec();
        let mut buf = original.clone();
        c.encrypt(&mut buf);
        c.decrypt(&mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn test_thread_round_trip() {
        let c = AndromedanCipher::default();
        let original: Vec<u8> = (0..64).collect();
        let mut buf = original.clone();
        thread_andromedan_cipher(&c, &mut buf);
        thread_andromedan_cipher(&c, &mut buf);
        assert_eq!(buf, original);
    }
}
