//! Shared text-embedding abstraction for trusty-* projects.
//!
//! Why: trusty-memory and trusty-search both shipped near-identical
//! `Embedder` traits and `FastEmbedder` implementations, with subtle
//! drift (cache vs no-cache, sync vs async warmup, `dim()` vs `dimension()`).
//! Centralising fixes one bug in one place and lets future consumers pick up
//! the embedder for free.
//!
//! What: an async `Embedder` trait with `embed_batch` as the single primitive
//! (single-text embed is a free helper), plus a production `FastEmbedder`
//! (fastembed-rs, all-MiniLM-L6-v2, 384-d) with LRU caching and ORT warmup,
//! and a `MockEmbedder` test double behind the `embedder-test-support`
//! feature.
//!
//! Test: `cargo test -p trusty-common --features embedder,embedder-test-support`
//! covers shape, cache hits, and the mock embedder. ONNX-backed tests are
//! `#[ignore]` to keep CI under one cargo-feature umbrella.

use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use lru::LruCache;
use parking_lot::Mutex;

/// Output dimension of the all-MiniLM-L6-v2 model.
///
/// Note: we now load the INT8-quantised variant (`AllMiniLML6V2Q`) which
/// produces identical 384-dim vectors but runs ~3-4× faster on CPU ONNX
/// and ships as a ~22MB file (vs 86MB for the f32 model).
pub const EMBED_DIM: usize = 384;

/// Default LRU cache capacity. Picked to be large enough to keep the
/// hot working set of repeat queries in memory but small enough that the
/// cache itself fits well inside L2/L3 on a typical developer machine.
pub const DEFAULT_CACHE_CAPACITY: usize = 256;

/// Identifier for the execution provider an embedder is actually using.
///
/// Why: callers want to log which backend is active (CPU vs CoreML/Metal vs
/// CUDA) so operators can verify the daemon is GPU-accelerated without a
/// debug log dive.
/// What: a stable, human-friendly tag returned by `FastEmbedder::provider()`.
/// Test: `FastEmbedder::new()` on Apple Silicon should yield `CoreML`; on
/// other platforms it yields `Cpu` (or `Cuda` when the `cuda` feature is on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionProvider {
    Cpu,
    CoreML,
    Cuda,
}

impl ExecutionProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionProvider::Cpu => "CPU",
            ExecutionProvider::CoreML => "CoreML",
            ExecutionProvider::Cuda => "CUDA",
        }
    }
}

impl std::fmt::Display for ExecutionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Abstraction over embedding backends.
///
/// Why: Decouple consumers from any one model so we can swap in remote APIs,
/// quantised models, or deterministic mocks without changing call sites.
/// What: a single primitive — `embed_batch` — plus a dimension accessor.
/// Single-text callers should use the [`embed_one`] convenience helper.
/// Test: covered by `FastEmbedder` and `MockEmbedder` tests below.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts. Returns one `Vec<f32>` per input, each of
    /// length `self.dimension()`. An empty input batch returns an empty Vec.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Output dimension of the produced embeddings.
    fn dimension(&self) -> usize;
}

/// Convenience helper: embed a single text via `embed_batch` and return the
/// lone vector.
///
/// Why: Most call sites only need one embedding at a time and writing
/// `.embed_batch(&[text]).await?.into_iter().next()` everywhere is noise.
/// What: builds a 1-element batch, calls `embed_batch`, returns the first
/// vector (or errors if the embedder produced nothing).
/// Test: covered indirectly by `mock_embedder_round_trip`.
pub async fn embed_one(embedder: &dyn Embedder, text: &str) -> Result<Vec<f32>> {
    let mut v = embedder.embed_batch(&[text.to_string()]).await?;
    v.pop()
        .context("embedder returned no embedding for non-empty input")
}

/// Local CPU embedder backed by fastembed-rs (ONNX runtime, all-MiniLM-L6-v2).
///
/// Why: Default to local-only embeddings so consumers have zero external
/// network dependency and predictable latency. The LRU cache keeps the hot
/// path free of redundant ONNX work for repeat strings (queries, common
/// chunks).
/// What: wraps a single `TextEmbedding` behind a `parking_lot::Mutex` (the
/// underlying `embed` requires `&mut self`) and an `LruCache<String, Vec<f32>>`.
/// Initialisation warms the ORT graph with a small batch so the first user
/// query doesn't pay the one-shot compile cost.
/// Test: `embed_batch_returns_correct_dim` and `cache_hit_is_idempotent`
/// (marked `#[ignore]` — they download a real model).
pub struct FastEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
    cache: Arc<Mutex<LruCache<String, Vec<f32>>>>,
    dim: usize,
    provider: ExecutionProvider,
}

