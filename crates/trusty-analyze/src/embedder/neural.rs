//! Neural embedder backed by fastembed (ONNX runtime).
//!
//! Why: BOW vectors don't capture semantic similarity (e.g. "auth" vs
//! "authentication" hash to different buckets). The fastembed
//! `all-MiniLM-L6-v2` model produces 384-dim sentence embeddings that cluster
//! semantically related code together, which is the whole point of the
//! `/clusters` endpoint.
//!
//! What: thin wrapper over `fastembed::TextEmbedding`. The model is loaded
//! once at construction (~30 MB ONNX) from a configurable cache directory so
//! we use the already-cached model that trusty-search downloaded. `embed_batch`
//! returns the model's natively L2-normalized 384-dim vectors.
//!
//! Test: `neural_embedder_loads_or_skips` attempts to load from the workspace
//! `.fastembed_cache` and verifies dimension/normalization when the model is
//! present, gracefully skipping when it isn't (so CI without the cache still
//! passes).

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::{Embedder, EmbedderKind};

/// Output dimension of `all-MiniLM-L6-v2`.
const ALL_MINILM_L6_V2_DIM: usize = 384;

/// Fastembed-backed neural embedder.
///
/// Why: `TextEmbedding::embed` requires `&mut self`, but our `Embedder`
/// trait takes `&self` (so `Arc<dyn Embedder>` works across handlers).
/// What: wrap the model in a `Mutex` — embedding is CPU-bound and we
/// happily serialize it; tokio's blocking pool isn't needed because
/// fastembed releases the lock between batches.
/// Test: see `neural_embedder_loads_or_skips`.
pub struct NeuralEmbedder {
    model: Mutex<TextEmbedding>,
}

impl NeuralEmbedder {
    /// Initialize the embedder, loading the `all-MiniLM-L6-v2` ONNX model.
    ///
    /// Why: separates expensive (one-shot) construction from cheap repeated
    /// `embed_batch` calls.
    /// What: if `cache_dir` is `Some`, fastembed loads model files from there;
    /// otherwise it uses fastembed's default cache (`~/.cache/fastembed`).
    /// Network access is only attempted if the model files are missing from
    /// the cache.
    /// Test: `neural_embedder_loads_or_skips` exercises both the happy path
    /// (cache present) and the failure path (returns `Err`, caller falls back
    /// to BOW).
    pub fn new(cache_dir: Option<&Path>) -> Result<Self> {
        let mut opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2);
        if let Some(dir) = cache_dir {
            opts.cache_dir = dir.to_path_buf();
        }
        // Suppress the progress-bar spam unless explicitly asked for it.
        opts.show_download_progress = false;
        let model = TextEmbedding::try_new(opts)
            .context("failed to initialize fastembed TextEmbedding (all-MiniLM-L6-v2)")?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

impl Embedder for NeuralEmbedder {
    fn kind(&self) -> EmbedderKind {
        EmbedderKind::Neural
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // fastembed signature: embed<S: AsRef<str>>(texts: Vec<S>, batch_size: Option<usize>)
        // Vectors are L2-normalized internally.
        let owned: Vec<&str> = texts.to_vec();
        let mut guard = self
            .model
            .lock()
            .map_err(|e| anyhow::anyhow!("fastembed model mutex poisoned: {e}"))?;
        let vectors = guard
            .embed(owned, None)
            .context("fastembed embed() failed")?;
        Ok(vectors)
    }

    fn dim(&self) -> usize {
        ALL_MINILM_L6_V2_DIM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_cache_dir() -> std::path::PathBuf {
        // Single-crate layout: $CARGO_MANIFEST_DIR is the repo root.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".fastembed_cache")
    }

    #[test]
    fn neural_embedder_loads_or_skips() {
        let cache = workspace_cache_dir();
        match NeuralEmbedder::new(Some(&cache)) {
            Ok(e) => {
                assert_eq!(e.kind(), EmbedderKind::Neural);
                assert_eq!(e.dim(), 384);
                let vecs = e
                    .embed_batch(&["hello world", "fn authenticate(user: User)"])
                    .expect("embed_batch failed on loaded model");
                assert_eq!(vecs.len(), 2);
                for v in &vecs {
                    assert_eq!(v.len(), 384);
                    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    assert!(
                        (norm - 1.0).abs() < 0.01,
                        "fastembed vector not normalized: norm={norm}"
                    );
                }
                println!("neural embedder: OK (384d, normalized)");
            }
            Err(e) => {
                // Not a test failure — the model may not be cached in CI.
                println!("neural embedder skipped (model not available): {e:#}");
            }
        }
    }

    #[test]
    fn neural_embedder_empty_batch_is_ok() {
        // Even without a loaded model, the empty-batch path doesn't touch it.
        // We only exercise this when the model is available.
        let cache = workspace_cache_dir();
        if let Ok(e) = NeuralEmbedder::new(Some(&cache)) {
            let vecs = e.embed_batch(&[]).unwrap();
            assert!(vecs.is_empty());
        } else {
            println!("neural_embedder_empty_batch_is_ok skipped (model not available)");
        }
    }
}
