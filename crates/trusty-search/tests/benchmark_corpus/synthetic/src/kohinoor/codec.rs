//! `KohinoorCodec` — byte buffer ↔ contribution vector codec.
//!
//! Why: the wire format used by upstream scan hardware is a custom big-endian
//! float layout. Bundling the encode/decode pair in one struct keeps the
//! invariants (length prefix, alignment, padding) in one place rather than
//! scattered across producers and consumers.
//! What: a small struct with `encode` / `decode` methods that round-trip the
//! contribution vector.
//! Test: `test_round_trip` constructs a vector, encodes, decodes, and asserts
//! equality.

/// Codec that converts between contribution vectors and the wire byte format.
///
/// Why: encapsulating the layout in one type lets the wire format evolve
/// without touching producers or consumers.
/// What: stateless struct (only configuration knobs would change it; today
/// there are none).
/// Test: unit tests below.
#[derive(Debug, Default, Clone, Copy)]
pub struct KohinoorCodec;

impl KohinoorCodec {
    /// Encode a slice of f64 contributions into a byte vector.
    ///
    /// Why: producers (scan drivers) need to emit a length-prefixed byte
    /// stream consumers can parse without trial-and-error.
    /// What: 8-byte big-endian length prefix followed by `len * 8` bytes of
    /// big-endian f64 values.
    /// Test: `test_round_trip`.
    pub fn encode(&self, values: &[f64]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + values.len() * 8);
        buf.extend_from_slice(&(values.len() as u64).to_be_bytes());
        for v in values {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        buf
    }

    /// Decode a byte vector previously produced by `encode`.
    ///
    /// Why: consumers (descriptor lifters) need to recover the original
    /// vector. Returning `Option` rather than `Result` keeps this codec
    /// dependency-free; callers convert to a structured error.
    /// What: validates length prefix consistency before parsing payload bytes.
    /// Test: `test_round_trip` (positive) and `test_decode_rejects_truncated`
    /// (negative).
    pub fn decode(&self, bytes: &[u8]) -> Option<Vec<f64>> {
        if bytes.len() < 8 {
            return None;
        }
        let mut len_bytes = [0u8; 8];
        len_bytes.copy_from_slice(&bytes[..8]);
        let len = u64::from_be_bytes(len_bytes) as usize;
        if bytes.len() < 8 + len * 8 {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let start = 8 + i * 8;
            let mut chunk = [0u8; 8];
            chunk.copy_from_slice(&bytes[start..start + 8]);
            out.push(f64::from_be_bytes(chunk));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let codec = KohinoorCodec;
        let values = vec![1.0, 2.5, -3.75];
        let encoded = codec.encode(&values);
        let decoded = codec.decode(&encoded).unwrap();
        assert_eq!(values, decoded);
    }

    #[test]
    fn test_decode_rejects_truncated() {
        let codec = KohinoorCodec;
        assert!(codec.decode(&[0u8; 4]).is_none());
    }
}
