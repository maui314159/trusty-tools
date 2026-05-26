# Candle Metal RSS Validation

Status: **PENDING** — needs hardware run on Apple Silicon.

## Background

In issue [#24](https://github.com/bobmatnyc/trusty-search/issues/24) (the
original `trusty-search` jetsam SIGKILL incident) the daemon was killed
mid-indexing on Apple Silicon after the process's virtual RSS spiked to
**~72 GB**. Root cause: the default ONNX Runtime CoreML execution
provider was configured with `MLComputeUnits=ALL`, which allocates from
the unified-memory GPU pool. Each indexing batch pre-allocated a tensor
slab sized to the peak shape, and successive batches stacked rather than
returning memory to the system, so the kernel's jetsam policy fired
even though *physical* RAM was nowhere near exhausted.

The current production fix (trusty-search 0.3.55+) keeps CoreML but
switches the default to `MLComputeUnits=CPUAndNeuralEngine` — the
Neural Engine has its own memory pool, so the 72 GB unified-memory spike
disappears while we still get ~10× CPU throughput.

Issue [#54](https://github.com/bobmatnyc/trusty-tools/issues/54) added a
**Candle** BERT backend (`embedder-candle` feature) as a possible future
default. Candle uses Metal directly (no ONNX/CoreML) and has a very
different memory-allocation model. Before we can promote candle Metal
into the default position, we need to reproduce the original incident's
workload shape against the candle backend and observe its RSS behaviour.

Issue [#55](https://github.com/bobmatnyc/trusty-tools/issues/55) is that
validation harness.

## What the Harness Does

`crates/trusty-embedder/src/bin/candle_metal_bench.rs` is a standalone
binary (built only with `--features candle`) that:

1. Builds a `CandleEmbedder` with Metal enabled on macOS (CPU
   otherwise) and a baseline `FastEmbedder` (current production
   default — fastembed + ONNX + CoreML(ANE) on Apple Silicon).
2. Generates `BATCHES × BATCH_SIZE` synthetic source-code-flavoured
   chunks. Defaults are **100 batches × 1000 texts = 100,000 chunks**,
   chosen to roughly match the chunk count of a 16k-file repo at
   indexing time (the workload that originally triggered #24).
3. Embeds each batch end-to-end, sampling process RSS via the
   `sysinfo`-backed `trusty_embedder::rss::current_rss_bytes()` helper
   before and after each batch.
4. Reports per-backend peak RSS, end RSS, throughput (tokens/sec,
   conservative ~100 tokens/text estimate), and latency p50/p99.
5. Applies the **GO criteria** mechanically and exits non-zero on
   NO-GO.

### GO Criteria

A successful (GO) run requires **all** of:

- **Candle peak RSS < 8 GB** (override with `TRUSTY_BENCH_RSS_LIMIT_GB`).
  The 8 GB ceiling is deliberately ~9× lower than the 72 GB incident
  threshold but comfortably above any honest workload — anything higher
  would suggest the same unified-memory accumulation pattern that
  caused #24.
- **Candle throughput within 2× of FastEmbedder** (override with
  `TRUSTY_BENCH_THROUGHPUT_X`). A backend that is RSS-safe but 10×
  slower is not actually a candidate for the default.

If `TRUSTY_BENCH_SKIP_BASELINE` is set, the throughput criterion is
skipped (RSS check still applies).

## How to Run

Prerequisites: an Apple Silicon Mac (M1/M2/M3 series) with at least
16 GB RAM. The benchmark itself should never approach the soft RSS
limits below, but if a regression reintroduces the #24 pattern we want
headroom so the host stays usable.

```bash
# Standard run (100 × 1000 = 100k chunks, both backends).
cargo run -p trusty-embedder --features candle --release \
    --bin candle_metal_bench

# Faster smoke test: 10 × 100 = 1k chunks.
TRUSTY_BENCH_BATCHES=10 TRUSTY_BENCH_BATCH_SIZE=100 \
    cargo run -p trusty-embedder --features candle --release \
    --bin candle_metal_bench

# Candle-only (skip the FastEmbedder baseline).
TRUSTY_BENCH_SKIP_BASELINE=1 \
    cargo run -p trusty-embedder --features candle --release \
    --bin candle_metal_bench
```

The binary writes progress to stderr and the final summary table +
verdict to stdout. Exit code 0 = GO, 1 = NO-GO.

### Environment Knobs

| Variable | Default | Purpose |
|---|---|---|
| `TRUSTY_BENCH_BATCHES` | `100` | Number of batches |
| `TRUSTY_BENCH_BATCH_SIZE` | `1000` | Texts per batch |
| `TRUSTY_BENCH_SKIP_BASELINE` | unset | Skip FastEmbedder baseline if set |
| `TRUSTY_BENCH_RSS_LIMIT_GB` | `8` | GO threshold for candle peak RSS |
| `TRUSTY_BENCH_THROUGHPUT_X` | `2.0` | Max candle/FastEmbedder slowdown |

## Results

> _Pending — fill in after running on an M-series Mac with ≥ 16 GB RAM._

### Test Hardware

- Model:
- Chip:
- RAM:
- macOS version:

### Run Summary

```
(paste binary's stdout summary table here)
```

### Verdict

- [ ] GO — candle Metal is safe to promote to default
- [ ] NO-GO — candle Metal regresses RSS or throughput; reasons:

### Notes

(any observations, RSS shapes, anomalies, second-run results, etc.)

## References

- Original incident: [trusty-search #24](https://github.com/bobmatnyc/trusty-search/issues/24)
- Candle backend introduction: [trusty-tools #54](https://github.com/bobmatnyc/trusty-tools/issues/54)
- This harness: [trusty-tools #55](https://github.com/bobmatnyc/trusty-tools/issues/55)
- Current production mitigation: `crates/trusty-common/src/embedder/mod.rs`
  (`init_options` — `MLComputeUnits=CPUAndNeuralEngine` default)