impl FastEmbedder {
    /// Construct a new `FastEmbedder` with the default cache size.
    pub async fn new() -> Result<Self> {
        Self::with_cache_size(DEFAULT_CACHE_CAPACITY).await
    }

    /// Identifier for the execution provider this embedder is actually using.
    ///
    /// Why: callers (e.g. `trusty-search` startup logs) want to surface
    /// whether the daemon is running on CPU or GPU/ANE without poking at
    /// internals.
    /// What: returns `ExecutionProvider::CoreML` on Apple Silicon (when EP
    /// registration succeeded), otherwise `Cpu` (or `Cuda` if/when wired).
    /// Test: covered by the public-surface compile check.
    pub fn provider(&self) -> ExecutionProvider {
        self.provider
    }

    /// Build `TextInitOptions` for the given model, attempting to register
    /// the CoreML execution provider at runtime when on Apple Silicon.
    ///
    /// Why: We want zero-friction GPU/ANE acceleration on Apple Silicon
    /// without forcing users to pass `--features coreml`. fastembed-rs accepts
    /// a `Vec<ExecutionProviderDispatch>` via `with_execution_providers`, and
    /// our `ort` dep (pinned to the exact `=2.0.0-rc.12` fastembed uses) has
    /// the `coreml` feature on by default on macOS, so we can always try to
    /// build and register CoreML at runtime. On non-Apple platforms, or if
    /// CoreML registration fails for any reason, we transparently fall back
    /// to the default CPU provider.
    /// What: returns `(TextInitOptions, ExecutionProvider)` where the tag
    /// reflects which backend was actually wired in.
    /// Test: on an M-series Mac the tag is `CoreML`; on Intel/Linux/Windows
    /// (or if CoreML build fails) the tag is `Cpu`.
    fn init_options(model: EmbeddingModel) -> (TextInitOptions, ExecutionProvider) {
        use ort::execution_providers::ExecutionProviderDispatch;

        let opts = TextInitOptions::new(model);

        // Always register an explicit CPU EP with the memory arena DISABLED.
        //
        // Why: ORT's default CPU memory arena pre-allocates a large contiguous
        // slab sized to the peak tensor shape on first inference. For repos
        // with 16k+ files this arena grows to 19-53 GB before any RSS soft cap
        // can react (issue bobmatnyc/trusty-search#89). Disabling the arena
        // forces per-inference allocations that are freed after each call,
        // capping steady-state RSS at ~hundreds of MB instead of tens of GB.
        let cpu_no_arena: ExecutionProviderDispatch =
            ort::ep::CPU::default().with_arena_allocator(false).build();

        // ──────────────────────────────────────────────────────────────────
        // CUDA (Linux/Windows, NVIDIA GPU)
        //
        // Why: when the operator opts in with `--features cuda` and runs on a
        // host with a CUDA-capable GPU, we should auto-prefer the CUDA EP so
        // embedding throughput jumps from CPU-bound (~5h for a 40k-file repo)
        // to GPU-bound (target <30 min). This mirrors the always-on CoreML
        // pattern on Apple Silicon but is gated on the build-time `cuda`
        // feature because the `ort/cuda` feature requires a CUDA toolkit at
        // compile time. If the binary was built without `cuda`, this branch
        // is compiled out entirely (no runtime cost, no link-time CUDA dep).
        //
        // Operator override: setting `TRUSTY_DEVICE=cpu` forces CPU even on a
        // GPU-enabled binary. Useful for A/B benchmarking or for running on a
        // host whose GPU is reserved for another workload.
        // Test: on a g4dn.xlarge with `--features cuda` the provider tag
        // resolves to `Cuda`; setting `TRUSTY_DEVICE=cpu` reverts to `Cpu`.
        #[cfg(feature = "embedder-cuda")]
        {
            let force_cpu = std::env::var("TRUSTY_DEVICE")
                .map(|v| v.eq_ignore_ascii_case("cpu"))
                .unwrap_or(false);
            if !force_cpu {
                let cuda: ExecutionProviderDispatch = ort::ep::CUDA::default().build();
                let providers: Vec<ExecutionProviderDispatch> = vec![cuda, cpu_no_arena];
                tracing::info!(
                    "trusty-embedder: registering CUDA + CPU(no-arena) execution providers \
                     (will fall back to CPU at session-init if no CUDA device is available)"
                );
                return (
                    opts.with_execution_providers(providers),
                    ExecutionProvider::Cuda,
                );
            }
            tracing::info!(
                "trusty-embedder: TRUSTY_DEVICE=cpu set — skipping CUDA EP registration"
            );
        }

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            // Operator override: setting `TRUSTY_DEVICE=cpu` forces CPU even on
            // Apple Silicon. This is the load-bearing escape hatch for the
            // macOS jetsam kill (trusty-search#118 / blocking bug): CoreML on
            // M-series allocates from the unified memory pool, which inflates
            // *virtual* RSS to ~100+ GB during indexing of large repos
            // (>~50 MB of source). macOS jetsam treats that virtual footprint
            // as memory pressure and SIGKILLs the process, even though
            // physical RAM is fine. Falling through to the CPU-only EP path
            // (which already disables the ORT memory arena) keeps the
            // footprint bounded — at the cost of slower embedding throughput.
            // Operators who want GPU on Apple Silicon explicitly pass
            // `--device auto` (default) or `--device gpu`.
            let force_cpu = std::env::var("TRUSTY_DEVICE")
                .map(|v| v.eq_ignore_ascii_case("cpu"))
                .unwrap_or(false);
            if !force_cpu {
                let coreml: ExecutionProviderDispatch = ort::ep::CoreML::default().build();
                // CoreML first (GPU/ANE), CPU-no-arena as fallback. The CPU EP
                // still applies its session-level DisableCpuMemArena flag even
                // when CoreML handles most ops, which is what prevents the spike.
                let providers: Vec<ExecutionProviderDispatch> = vec![coreml, cpu_no_arena];
                tracing::info!(
                    "trusty-embedder: registering CoreML + CPU(no-arena) execution providers (Apple Silicon)"
                );
                return (
                    opts.with_execution_providers(providers),
                    ExecutionProvider::CoreML,
                );
            }
            tracing::info!(
                "trusty-embedder: TRUSTY_DEVICE=cpu set — skipping CoreML EP registration (Apple Silicon jetsam-kill avoidance)"
            );
        }

