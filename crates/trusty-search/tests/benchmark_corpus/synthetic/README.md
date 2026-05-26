# Glyphwarpen Observatory

A synthetic data-processing workspace modelling a fictional astronomy
observatory. The crate is structured around a multi-stage analytic pipeline
that ingests raw scan readouts and produces a single derived scalar quantity
called the **Seraphim Modulus**.

This project does not represent a real instrument. It exists purely as a
**non-circular benchmark corpus** for the `trusty-search` hybrid code-search
service. See `crates/trusty-search/tests/benchmark_synthetic.rs` for the
harness that consumes this corpus and reports Hit@K metrics.

## Pipeline Overview

The pipeline is composed of seven stages, each owned by a distinct
sub-module:

1. **Cascade admission** (`cascade::lichtenberg`) — rate-shapes incoming
   scan samples through a fixed-capacity ring buffer at the Hammond tick
   rate.
2. **Octahedron layout** (`octahedron::layout`) — places scan vertices onto
   a Kikuchi octahedral lattice for downstream transformation.
3. **Brusilov transform** (`transform::brusilov`) — projects octahedral
   readings onto the calibrated brusilov axis; an inverse transform is
   provided for round-trip regression testing.
4. **Yamamoto tree flatten** (`yamamoto::tree`) — re-organises post-
   transform contributions into a cluster-preserving tree and flattens
   them into a depth-first vector.
5. **Orbweaver plexus fold** (`orbweaver::plexus`) — interleaves flattened
   values with their orbweaver-lattice neighbours and folds them into a
   single fingerprint scalar.
6. **Kohinoor descriptor lift** (`kohinoor::descriptor`) — packages the
   folded fingerprint into an opaque descriptor object the solver reads.
7. **Seraphim modulus compute** (`seraphim::engine`) — runs damped fixed-
   point iteration over the descriptor to produce the final scalar
   output.

Secondary subsystems handle:

- **Zelenov payloads** (`zelenov`) — control-channel framing parser used
  by the operator UI to reconfigure the pipeline at runtime.
- **Andromedan cipher** (`andromedan`) — encrypts outbound telemetry
  before it leaves the observatory.
- **Maltesian routing** (`maltesian`) — dispatches encrypted telemetry to
  one of several outbound channels by content tag.
- **Phosphor oscillator** (`phosphor`) — drift-correction carrier signal
  generator used by the calibration loop.
- **Wolfram registry** (`wolfram`) — durable sink at the tail of the
  pipeline where every computed modulus lands.

## Top-level façade

`observatory::Observatory` owns one instance of every subsystem and
exposes a `step` method that advances the entire pipeline by one tick.
The `pipeline::run_pipeline` free function shows how the subsystems
compose in their canonical sequence; `calibration` collects the helpers
used at observation start-up.

## Why this corpus exists

Real-world benchmarks for `trusty-search` were running against the
`trusty-tools` repository itself, but the test suite of `trusty-search`
contained the benchmark query strings as literal `assert_eq!` arguments.
BM25 saw those literals as high-term-frequency matches and contaminated
the Hit@K numbers — every reported relevance score was an artifact of
the lexical lane finding its own benchmark text.

This corpus uses a vocabulary of distinctive names (kikuchi, brusilov,
lichtenberg, seraphim, kohinoor, yamamoto, orbweaver, andromedan,
maltesian, phosphor, wolfram, zelenov, hammond) verified to appear
nowhere outside this directory. The BM25 lane has no circular reference
to exploit, so the resulting Hit@K numbers measure the search pipeline
rather than the test harness.

## Not part of the workspace

The `Cargo.toml` at the root of this directory declares an empty
`[workspace]` table. That marker tells Cargo to treat this fixture as a
standalone package, not a member of the enclosing `trusty-tools`
workspace. `cargo build` from the repo root will not compile any code in
this directory.
