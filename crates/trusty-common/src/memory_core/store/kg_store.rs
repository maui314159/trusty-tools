//! Why: The knowledge graph is migrating from SQLite to redb for embedded
//!      ACID storage without the r2d2/rusqlite dependency chain.
//! What: Table definitions, composite key encoding, and postcard value
//!       serialization for the redb-backed KG.
//! Test: Unit tests for encode/decode round-trips in this module.

use redb::TableDefinition;
use serde::{Deserialize, Serialize};

// ── Table definitions ────────────────────────────────────────────────────

/// Primary triple store.
///
/// Why: Composite key encoding allows efficient range scans by subject prefix
///      while preserving Ord semantics for redb's BTreeMap-backed tables.
/// What: Key = `[subject_len: u16 BE][subject bytes][predicate bytes]`.
///       Value = postcard-encoded [`TripleValue`].
/// Test: See `round_trip_triple_key` and `subject_prefix_range_simulation`.
pub const TRIPLES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("triples");

/// Reverse index: object → (subject+predicate key) for O(degree) reverse lookup.
///
/// Why: Without this, finding "who points at X" requires a full scan of TRIPLES.
/// What: Key = `[object_len: u16 BE][object bytes][subject_len: u16 BE][subject bytes][predicate bytes]`.
///       Value = empty `&[u8]`.
/// Test: Range-scan simulation in `object_index_key_orders_by_object`.
pub const TRIPLES_BY_OBJECT: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("triples_by_object");

/// Predicate index for queries like "all triples with predicate P".
///
/// Why: Predicate-first range scans (e.g. all `created_by` edges).
/// What: Key = `[predicate_len: u16 BE][predicate bytes][subject_len: u16 BE][subject bytes]`.
///       Value = empty `&[u8]`.
/// Test: Range-scan simulation in `predicate_index_key_orders_by_predicate`.
pub const TRIPLES_BY_PREDICATE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("triples_by_predicate");

/// Active subject count — maintained for O(1) `count_active_triples`.
///
/// Why: Computing the active triple count for a subject on demand requires a
///      range scan; we maintain it incrementally for cheap reads.
/// What: Key = subject str (UTF-8 bytes — the entire key is the subject, no
///       length prefix needed since there is only one component).
///       Value = `u64` LE (count of active triples for this subject).
/// Test: `round_trip_u64`.
pub const ACTIVE_SUBJECT_COUNTS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("active_subject_counts");

/// Drawer metadata.
///
/// Why: Drawers are addressable by UUID; keep them in a separate table so
///      drawer listing does not interleave with triple range scans.
/// What: Key = uuid bytes (`[u8; 16]`).
///       Value = postcard-encoded [`DrawerRecord`].
/// Test: `round_trip_drawer_record`.
pub const DRAWERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("drawers");

/// Payload store (for `open-mpm`'s `TrustyBackedMemoryStore`).
///
/// Why: Payloads are namespaced by segment and addressed by id; share the
///      same redb env as the KG so payload + KG ops can ride a single
///      transaction.
/// What: Key = `[segment_len: u16 BE][segment bytes][id bytes]`.
///       Value = postcard-encoded payload string.
/// Test: `round_trip_payload_key`.
pub const PAYLOADS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("payloads");

// ── Value types (postcard-serializable) ──────────────────────────────────

/// Why: The TRIPLES table value carries the object plus temporal/confidence
///      metadata; keep it serde-derived so postcard can pack it densely.
/// What: A single triple's value payload — object, valid time window,
///       confidence, optional provenance string.
/// Test: `round_trip_triple_value`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripleValue {
    pub object: String,
    /// Unix epoch milliseconds when this fact became valid.
    pub valid_from_ms: i64,
    /// Unix epoch milliseconds when this fact was invalidated. `None` = active.
    pub valid_to_ms: Option<i64>,
    pub confidence: f32,
    pub provenance: Option<String>,
}

/// Why: Drawer rows carry content + tags + importance for the Memory Palace
///      "drawer" abstraction; serde-encoded so we can add fields without a
///      schema migration.
/// What: A drawer's metadata payload.
/// Test: `round_trip_drawer_record`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawerRecord {
    pub room_id: String,
    pub content: String,
    pub importance: f32,
    pub tags: Vec<String>,
    pub source_file: Option<String>,
    /// Unix epoch milliseconds when the drawer was created.
    pub created_at_ms: i64,
}

// ── Key encoding helpers ─────────────────────────────────────────────────

/// Why: redb requires Ord-preserving byte keys for range scans. Composite
///      string keys are encoded with a u16 BE length prefix per leading
///      component so prefix-based range scans (`subject..`) work correctly.
/// What: Encodes `(subject, predicate)` → `Vec<u8>` for TRIPLES table lookup.
/// Test: `round_trip_triple_key`.
pub fn encode_triple_key(subject: &str, predicate: &str) -> Vec<u8> {
    let s = subject.as_bytes();
    let p = predicate.as_bytes();
    let mut out = Vec::with_capacity(2 + s.len() + p.len());
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
    out.extend_from_slice(p);
    out
}