        #[allow(unreachable_code)]
        {
            tracing::info!("trusty-embedder: registering CPU(no-arena) execution provider");
            let providers: Vec<ExecutionProviderDispatch> = vec![cpu_no_arena];
            (
                opts.with_execution_providers(providers),
                ExecutionProvider::Cpu,
            )
        }
    }

    /// Construct with an explicit LRU capacity.
    pub async fn with_cache_size(capacity: usize) -> Result<Self> {
        let capacity =
            NonZeroUsize::new(capacity.max(1)).expect("capacity.max(1) is always non-zero");

        // fastembed's `try_new` downloads + builds an ONNX session — blocking
        // work that must run off the async reactor.
        let (model, provider) =
            tokio::task::spawn_blocking(|| -> Result<(TextEmbedding, ExecutionProvider)> {
                // Honour the explicit `TRUSTY_DEVICE=gpu` requirement: when the
                // operator asks for GPU, init_options will have selected an
                // accelerated EP. If that EP fails to initialise (no GPU, no
                // CUDA driver, etc.) AND the user did NOT explicitly require
                // GPU, we transparently fall back to CPU. With `gpu` we
                // surface the failure so the operator notices instead of
                // silently running CPU-bound on a "GPU node".
                let require_gpu = std::env::var("TRUSTY_DEVICE")
                    .map(|v| v.eq_ignore_ascii_case("gpu"))
                    .unwrap_or(false);

                let (q_opts, q_provider) = Self::init_options(EmbeddingModel::AllMiniLML6V2Q);
                let (m, provider) = match TextEmbedding::try_new(q_opts) {
                    Ok(m) => (m, q_provider),
                    Err(q_err) => {
                        // Hardware-accelerated EP build failed — most often
                        // "no CUDA device" or "CoreML EP not available". On a
                        // best-effort tier (default), retry once with CPU only
                        // so the daemon still starts. On `TRUSTY_DEVICE=gpu`
                        // we propagate the original error.
                        if q_provider != ExecutionProvider::Cpu && !require_gpu {
                            tracing::warn!(
                                "{} EP init failed ({q_err:#}); retrying with CPU-only \
                                 execution provider",
                                q_provider
                            );
                            // SAFETY: see TRUSTY_DEVICE comment in
                            // init_options — the env mutation happens before
                            // any worker thread reads it.
                            unsafe { std::env::set_var("TRUSTY_DEVICE", "cpu") };
                            let (cpu_opts, cpu_provider) =
                                Self::init_options(EmbeddingModel::AllMiniLML6V2Q);
                            match TextEmbedding::try_new(cpu_opts) {
                                Ok(m) => (m, cpu_provider),
                                Err(cpu_err) => {
                                    tracing::warn!(
                                        "AllMiniLML6V2Q init failed on CPU ({cpu_err:#}), \
                                         falling back to AllMiniLML6V2"
                                    );
                                    let (fb_opts, fb_provider) =
                                        Self::init_options(EmbeddingModel::AllMiniLML6V2);
                                    let m = TextEmbedding::try_new(fb_opts).context(
                                        "failed to initialise fastembed (tried CUDA→CPU on AllMiniLML6V2Q, then AllMiniLML6V2)",
                                    )?;
                                    (m, fb_provider)
                                }
                            }
                        } else if require_gpu {
                            return Err(anyhow::anyhow!(
                                "TRUSTY_DEVICE=gpu requested but accelerated execution provider \
                                 failed to initialise: {q_err:#}"
                            ));
                        } else {
                            tracing::warn!(
                                "AllMiniLML6V2Q init failed ({q_err:#}), falling back to AllMiniLML6V2"
                            );
                            let (fb_opts, fb_provider) =
                                Self::init_options(EmbeddingModel::AllMiniLML6V2);
                            let m = TextEmbedding::try_new(fb_opts).context(
                                "failed to initialise fastembed (tried AllMiniLML6V2Q and AllMiniLML6V2)",
                            )?;
                            (m, fb_provider)
                        }
                    }
                };
                let mut m = m;

                // Warm the graph so the first real user query is hot.
                let warmup: Vec<&str> = vec![
                    "hello world",
                    "the quick brown fox",
                    "memory palace warmup",
                    "embedding model ready",
                    "trusty common warmup",
                ];
                let _ = m
                    .embed(warmup, None)
                    .context("fastembed warmup batch failed")?;
                Ok((m, provider))
            })
            .await
            .context("spawn_blocking joined with error during embedder init")??;

        tracing::info!(
            "trusty-embedder: FastEmbedder ready (provider={}, dim={})",
            provider,
            EMBED_DIM
        );

        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            cache: Arc::new(Mutex::new(LruCache::new(capacity))),
            dim: EMBED_DIM,
            provider,
        })
    }
}

