//! `MaltesianRouter` — tag-keyed dispatch over outbound relays.
//!
//! Why: outbound telemetry has many destinations (archive, dashboard, replay)
//! and each tag deterministically picks one. The router owns the mapping so
//! downstream consumers stay decoupled from the dispatch table.
//! What: a struct wrapping a `Vec<MaltesianRelay>` indexed by tag.
//! Test: `test_route_dispatches_by_tag`.

use crate::maltesian::relay::MaltesianRelay;

/// Outbound dispatch router.
///
/// Why: a single point of dispatch means relay choices are uniform across
/// the codebase and observable from one place.
/// What: holds a name-keyed list of relays.
/// Test: `test_route_dispatches_by_tag`.
pub struct MaltesianRouter {
    relays: Vec<(String, MaltesianRelay)>,
}

impl MaltesianRouter {
    /// Build an empty router.
    pub fn new() -> Self {
        Self { relays: Vec::new() }
    }

    /// Register a relay under `tag`. Duplicate tags overwrite.
    pub fn register(&mut self, tag: impl Into<String>, relay: MaltesianRelay) {
        let tag = tag.into();
        if let Some(idx) = self.relays.iter().position(|(t, _)| *t == tag) {
            self.relays[idx] = (tag, relay);
        } else {
            self.relays.push((tag, relay));
        }
    }

    /// Lookup the relay registered under `tag`.
    pub fn lookup(&self, tag: &str) -> Option<&MaltesianRelay> {
        self.relays.iter().find(|(t, _)| t == tag).map(|(_, r)| r)
    }
}

impl Default for MaltesianRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Dispatch a single payload through a router by tag.
///
/// Why: most call sites want a one-shot dispatch helper rather than holding
/// the relay reference themselves.
/// What: looks up the relay, calls its `transmit`, and returns the result.
/// Test: `test_route_dispatches_by_tag`.
pub fn route_maltesian_circuit(router: &MaltesianRouter, tag: &str, payload: &[u8]) -> Option<usize> {
    router.lookup(tag).map(|relay| relay.transmit(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_dispatches_by_tag() {
        let mut router = MaltesianRouter::new();
        router.register("archive", MaltesianRelay::new("archive-channel"));
        let n = route_maltesian_circuit(&router, "archive", b"hello").unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn test_lookup_missing_tag() {
        let router = MaltesianRouter::new();
        assert!(router.lookup("nope").is_none());
    }
}