/// Why: Round-trip decode for diagnostic/debug paths and tests.
/// What: Splits an encoded triple key back into `(subject, predicate)`.
///       Returns `None` if the key is malformed (length prefix exceeds bytes
///       remaining or interior bytes are not valid UTF-8).
/// Test: `round_trip_triple_key`.
pub fn decode_triple_key(bytes: &[u8]) -> Option<(String, String)> {
    if bytes.len() < 2 {
        return None;
    }
    let s_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let rest = &bytes[2..];
    if rest.len() < s_len {
        return None;
    }
    let subject = std::str::from_utf8(&rest[..s_len]).ok()?.to_string();
    let predicate = std::str::from_utf8(&rest[s_len..]).ok()?.to_string();
    Some((subject, predicate))
}

/// Why: Reverse lookup by object — find all (subject, predicate) pairs that
///      point at a given object.
/// What: Encodes `(object, subject, predicate)` → composite key with two
///       length-prefixed leading components so the object prefix sorts first.
/// Test: `object_index_key_orders_by_object`.
pub fn encode_object_index_key(object: &str, subject: &str, predicate: &str) -> Vec<u8> {
    let o = object.as_bytes();
    let s = subject.as_bytes();
    let p = predicate.as_bytes();
    let mut out = Vec::with_capacity(4 + o.len() + s.len() + p.len());
    out.extend_from_slice(&(o.len() as u16).to_be_bytes());
    out.extend_from_slice(o);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
    out.extend_from_slice(p);
    out
}

/// Why: Predicate-first index — find all subjects connected via a given
///      predicate.
/// What: Encodes `(predicate, subject)` → composite key with two
///       length-prefixed components.
/// Test: `predicate_index_key_orders_by_predicate`.
pub fn encode_predicate_index_key(predicate: &str, subject: &str) -> Vec<u8> {
    let p = predicate.as_bytes();
    let s = subject.as_bytes();
    let mut out = Vec::with_capacity(4 + p.len() + s.len());
    out.extend_from_slice(&(p.len() as u16).to_be_bytes());
    out.extend_from_slice(p);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
    out
}

/// Why: Range scans by subject use `range(prefix..end)` where `prefix` is
///      `[subject_len][subject]`; this helper computes that prefix.
/// What: Subject prefix = `[subject_len: u16 BE][subject bytes]`.
/// Test: `subject_prefix_range_simulation`.
pub fn subject_prefix(subject: &str) -> Vec<u8> {
    let s = subject.as_bytes();
    let mut out = Vec::with_capacity(2 + s.len());
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
    out
}

/// Why: The PAYLOADS table is keyed by `(segment, id)`; this helper produces
///      the composite key for both reads and writes.
/// What: Payload key = `[segment_len: u16 BE][segment bytes][id bytes]`.
/// Test: `round_trip_payload_key`.
pub fn encode_payload_key(segment: &str, id: &[u8]) -> Vec<u8> {
    let seg = segment.as_bytes();
    let mut out = Vec::with_capacity(2 + seg.len() + id.len());
    out.extend_from_slice(&(seg.len() as u16).to_be_bytes());
    out.extend_from_slice(seg);
    out.extend_from_slice(id);
    out
}

// ── Value encode/decode ──────────────────────────────────────────────────

/// Why: All value types share a single postcard codec — central helper keeps
///      call sites concise and the format consistent.
/// What: Serializes `v` to a `Vec<u8>` using postcard.
/// Test: `round_trip_triple_value`, `round_trip_drawer_record`.
pub fn encode_value<T: Serialize>(v: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(v)
}

/// Why: Mirror of [`encode_value`] for reads.
/// What: Deserializes a postcard-encoded byte slice into `T`.
/// Test: `round_trip_triple_value`, `round_trip_drawer_record`.
pub fn decode_value<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

