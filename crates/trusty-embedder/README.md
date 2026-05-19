# trusty-embedder

Shared text-embedding abstraction for the `trusty-*` family of projects.

Provides a single async `Embedder` trait and a production-ready
`FastEmbedder` implementation backed by [fastembed-rs](https://crates.io/crates/fastembed)
(all-MiniLM-L6-v2, 384-dimensional output, INT8-quantized ONNX) with LRU
caching and ORT warmup. A `MockEmbedder` test double is available behind
the `test-support` feature.

## Installation

```sh
cargo add trusty-embedder
```

For tests/benches that want a deterministic stand-in:

```sh
cargo add trusty-embedder --features test-support
```

## Quick Example

```rust
use trusty_embedder::{Embedder, FastEmbedder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let embedder = FastEmbedder::new()?;
    let vectors = embedder
        .embed_batch(&["hello world".into(), "trusty".into()])
        .await?;
    assert_eq!(vectors[0].len(), trusty_embedder::EMBEDDING_DIM);
    Ok(())
}
```

## Feature Flags

- **`test-support`** *(optional)* — exposes `MockEmbedder`, a deterministic
  hash-based embedder for unit tests that don't want to load an ONNX model.

## Notes

- Model files are downloaded by `fastembed` on first use and cached in the
  standard fastembed cache location.
- The crate falls back from `AllMiniLML6V2Q` (INT8) to `AllMiniLML6V2` (FP32)
  when the quantized variant is unavailable in the host fastembed build.
- `embed_batch` is the single primitive — single-text embedding is a thin
  helper that wraps a one-element batch.

## License

Licensed under the [Elastic License 2.0](https://www.elastic.co/licensing/elastic-license).

## Repository

<https://github.com/bobmatnyc/trusty-common>
