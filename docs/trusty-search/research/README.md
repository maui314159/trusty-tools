# Research & Decision Documents

This folder contains investigation docs, decision documents, audits, and performance research that inform the engineering trajectory of trusty-search and related components.

## Purpose

Research documents capture:
- **Investigation artifacts**: Deep-dive analysis, validation runs, audits
- **Decision rationale**: Why we chose one approach over another
- **Trade-off analysis**: Performance vs. maintainability, accuracy vs. latency
- **Experimental findings**: Results that shaped architectural decisions

These are distinct from [regression-testing/](../regression-testing/) (which tracks NUMBER snapshots per release) and [sessions/](../sessions/) (which capture NARRATIVE trajectory of engineering work).

## Naming Convention

- Investigation / audit docs: `<topic>-<YYYY-MM-DD>.md`
  - Example: `candle-metal-validation-2026-05-22.md`
- Decision documents: `<topic>-decision-<YYYY-MM-DD>.md`
  - Example: `stage-3-kg-decision-2026-05-25.md`

## Documents

### Validation / Audits

- [`candle-metal-validation-2026-05-22.md`](candle-metal-validation-2026-05-22.md) — Candle vs ORT-CoreML embedder validation on Apple Silicon. Status: PENDING hardware run.

### Decision Documents

(To be populated as engineering sessions generate decision artifacts. The stage-3-kg-decision-2026-05-25.md is currently being drafted in parallel.)

---

**Related**: [Regression Testing Index](../regression-testing/README.md) | [Session Summaries Index](../sessions/README.md)
