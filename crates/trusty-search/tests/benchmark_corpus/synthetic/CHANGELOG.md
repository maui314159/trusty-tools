# Glyphwarpen Observatory — Changelog

## 0.1.0 — 2026-05-25

Initial synthetic corpus.

### Added

- 13 pipeline subsystems: cascade, octahedron, transform, yamamoto,
  orbweaver, kohinoor, seraphim, zelenov, andromedan, maltesian,
  phosphor, wolfram, plus the constants module.
- Top-level orchestrator `Observatory` in `observatory.rs` that owns one
  instance of every subsystem and exposes a per-tick `step` method.
- End-to-end pipeline driver `run_pipeline` in `pipeline.rs` that
  composes cascade admission, octahedron layout, brusilov transform,
  yamamoto flatten, orbweaver fold, kohinoor lift, and seraphim modulus
  compute into a single call.
- Calibration helpers `calibrate_brusilov` and `lock_phosphor` in
  `calibration.rs` for observation start-up.
- Diagnostics report `summarise_diagnostics` in `diagnostics.rs` for
  operator-UI consumption.
- `config.yaml` operator configuration covering the pipeline tunables
  for the canonical observation profile.

### Rationale

This corpus exists to serve as the non-circular benchmark target for
`trusty-search`. The vocabulary of names has been chosen to have zero
overlap with the rest of the `trusty-tools` repository — so the BM25
lane has nothing outside this directory to match against. See `README.md`
for the longer motivation.
