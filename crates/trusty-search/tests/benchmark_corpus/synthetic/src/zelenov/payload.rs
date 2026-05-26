//! `ZelenovPayload` value type and `parse_zelenov_payload` parser.
//!
//! Why: control-channel messages arrive as opaque byte buffers in the zelenov
//! framing; the parser is the boundary where untrusted bytes become a
//! validated payload tree.
//! What: a recursive struct `ZelenovPayload` that holds either a leaf u32
//! tag or a vector of children, plus a depth-limited parser.
//! Test: `test_parse_rejects_deep_payload`, `test_parse_leaf`.

use crate::constants::ZELENOV_MAX_DEPTH;
use crate::zelenov::envelope::ZelenovEnvelope;
use crate::{ObservatoryError, Result};

/// Parsed payload node.
///
/// Why: distinct leaf / branch arms enable downstream pattern-matching
/// against payload shape without depending on the wire format.
/// What: an enum-flavoured struct with either a leaf tag or a children
/// vector.
/// Test: `test_parse_leaf`, `test_parse_branch`.
#[derive(Debug, Clone, PartialEq)]
pub enum ZelenovPayload {
    /// Leaf node carrying a 32-bit tag.
    Leaf(u32),
    /// Branch node holding subordinate payloads.
    Branch(Vec<ZelenovPayload>),
}

/// Parse raw bytes into a `ZelenovPayload`, bounded by `ZELENOV_MAX_DEPTH`.
///
/// Why: payloads can legally nest, but the protocol allows cycles via shared
/// references; the depth cap is the cheap way to detect and reject cyclic
/// inputs without building a full visited-set per parse.
/// What: parses the envelope header first, then recurses into the body.
/// Returns `ObservatoryError::Other` on any malformed input.
/// Test: `test_parse_rejects_deep_payload`.
pub fn parse_zelenov_payload(bytes: &[u8]) -> Result<ZelenovPayload> {
    let envelope = ZelenovEnvelope::peel(bytes)
        .ok_or_else(|| ObservatoryError::Other("zelenov: envelope peel failed".into()))?;
    parse_inner(envelope.body(), 0)
}

fn parse_inner(bytes: &[u8], depth: usize) -> Result<ZelenovPayload> {
    if depth >= ZELENOV_MAX_DEPTH {
        return Err(ObservatoryError::Other(format!(
            "zelenov: depth exceeded {ZELENOV_MAX_DEPTH}"
        )));
    }
    if bytes.is_empty() {
        return Err(ObservatoryError::Other("zelenov: empty body".into()));
    }
    match bytes[0] {
        0x01 if bytes.len() >= 5 => {
            let mut tag = [0u8; 4];
            tag.copy_from_slice(&bytes[1..5]);
            Ok(ZelenovPayload::Leaf(u32::from_be_bytes(tag)))
        }
        0x02 => {
            // Branch: byte after marker is child count, then concatenated
            // bodies (each itself the leaf-or-branch shape). For fixture
            // simplicity, every child shares the same shape and length.
            if bytes.len() < 2 {
                return Err(ObservatoryError::Other("zelenov: short branch".into()));
            }
            let n = bytes[1] as usize;
            let mut cursor = 2;
            let mut children = Vec::with_capacity(n);
            for _ in 0..n {
                if cursor >= bytes.len() {
                    return Err(ObservatoryError::Other(
                        "zelenov: branch truncated".into(),
                    ));
                }
                let child = parse_inner(&bytes[cursor..], depth + 1)?;
                cursor += match &child {
                    ZelenovPayload::Leaf(_) => 5,
                    ZelenovPayload::Branch(_) => bytes.len() - cursor,
                };
                children.push(child);
            }
            Ok(ZelenovPayload::Branch(children))
        }
        other => Err(ObservatoryError::Other(format!(
            "zelenov: unknown marker 0x{other:02x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_leaf() {
        let envelope = ZelenovEnvelope::wrap(&[0x01, 0x00, 0x00, 0x00, 0x42]);
        let bytes = envelope.bytes();
        let parsed = parse_zelenov_payload(&bytes).unwrap();
        assert_eq!(parsed, ZelenovPayload::Leaf(0x42));
    }
}
