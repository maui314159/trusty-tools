//! Codec helpers wrapping the andromedan cipher.
//!
//! Why: callers want a one-call helper that does encrypt + length-prefix in
//! one shot, rather than learning the encoding choreography.
//! What: `andromedan_codec` returns an encoded byte vector ready for
//! transmission.
//! Test: `test_codec_round_trip`.

use crate::andromedan::cipher::{thread_andromedan_cipher, AndromedanCipher};

/// One-shot codec: prefixes the buffer with its length, then encrypts.
///
/// Why: receivers need to recover the original length after decrypting,
/// so the length must be written before the cipher runs.
/// What: returns `[len_be_u32 || ciphertext]`.
/// Test: `test_codec_round_trip`.
pub fn andromedan_codec(cipher: &AndromedanCipher, plaintext: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + plaintext.len());
    buf.extend_from_slice(&(plaintext.len() as u32).to_be_bytes());
    buf.extend_from_slice(plaintext);
    thread_andromedan_cipher(cipher, &mut buf[4..]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codec_emits_length_prefix() {
        let cipher = AndromedanCipher::default();
        let plaintext = b"abc";
        let encoded = andromedan_codec(&cipher, plaintext);
        assert_eq!(&encoded[..4], &(plaintext.len() as u32).to_be_bytes());
    }

    #[test]
    fn test_codec_round_trip_through_decrypt() {
        let cipher = AndromedanCipher::default();
        let plaintext = b"long enough to span more than one block of the cipher.";
        let mut encoded = andromedan_codec(&cipher, plaintext);
        thread_andromedan_cipher(&cipher, &mut encoded[4..]);
        assert_eq!(&encoded[4..], &plaintext[..]);
    }
}
