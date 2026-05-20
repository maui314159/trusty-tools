//! Embedding backends for trusty-analyzer.
//!
//! Why: clustering quality depends heavily on embedding quality. BOW hashing
//! is cheap and always available; neural embeddings (fastembed) produce
//! semantically richer vectors at the cost of a one-time model load.
//!
//! What: `Embedder` trait with two implementations — `BowEmbedder` (wraps
//! `crate::core::bow_embedding`) and `NeuralEmbedder` (fastembed
//! `all-MiniLM-L6-v2`). `EmbedderKind` selects which to use.
//!
//! Test: both embedders produce normalized vectors of the correct dimension.

pub mod bow;
pub mod neural;

pub use bow::BowEmbedder;
pub use neural::NeuralEmbedder;

/// Which embedding backend to use.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderKind {
    /// Bag-of-words hashed embedding. Deterministic, fast, no model required.
    #[default]
    Bow,
    /// Neural embedding (fastembed `all-MiniLM-L6-v2`, 384-dim).
    Neural,
}

impl EmbedderKind {
    /// Short label suitable for API responses (`"bow"` or `"neural"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bow => "bow",
            Self::Neural => "neural",
        }
    }
}

/// Common interface for all embedding backends.
///
/// Why: callers (clustering, similarity search) should be backend-agnostic so
/// that we can swap BOW for neural without touching call sites.
/// What: embed a batch of texts into a `Vec<Vec<f32>>` of consistent
/// dimension; expose `kind()` for response metadata and `dim()` for sanity
/// checks.
/// Test: see per-implementation tests in `bow.rs` and `neural.rs`.
pub trait Embedder: Send + Sync {
    /// Which backend this is — used in API responses for transparency.
    fn kind(&self) -> EmbedderKind;
    /// Embed a batch of texts. Returns one vector per input, all same dimension.
    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>>;
    /// Embedding dimension produced by this backend.
    fn dim(&self) -> usize;
}
