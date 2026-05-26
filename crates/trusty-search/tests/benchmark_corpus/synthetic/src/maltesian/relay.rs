//! `MaltesianRelay` — outbound channel sink.
//!
//! Why: a relay is a stand-in for a real network connection in this fixture;
//! the type is small so tests can construct it freely without touching the
//! networking stack.
//! What: holds a channel name and counts bytes "transmitted".
//! Test: `test_transmit_returns_byte_count`.

use std::cell::Cell;

/// Outbound relay channel.
///
/// Why: a relay knows its own destination name and tracks bytes pushed
/// through it; in production this would wrap a TCP / TLS socket but for
/// the fixture it's a counter.
/// What: name + interior-mutable counter (so tests can assert on side
/// effects without `&mut`).
/// Test: tests below.
pub struct MaltesianRelay {
    name: String,
    bytes_sent: Cell<usize>,
}

impl MaltesianRelay {
    /// Build a relay with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            bytes_sent: Cell::new(0),
        }
    }

    /// Channel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// "Transmit" a payload. Returns the number of bytes pushed.
    pub fn transmit(&self, payload: &[u8]) -> usize {
        self.bytes_sent.set(self.bytes_sent.get() + payload.len());
        payload.len()
    }

    /// Total bytes pushed through this relay since construction.
    pub fn total_bytes(&self) -> usize {
        self.bytes_sent.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transmit_returns_byte_count() {
        let r = MaltesianRelay::new("test");
        assert_eq!(r.transmit(b"hello"), 5);
        assert_eq!(r.total_bytes(), 5);
        assert_eq!(r.transmit(b"!"), 1);
        assert_eq!(r.total_bytes(), 6);
    }
}