/// Why: redb table values are `&[u8]`, so the `active_subject_counts` u64
///      needs an explicit LE encoding rather than postcard wrapping.
/// What: Encodes a `u64` as 8 little-endian bytes.
/// Test: `round_trip_u64`.
pub fn encode_u64(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

/// Why: Mirror of [`encode_u64`].
/// What: Decodes 8 LE bytes into a `u64`. Returns 0 if `bytes.len() < 8`
///       (matches redb's "missing key returns zero" convention for counts).
/// Test: `round_trip_u64`.
pub fn decode_u64(bytes: &[u8]) -> u64 {
    if bytes.len() < 8 {
        return 0;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(buf)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_triple_key() {
        let key = encode_triple_key("user:alice", "knows");
        let (subject, predicate) = decode_triple_key(&key).expect("decode");
        assert_eq!(subject, "user:alice");
        assert_eq!(predicate, "knows");
    }

    #[test]
    fn round_trip_triple_key_empty_predicate() {
        let key = encode_triple_key("subj", "");
        let (s, p) = decode_triple_key(&key).expect("decode");
        assert_eq!(s, "subj");
        assert_eq!(p, "");
    }

    #[test]
    fn decode_triple_key_rejects_truncated() {
        assert!(decode_triple_key(&[]).is_none());
        assert!(decode_triple_key(&[0u8]).is_none());
        // length prefix says 10 bytes follow but only 2 do
        assert!(decode_triple_key(&[0, 10, b'a', b'b']).is_none());
    }

    #[test]
    fn subject_prefix_range_simulation() {
        // Every triple for subject "alice" must start with `subject_prefix("alice")`,
        // and no triple for subject "alicia" should — even though "alicia" starts
        // with "alic" — because the length prefix differs.
        let prefix_alice = subject_prefix("alice");
        let alice_knows = encode_triple_key("alice", "knows");
        let alice_likes = encode_triple_key("alice", "likes");
        let alicia_knows = encode_triple_key("alicia", "knows");

        assert!(alice_knows.starts_with(&prefix_alice));
        assert!(alice_likes.starts_with(&prefix_alice));
        assert!(!alicia_knows.starts_with(&prefix_alice));
    }

    #[test]
    fn subject_prefix_orders_lexicographically() {
        // BTreeMap-backed redb tables sort keys lexicographically. Length-
        // prefixed keys with the same length sort by content order.
        let k1 = encode_triple_key("aaa", "p");
        let k2 = encode_triple_key("aab", "p");
        let k3 = encode_triple_key("bbb", "p");
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn object_index_key_orders_by_object() {
        // All entries with the same object must sort together and before any
        // entry with a strictly greater object.
        let k1 = encode_object_index_key("obj_a", "s1", "p1");
        let k2 = encode_object_index_key("obj_a", "s2", "p2");
        let k3 = encode_object_index_key("obj_b", "s0", "p0");
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn predicate_index_key_orders_by_predicate() {
        let k1 = encode_predicate_index_key("knows", "s1");
        let k2 = encode_predicate_index_key("knows", "s2");
        let k3 = encode_predicate_index_key("likes", "s0");
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn round_trip_triple_value() {
        let v = TripleValue {
            object: "user:bob".to_string(),
            valid_from_ms: 1_700_000_000_000,
            valid_to_ms: Some(1_710_000_000_000),
            confidence: 0.85,
            provenance: Some("test/path.rs:42".to_string()),
        };
        let bytes = encode_value(&v).expect("encode");
        let decoded: TripleValue = decode_value(&bytes).expect("decode");
        assert_eq!(v, decoded);
    }

    #[test]
    fn round_trip_triple_value_active() {
        // Active triple has `valid_to_ms = None`.
        let v = TripleValue {
            object: "x".to_string(),
            valid_from_ms: 0,
            valid_to_ms: None,
            confidence: 1.0,
            provenance: None,
        };
        let bytes = encode_value(&v).expect("encode");
        let decoded: TripleValue = decode_value(&bytes).expect("decode");
        assert_eq!(v, decoded);
    }

    #[test]
    fn round_trip_drawer_record() {
        let d = DrawerRecord {
            room_id: "room-1".to_string(),
            content: "Project kickoff notes".to_string(),
            importance: 0.7,
            tags: vec!["project".to_string(), "kickoff".to_string()],
            source_file: Some("notes/2025-01-01.md".to_string()),
            created_at_ms: 1_700_000_000_000,
        };
        let bytes = encode_value(&d).expect("encode");
        let decoded: DrawerRecord = decode_value(&bytes).expect("decode");
        assert_eq!(d, decoded);
    }

    #[test]
    fn round_trip_u64() {
        for v in [0u64, 1, 42, u64::MAX, 1_000_000_000_000] {
            let bytes = encode_u64(v);
            assert_eq!(decode_u64(&bytes), v);
        }
    }

    #[test]
    fn decode_u64_short_returns_zero() {
        // Match the "missing key returns zero" convention used by callers.
        assert_eq!(decode_u64(&[]), 0);
        assert_eq!(decode_u64(&[1, 2, 3]), 0);
    }

    #[test]
    fn round_trip_payload_key() {
        let id = [0xAB, 0xCD, 0xEF, 0x01];
        let k = encode_payload_key("session", &id);
        // Verify segment length prefix.
        assert_eq!(&k[0..2], &(7u16).to_be_bytes());
        assert_eq!(&k[2..9], b"session");
        assert_eq!(&k[9..], &id);
    }

    #[test]
    fn payload_keys_group_by_segment() {
        // Keys with the same segment prefix sort together.
        let k1 = encode_payload_key("seg_a", &[1, 2, 3]);
        let k2 = encode_payload_key("seg_a", &[1, 2, 4]);
        let k3 = encode_payload_key("seg_b", &[0]);
        assert!(k1 < k2);
        assert!(k2 < k3);
    }

    #[test]
    fn table_definitions_have_distinct_names() {
        use redb::TableHandle;
        // Sanity check: no two tables share the same name (would alias in redb).
        let names = [
            TRIPLES.name(),
            TRIPLES_BY_OBJECT.name(),
            TRIPLES_BY_PREDICATE.name(),
            ACTIVE_SUBJECT_COUNTS.name(),
            DRAWERS.name(),
            PAYLOADS.name(),
        ];
        for i in 0..names.len() {
            for j in (i + 1)..names.len() {
                assert_ne!(names[i], names[j]);
            }
        }
    }
}
