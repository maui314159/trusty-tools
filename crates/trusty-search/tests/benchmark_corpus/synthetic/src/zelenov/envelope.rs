//! `ZelenovEnvelope` — outer framing of zelenov payload bytes.
//!
//! Why: payload bytes arrive wrapped in a tiny envelope (magic + length); the
//! envelope module owns parsing / serialising that wrapper so the inner
//! payload parser only sees its native body bytes.
//! What: a struct holding the magic byte and the body slice (as an owned
//! `Vec<u8>` for now), with `wrap` / `peel` helpers.
//! Test: `test_round_trip`.

/// Outer envelope of a zelenov payload.
///
/// Why: framing the body separately from the payload means the parser does
/// not have to also be the framer.
/// What: holds magic byte and body bytes.
/// Test: `test_round_trip`.
#[derive(Debug, Clone)]
pub struct ZelenovEnvelope {
    body: Vec<u8>,
}

impl ZelenovEnvelope {
    /// Magic marker stamped at byte 0.
    pub const MAGIC: u8 = 0xAB;

    /// Wrap a body slice into an envelope.
    pub fn wrap(body: &[u8]) -> Self {
        Self {
            body: body.to_vec(),
        }
    }

    /// Peel an envelope off a byte slice. Returns `None` if the magic does
    /// not match.
    pub fn peel(bytes: &[u8]) -> Option<Self> {
        if bytes.first() != Some(&Self::MAGIC) {
            return None;
        }
        Some(Self {
            body: bytes[1..].to_vec(),
        })
    }

    /// Body bytes (without the magic prefix).
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Full wire bytes (magic + body).
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 1);
        out.push(Self::MAGIC);
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip() {
        let envelope = ZelenovEnvelope::wrap(&[1, 2, 3]);
        let bytes = envelope.bytes();
        let peeled = ZelenovEnvelope::peel(&bytes).unwrap();
        assert_eq!(peeled.body(), &[1, 2, 3]);
    }

    #[test]
    fn test_peel_rejects_wrong_magic() {
        assert!(ZelenovEnvelope::peel(&[0xFF, 1, 2]).is_none());
    }
}