#[async_trait]
impl Embedder for FastEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Split into cached hits vs misses.
        let mut results: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut to_compute: Vec<(usize, String)> = Vec::new();
        {
            let mut cache = self.cache.lock();
            for (i, t) in texts.iter().enumerate() {
                if let Some(v) = cache.get(t) {
                    results[i] = Some(v.clone());
                } else {
                    to_compute.push((i, t.clone()));
                }
            }
        }

        if !to_compute.is_empty() {
            let model = Arc::clone(&self.model);
            let owned: Vec<String> = to_compute.iter().map(|(_, s)| s.clone()).collect();
            let computed = tokio::task::spawn_blocking(move || -> Result<Vec<Vec<f32>>> {
                let mut guard = model.lock();
                guard
                    .embed(owned, None)
                    .context("fastembed embed call failed")
            })
            .await
            .context("spawn_blocking joined with error during embed")??;

            if computed.len() != to_compute.len() {
                anyhow::bail!(
                    "fastembed returned {} embeddings, expected {}",
                    computed.len(),
                    to_compute.len()
                );
            }

            let mut cache = self.cache.lock();
            for ((idx, key), vector) in to_compute.into_iter().zip(computed.into_iter()) {
                cache.put(key, vector.clone());
                results[idx] = Some(vector);
            }
        }

        results
            .into_iter()
            .map(|opt| opt.context("missing embedding slot after batch"))
            .collect()
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

/// Deterministic test double — hashes input bytes into a fixed-dim vector.
///
/// Why: ONNX model downloads dominate test runtime and can race on cold
/// caches when multiple tests construct embedders in parallel. The mock
/// gives integration tests a "rank by similarity" surface without any I/O.
/// What: a tiny per-byte hash spread across `dim` slots, with the first byte
/// always contributing so short/empty strings still differ.
/// Test: `mock_embedder_round_trip` confirms shape + determinism.
#[cfg(any(test, feature = "embedder-test-support"))]
pub struct MockEmbedder {
    dim: usize,
}

#[cfg(any(test, feature = "embedder-test-support"))]
impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn hash_to_vec(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0_f32; self.dim];
        for (i, b) in text.bytes().enumerate() {
            let slot = (i + b as usize) % self.dim;
            v[slot] += (b as f32) / 255.0;
        }
        if let Some(first) = text.bytes().next() {
            v[0] += first as f32 / 255.0;
        }
        v
    }
}

