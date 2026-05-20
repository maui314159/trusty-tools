//! Canonical fact triples shared across the search/analyzer boundary.
//!
//! Wire-compatible with `trusty_search_core::facts::FactRecord`. The
//! analyzer owns the FactStore now, but the record shape stays the same so
//! existing data files migrate without translation.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One canonical fact about an indexed corpus.
///
/// # Wire format
///
/// `id` is stored internally as `u64` but is serialized as a JSON **string**
/// to survive JavaScript clients: `Number` in JS only exactly represents
/// integers up to 2^53, so a raw `u64` field would silently lose precision
/// in `JSON.parse`. On deserialization we accept either a string (canonical)
/// or a number (legacy/Rust-to-Rust), so existing producers do not break.
///
/// # Hash stability
///
/// `id` is now a stable xxh3 hash of `(subject, predicate, object)` (see
/// `crate::core::facts::fact_hash`). Prior versions used
/// `std::collections::hash_map::DefaultHasher`, whose algorithm is **not**
/// stable across Rust toolchain versions and silently corrupted persisted
/// redb entries on compiler upgrades. Any redb files written before this
/// change are invalidated; no migration is provided — facts are derived
/// from source and will be re-asserted on the next analyzer run.
/// Tracked in issue bobmatnyc/trusty-search#64.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FactRecord {
    /// Stable xxh3 hash of `(subject, predicate, object)`.
    ///
    /// Serialized as a JSON string for JavaScript compatibility (see the
    /// type-level docs). Deserialization accepts string or number.
    #[serde(serialize_with = "serialize_id_as_string")]
    #[serde(deserialize_with = "deserialize_id_from_string_or_number")]
    pub id: u64,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// Confidence score in [0.0, 1.0]. Latest value wins on upsert.
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Chunk IDs supporting this fact. Set-merged on upsert.
    #[serde(default)]
    pub provenance: Vec<String>,
    /// Index this fact came from.
    pub index_id: String,
    /// Unix timestamp (seconds) at first creation.
    #[serde(default)]
    pub created_at: u64,
}

fn default_confidence() -> f32 {
    1.0
}

/// Serialize `u64` as a JSON string to avoid silent precision loss in
/// JavaScript clients (`Number` only represents integers up to 2^53 exactly).
fn serialize_id_as_string<S>(id: &u64, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    ser.serialize_str(&id.to_string())
}

/// Accept either a JSON string (canonical, JS-safe) or a JSON number (legacy
/// Rust-to-Rust wire) when reading a `FactRecord.id`. Rejects anything else.
fn deserialize_id_from_string_or_number<'de, D>(de: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdRepr<'a> {
        Str(&'a str),
        Owned(String),
        Num(u64),
    }

    match IdRepr::deserialize(de)? {
        IdRepr::Str(s) => s.parse::<u64>().map_err(D::Error::custom),
        IdRepr::Owned(s) => s.parse::<u64>().map_err(D::Error::custom),
        IdRepr::Num(n) => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fact_record_round_trips() {
        let f = FactRecord {
            id: 42,
            subject: "fn search".into(),
            predicate: "implements".into(),
            object: "trait Searcher".into(),
            confidence: 0.9,
            provenance: vec!["c1".into()],
            index_id: "i".into(),
            created_at: 1_700_000_000,
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: FactRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn id_serializes_as_string_for_js_compat() {
        // Why: JavaScript's Number cannot exactly represent u64 values above
        // 2^53; the wire format must be a string so JSON.parse preserves the
        // exact id. See issue bobmatnyc/trusty-search#64.
        let f = FactRecord {
            id: u64::MAX,
            subject: "s".into(),
            predicate: "p".into(),
            object: "o".into(),
            confidence: 1.0,
            provenance: vec![],
            index_id: "i".into(),
            created_at: 0,
        };
        let s = serde_json::to_string(&f).unwrap();
        // id field must be a quoted string, not a raw number.
        assert!(
            s.contains(&format!("\"id\":\"{}\"", u64::MAX)),
            "id should serialize as a JSON string, got: {s}"
        );
        let back: FactRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, u64::MAX);
    }

    #[test]
    fn id_deserializes_from_legacy_number_form() {
        // Why: older producers (and any Rust-to-Rust path that bypassed our
        // serializer) wrote `id` as a JSON number. Stay backward-compatible
        // on the read side so we don't break replay/import of old data.
        let json = r#"{
            "id": 12345,
            "subject": "s",
            "predicate": "p",
            "object": "o",
            "confidence": 1.0,
            "provenance": [],
            "index_id": "i",
            "created_at": 0
        }"#;
        let f: FactRecord = serde_json::from_str(json).unwrap();
        assert_eq!(f.id, 12345);
    }
}
