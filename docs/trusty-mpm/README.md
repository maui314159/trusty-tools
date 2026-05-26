# trusty-mpm — documentation

MPM platform — 8 binary crates: `trusty-mpm-core`, `-mcp`, `-daemon`, `-client`, `-cli`, `-tui`, `-telegram`, `-gui`. Docs covering any of the eight live here.

## Layout

This directory follows the standard three-subdir layout used across all
published trusty-* crates:

| Subdir | Contents |
|--------|----------|
| [`regression-testing/`](regression-testing/) | Versioned performance/quality snapshots, baseline measurements, alternate-corpus baselines. |
| [`research/`](research/) | Investigation docs, audits, decision documents. |
| [`sessions/`](sessions/) | Engineering-session summaries — narrative + reasoning. |

## Status

No `trusty-mpm` documentation has been authored yet in this layout. As work
on trusty-mpm produces benchmarks, decisions, or session summaries, add files
under the appropriate subdir and update its README index.

See [`docs/trusty-search/`](../trusty-search/) for a worked example of the
populated layout.
