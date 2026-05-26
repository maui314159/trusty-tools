# Session Summaries

This folder contains session-summary documents that capture the trajectory, decisions, and empirical findings of individual engineering sessions.

## Purpose

Session documents provide:
- **Narrative context**: The arc from problem statement to shipped features
- **Decision rationale**: Why each architectural choice was made in sequence
- **Empirical evidence**: Benchmark results and findings that shaped each decision
- **Prioritized backlogs**: What shipped, what's blocked, and why

These are distinct from [regression-testing/](../regression-testing/) (which tracks metrics over time) and [research/](../research/) (which capture isolated investigation artifacts).

## Naming Convention

`SESSION-<YYYY-MM-DD>-<topic>.md`

Example: `SESSION-2026-05-25-trusty-search.md`

## Session Summaries

- [`SESSION-2026-05-25-trusty-search.md`](SESSION-2026-05-25-trusty-search.md) — Major trusty-search engineering session: walker .gitignore fix → staged pipeline → per-lane MCP tools. v0.8.0 → v0.10.0. 20+ tickets filed, 10+ shipped. 2,342 words.

---

**Related**: [Regression Testing Index](../regression-testing/README.md) | [Research & Decision Documents Index](../research/README.md)