#[cfg(any(test, feature = "embedder-test-support"))]
#[async_trait]
impl Embedder for MockEmbedder {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.hash_to_vec(t)).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_embedder_round_trip() {
        let e = MockEmbedder::new(EMBED_DIM);
        assert_eq!(e.dimension(), EMBED_DIM);
        let v = embed_one(&e, "hello").await.unwrap();
        assert_eq!(v.len(), EMBED_DIM);
        let batch = e
            .embed_batch(&["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert_ne!(batch[0], batch[1]);
    }

    #[tokio::test]
    async fn mock_embedder_empty_input_returns_empty() {
        let e = MockEmbedder::new(EMBED_DIM);
        let v = e.embed_batch(&[]).await.unwrap();
        assert!(v.is_empty());
    }

    // ONNX-backed test: downloads ~23MB on first run. Marked ignored so default
    // `cargo test` stays offline; run with `cargo test -- --ignored` when needed.
    #[tokio::test]
    #[ignore]
    async fn fastembed_returns_correct_dim() {
        let e = FastEmbedder::new().await.unwrap();
        assert_eq!(e.dimension(), 384);
        let v = embed_one(&e, "fn authenticate(user: &str) -> bool")
            .await
            .unwrap();
        assert_eq!(v.len(), 384);
        assert!(v.iter().any(|x| *x != 0.0));
    }

    #[tokio::test]
    #[ignore]
    async fn fastembed_cache_hit_is_idempotent() {
        let e = FastEmbedder::new().await.unwrap();
        let v1 = embed_one(&e, "cached").await.unwrap();
        let v2 = embed_one(&e, "cached").await.unwrap();
        assert_eq!(v1, v2);
    }

    /// Why: `TRUSTY_DEVICE=cpu` MUST suppress CoreML EP registration on Apple
    /// Silicon. CoreML on M-series uses the unified memory pool and inflates
    /// virtual RSS to ~100 GB during indexing of large repos, which triggers
    /// macOS jetsam SIGKILL even though physical RAM is fine (blocking bug,
    /// reported via trusty-search). The `--device cpu` flag is the operator's
    /// escape hatch; if `init_options` ignores it the daemon is unkillable
    /// short of disabling the launchd plist.
    /// What: serialises env mutation, sets `TRUSTY_DEVICE=cpu`, calls
    /// `init_options`, and asserts the returned `ExecutionProvider` is `Cpu`.
    /// Then clears the var and asserts it goes back to `CoreML` on macOS
    /// aarch64 (or stays `Cpu` elsewhere — both are acceptable for this test
    /// since the bug is specifically about the override being honoured).
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    #[test]
    fn trusty_device_cpu_disables_coreml_on_apple_silicon() {
        use std::sync::Mutex;
        // Serialise env mutation across all tests in this binary that touch
        // process-global env.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        // SAFETY: test is single-threaded under ENV_LOCK; no other thread
        // observes the env mutation.
        let prev = std::env::var("TRUSTY_DEVICE").ok();
        unsafe { std::env::set_var("TRUSTY_DEVICE", "cpu") };

        let (_opts, provider) = FastEmbedder::init_options(EmbeddingModel::AllMiniLML6V2Q);
        assert_eq!(
            provider,
            ExecutionProvider::Cpu,
            "TRUSTY_DEVICE=cpu must suppress CoreML EP on Apple Silicon"
        );

        // Restore for sibling tests.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("TRUSTY_DEVICE", v),
                None => std::env::remove_var("TRUSTY_DEVICE"),
            }
        }
    }

    /// Why: counterpart to the test above — confirms the default path still
    /// registers CoreML when `TRUSTY_DEVICE` is unset, so we don't regress
    /// GPU acceleration for operators who *want* it.
    /// What: clears `TRUSTY_DEVICE`, calls `init_options`, asserts `CoreML`.
    /// Test: this test, on M-series Mac.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    #[test]
    fn default_apple_silicon_uses_coreml() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let prev = std::env::var("TRUSTY_DEVICE").ok();
        // SAFETY: single-threaded under ENV_LOCK.
        unsafe { std::env::remove_var("TRUSTY_DEVICE") };

        let (_opts, provider) = FastEmbedder::init_options(EmbeddingModel::AllMiniLML6V2Q);
        assert_eq!(
            provider,
            ExecutionProvider::CoreML,
            "default behaviour on Apple Silicon must still register CoreML"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("TRUSTY_DEVICE", v),
                None => std::env::remove_var("TRUSTY_DEVICE"),
            }
        }
    }
}
