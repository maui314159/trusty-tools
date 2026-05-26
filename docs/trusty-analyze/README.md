# trusty-analyze — documentation

Code-analysis daemon + MCP server (complexity, smells, quality metrics). Listens on 7879.

## Layout

This directory follows the standard three-subdir layout used across all
published trusty-* crates:

| Subdir | Contents |
|--------|----------|
| [`regression-testing/`](regression-testing/) | Versioned performance/quality snapshots, baseline measurements, alternate-corpus baselines. |
| [`research/`](research/) | Investigation docs, audits, decision documents. |
| [`sessions/`](sessions/) | Engineering-session summaries — narrative + reasoning. |

## Status

No `trusty-analyze` documentation has been authored yet in this layout. As work
on trusty-analyze produces benchmarks, decisions, or session summaries, add files
under the appropriate subdir and update its README index.

See [`docs/trusty-search/`](../trusty-search/) for a worked example of the
populated layout.
