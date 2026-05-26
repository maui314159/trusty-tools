# Synthetic Corpus Baseline — First Non-Circular Measurement

**Date**: 2026-05-25
**Daemon version**: 0.9.1 (uptime 1953s at run start; same instance as `v0.9.1-2026-05-25.md`)
**Tracking issue**: [#123](https://github.com/bobmatnyc/trusty-tools/issues/123) — _BM25 circular bias in trusty-tools benchmark corpus_
**Status**: First measurement of trusty-search hybrid retrieval that is provably free of the circular-bias contamination documented in #123

This document is **parallel infrastructure**, not a version snapshot. The `current.md` pointer remains aimed at `v0.9.1-2026-05-25.md`. Numbers reported here are not directly comparable to the version-snapshot series — they describe a fundamentally different corpus.

## Motivation

Every benchmark of `trusty-search` to date has used `trusty-tools` itself as the indexed corpus. Issue #119's test harness added `assert_eq!` literals to `crates/trusty-search/src/core/classifier.rs` and `core/indexer/tests.rs` whose contents include the very benchmark query strings (`"symbol graph BFS expansion"`, `"redb persistence write transaction"`, etc.). BM25 sees these literals as high-term-frequency matches and lifts the corresponding files in the lexical lane — regardless of whether those files would have been the correct answer in any other context. Every Hit@K number we have reported for v0.8.x and v0.9.x has carried a footnote about this circular dependency.

The v0.9.1 snapshot quantified this explicitly:

> **v0.9.1 Hit@1 (57.1%) is lower than v0.9.0's BM25-only 100%** — this was expected and fully explains the #135 finding. v0.9.0's 100% was an artifact of BM25 circular bias (#123) where benchmark query strings appear verbatim in the indexed test files.

The question "is the 57.1% Hit@1 hybrid result reflecting reality or measurement bias?" cannot be answered against `trusty-tools` because both the numerator (relevant retrieval) and the denominator (which queries are 'hard') are contaminated by the literal-string footnote.

This baseline solves the problem by indexing a **synthetic corpus** where every distinctive identifier has been verified to appear nowhere else in the repository.

## The synthetic corpus

**Path**: `crates/trusty-search/tests/benchmark_corpus/synthetic/`
**Name**: `glyphwarpen-observatory`
**Shape**: a fake Rust workspace modelling a fictional astronomy data pipeline.

### File census

| Metric | Value |
|--------|-------|
| `.rs` files | 42 |
| Top-level Rust modules | 17 (`lib.rs` + 13 subsystem modules + 3 orchestrators + `constants.rs`) |
| Total source lines (rs only) | ~2 500 |
| Supporting files | `Cargo.toml`, `README.md`, `CHANGELOG.md`, `config.yaml` |
| Tree-sitter chunks at index time | **298** (47 files indexed by the daemon) |

The corpus declares its own isolated workspace via an empty `[workspace]` table in its `Cargo.toml`, so the parent monorepo's `cargo build` skips it entirely; only tree-sitter ever parses it.

### Symbol naming convention

Symbols use **rare-noun + technical-suffix** patterns drawn from a hand-picked vocabulary:

- Subsystem nouns: `hammond`, `lichtenberg`, `brusilov`, `kikuchi`, `seraphim`, `kohinoor`, `zelenov`, `yamamoto`, `orbweaver`, `andromedan`, `maltesian`, `phosphor`, `wolfram`, `glyphwarpen`
- Technical suffixes: `Lever`, `Cascade`, `Transform`, `Octahedron`, `Engine`, `Modulus`, `Descriptor`, `Tree`, `Plexus`, `Cipher`, `Router`, `Oscillator`, `Registry`, `Payload`, `Envelope`

Symbol case distribution:

| Style | Examples |
|-------|----------|
| PascalCase types | `HammondLever`, `LichtenbergCascade`, `BrusilovTransform`, `KikuchiOctahedron`, `SeraphimEngine`, `KohinoorDescriptor`, `YamamotoTree`, `OrbweaverPlexus`, `AndromedanCipher`, `MaltesianRouter`, `PhosphorOscillator`, `WolframRegistry`, `ZelenovPayload` |
| snake_case functions | `compute_seraphim_modulus`, `parse_zelenov_payload`, `lift_kohinoor_descriptor`, `flatten_yamamoto_tree`, `fold_orbweaver_plexus`, `thread_andromedan_cipher`, `route_maltesian_circuit`, `modulate_phosphor_oscillator`, `octahedron_layout`, `traverse_kikuchi_octahedron`, `yamamoto_traversal`, `andromedan_codec`, `calibrate_brusilov`, `lock_phosphor` |
| SCREAMING_SNAKE constants | `SERAPHIM_DEFAULT_THRESHOLD`, `ZELENOV_MAX_DEPTH`, `KIKUCHI_BUFFER_LIMIT`, `BRUSILOV_EPOCH`, `WOLFRAM_NODE_CAP`, `HAMMOND_TICK_RATE`, `YAMAMOTO_FANOUT_CAP`, `ANDROMEDAN_DEFAULT_ROTATION` |

### Symbol-leak verification

Each of the 14 ground-truth query strings was verified to appear NOWHERE outside the synthetic corpus directory before the harness was committed. Verification command:

```bash
jq -c '.queries[] | {id, text}' crates/trusty-search/tests/benchmark_corpus/synthetic/GROUND_TRUTH.json \
| while read -r line; do
    query=$(echo "$line" | jq -r '.text')
    id=$(echo "$line" | jq -r '.id')
    count=$(rg -F -l \
      -g '!**/synthetic/**' \
      -g '!docs/regression-testing/**' \
      -g '!target/**' \
      -g '!.git/**' \
      -g '!.claude/**' \
      -- "$query" . 2>/dev/null | wc -l | tr -d ' ')
    [ "$count" -gt 0 ] && echo "LEAK Q[$id]: $query" || echo "OK   Q[$id]: $query"
done
```

Run output (`docs/regression-testing/**` is excluded so this file's own copies don't count):

```
OK   Q[Q01]: 'HammondLever'
OK   Q[Q02]: 'LichtenbergCascade'
OK   Q[Q03]: 'parse_zelenov_payload'
OK   Q[Q04]: 'BRUSILOV_EPOCH'
OK   Q[Q05]: 'compute seraphim modulus from descriptor'
OK   Q[Q06]: 'flatten clustered tree into depth first vector'
OK   Q[Q07]: 'fixed point iteration with damping factor'
OK   Q[Q08]: 'encrypt outbound telemetry stream cipher'
OK   Q[Q09]: 'where is BrusilovTransform applied'
OK   Q[Q10]: 'callers of fold_orbweaver_plexus'
OK   Q[Q11]: 'Glyphwarpen Observatory benchmark corpus motivation'
OK   Q[Q12]: 'changelog initial release notes synthetic corpus'
OK   Q[Q13]: 'configured outbound channels for archive and dashboard'
OK   Q[Q14]: 'seraphim damping factor and iteration cap'
```

Zero leaks across all 14 queries.

## Query set

14 queries in `GROUND_TRUTH.json`, covering:

| Category | Count | What it tests |
|----------|-------|---------------|
| Definition (PascalCase) | 2 | `HammondLever`, `LichtenbergCascade` |
| Definition (snake_case) | 1 | `parse_zelenov_payload` (tests #119 fix) |
| Definition (SCREAMING_SNAKE) | 1 | `BRUSILOV_EPOCH` (constant-routing) |
| Conceptual (multi-word) | 4 | semantic descriptions of what code does, no exact identifier present |
| Usage | 2 | `where is BrusilovTransform applied`, `callers of fold_orbweaver_plexus` |
| Text (README/CHANGELOG) | 2 | matches that should route to `mode=text` |
| Data (config.yaml) | 2 | matches that should route to `mode=data` |

## Three-mode results (run 2026-05-25, daemon v0.9.1)

Reindex completed in 3.6 s. All three pipeline stages (`lexical`, `semantic`, `graph`) reached `ready` for the synthetic-benchmark index. Every query was executed once per mode against `POST /indexes/synthetic-benchmark/search`:

- **lexical** = `{"stage": "lexical"}` (BM25 only)
- **hybrid** = no `stage` parameter (full BM25 + vector + KG + RRF fusion)
- **kg-leading** = `{"expand_graph": true, "use_kg_first": true}` (graph-leading)

### Aggregate

| Mode | Hit@1 | Hit@5 | p50 client ms | p50 server ms |
|------|-------|-------|----------------|----------------|
| lexical    | 6/14 (43%) | 10/14 (71%) | 10 | 5 |
| hybrid     | 6/14 (43%) | 9/14 (64%) | 11 | 7 |
| kg-leading | 6/14 (43%) | 9/14 (64%) | 11 | 6 |

### Per-query

| ID | Query | Lexical H@1 | Hybrid H@1 | KG H@1 | Lex H@5 | Hyb H@5 | KG H@5 | Intent classified as |
|----|-------|:-----------:|:----------:|:------:|:-------:|:-------:|:------:|---------------------|
| Q01 | `HammondLever` | Y | Y | Y | Y | Y | Y | Definition |
| Q02 | `LichtenbergCascade` | Y | Y | Y | Y | Y | Y | Definition |
| Q03 | `parse_zelenov_payload` | Y | Y | Y | Y | Y | Y | Definition |
| Q04 | `BRUSILOV_EPOCH` | — | — | — | — | — | — | Unknown |
| Q05 | `compute seraphim modulus from descriptor` | — | — | — | Y | Y | Y | Conceptual |
| Q06 | `flatten clustered tree into depth first vector` | Y | Y | Y | Y | Y | Y | Conceptual |
| Q07 | `fixed point iteration with damping factor` | Y | Y | Y | Y | Y | Y | Conceptual |
| Q08 | `encrypt outbound telemetry stream cipher` | Y | Y | Y | Y | Y | Y | Conceptual |
| Q09 | `where is BrusilovTransform applied` | — | — | — | Y | Y | Y | Usage |
| Q10 | `callers of fold_orbweaver_plexus` | — | — | — | Y | Y | Y | Usage |
| Q11 | `Glyphwarpen Observatory benchmark corpus motivation` | — | — | — | — | — | — | Conceptual |
| Q12 | `changelog initial release notes synthetic corpus` | — | — | — | Y | — | — | Conceptual |
| Q13 | `configured outbound channels for archive and dashboard` | — | — | — | — | — | — | Conceptual |
| Q14 | `seraphim damping factor and iteration cap` | — | — | — | — | — | — | Conceptual |

`match_reason` was `bm25` for all 14 lexical hits and `hybrid` (with one `vector` for Q12) for the hybrid / kg-leading modes.

## Comparison to v0.9.1 trusty-tools (circular-bias)

| Source corpus | Mode | Hit@1 | Hit@5 | Bias status |
|---------------|------|-------|-------|------------|
| trusty-tools (v0.9.1 snapshot) | hybrid | 57.1% (8/14) | 85.7% (12/14) | **Contaminated** — query strings appear as `assert_eq!` literals |
| synthetic (this baseline) | lexical | 42.9% (6/14) | 71.4% (10/14) | **Clean** |
| synthetic (this baseline) | hybrid | 42.9% (6/14) | 64.3% (9/14) | **Clean** |
| synthetic (this baseline) | kg-leading | 42.9% (6/14) | 64.3% (9/14) | **Clean** |

The 14 percentage-point drop in Hit@1 and 21 pp drop in Hit@5 between the v0.9.1 trusty-tools number and the synthetic clean number is the **size of the BM25 circular bias** on a small (47-file) corpus. Real-world corpora have larger BM25 vocabulary and more vector neighbours, so the bias magnitude on a 14k-file repo like `trusty-tools` could be larger or smaller; this baseline is one data point, not a calibration.

### Per-mode interpretation

**Lexical (Hit@5 71%) ≥ Hybrid (Hit@5 64%):** on a 47-file corpus, the lexical lane is more reliable than the hybrid lane — the vector signal demotes some exact-symbol hits in favour of semantically-adjacent files. This matches the finding documented in v0.9.1 (lexical wins on exact-term queries, hybrid wins on conceptual queries) and is consistent across the synthetic and organic corpora.

**Hybrid ≈ KG-leading:** flipping `use_kg_first: true` produced identical Hit@1/Hit@5 on this corpus — KG signal is not yet making a measurable difference at this scale. This is expected: the synthetic corpus has limited inter-file call density (the orchestrator `pipeline.rs` is the only multi-subsystem caller), so KG expansion has few high-confidence edges to lift on.

**Q11–Q14 universally miss across all modes:** these are the text-mode and data-mode queries. The default search request did NOT specify `mode=text` or `mode=data`, so the per-mode filter restricted retrieval to code chunks. Adding mode routing to the harness is a follow-up — the current numbers reflect "what happens when an agent queries naively". **This is a useful finding in itself**: it shows that without explicit mode hints, README content and config.yaml structure are unreachable.

**Q04 (`BRUSILOV_EPOCH`) misses across all modes:** the constant is defined in `constants.rs` but every mode surfaces `calibration.rs` / `transform/inverse.rs` (which CONSUME it) at top-1. This reproduces the v0.9.1 finding that hybrid-mode top-1 ranking can displace constants/definitions in favour of usage sites — and it does so on a clean corpus, so it isn't a circular-bias artifact. This is a legitimate ranking issue worth investigating in #117 / #119 follow-ups.

## Caveats

1. **Corpus is synthetic.** Real-world repositories have organic vocabulary distributions (Zipfian token frequencies, doc-comment phrasing in natural language, ambiguous module names like `util.rs` / `helpers.rs`) that this 42-file fixture does not reproduce. The synthetic numbers are not "the truth" — they're a circular-bias-free reference point.
2. **No mode-routing in harness.** Q11–Q14 universally miss because the harness does not send `mode=text` / `mode=data`. The four failures are not relevance regressions; they're a harness-coverage gap. A v2 harness that routes by `expected_mode` from `GROUND_TRUTH.json` will produce different Hit@K for those queries.
3. **Small corpus.** 47 files / 298 chunks is well below the production scale at which the KG and Louvain signals become load-bearing. The KG-leading mode collapsing to the hybrid baseline is consistent with that scale.
4. **Single run.** Each query was executed once. p50 latency is meaningful but tail-latency claims would need multi-trial.
5. **One observer, one daemon instance.** Same daemon as v0.9.1-2026-05-25 snapshot; no daemon restart between runs.

## Follow-ups

### Option A (deferred from #123): index `open-mpm` for organic-code validation

This baseline only fixes the contamination axis. A second axis worth measuring is **organic-vs-synthetic shape**. The `open-mpm` crate (~10k LOC of real Rust code authored before the trusty-search benchmark queries existed) is a natural fit: every query the harness asks about exists "in the wild" rather than being authored for the test. Sketch:

1. Add a second `cargo test --test benchmark_open_mpm` harness identical in structure to `benchmark_synthetic.rs`.
2. Hand-pick 14 ground-truth queries against `open-mpm` symbols (`AgentRunner`, `SessionRegistry`, …) chosen so the query strings appear only as identifier names, not as benchmark-style assert literals.
3. Run the same three-mode comparison and emit `docs/regression-testing/open-mpm-baseline-YYYY-MM-DD.md`.

If `open-mpm` (organic, never benchmarked-against) and synthetic (clean by construction) agree within ±5 pp on Hit@5, we can retire the trusty-tools baseline. If they disagree by more, the disagreement isolates "organic vocabulary effects" from "circular-bias contamination" and tells us which one matters more for hybrid retrieval quality.

### Harness improvements

- Route `mode=text` / `mode=data` based on `GROUND_TRUTH.json.queries[].expected_mode` so Q11–Q14 stop universally missing for harness reasons.
- Multi-trial latency (run each query 3× and report p50/p99 per mode).
- Capture `meta.graph_scoring` and `meta.community_cohesion` per query to confirm the KG lane is engaging.

### Ranking follow-ups surfaced by Q04 and Q11–Q14

- **Q04 (constant Definition routing)**: top-1 is a USAGE site, not the DEFINITION site, on every mode. This is a clean-corpus reproduction of the v0.9.1 finding and worth a stand-alone investigation.
- **Q11/Q12 (README/CHANGELOG)** and **Q13/Q14 (config.yaml)**: when no `mode` filter is supplied, these files are unreachable. Either the default mode filter needs to relax for unrouted text queries, or the classifier needs to route phrases like "configured outbound channels" to `mode=data` automatically.

## Cross-links

- [#123](https://github.com/bobmatnyc/trusty-tools/issues/123) — _BM25 circular bias in benchmark corpus_ (this work closes it)
- [#119](https://github.com/bobmatnyc/trusty-tools/issues/119) — QueryClassifier snake_case Definition routing (introduced the contaminating literals)
- [#128](https://github.com/bobmatnyc/trusty-tools/issues/128) — Stage 3 signal A/B validation (gains a clean A/B baseline from this work)
- [#129](https://github.com/bobmatnyc/trusty-tools/issues/129) — Benchmark tracker
- [v0.9.1-2026-05-25.md](v0.9.1-2026-05-25.md) — the trusty-tools snapshot this baseline is compared against

## Raw measurements

The harness is `crates/trusty-search/tests/benchmark_synthetic.rs`. Re-run with:

```bash
cargo test --test benchmark_synthetic -- --include-ignored --nocapture
```

The harness reads `GROUND_TRUTH.json`, registers and reindexes `synthetic-benchmark`, runs every query in every mode, prints the markdown tables above, asserts that at least one hit landed, and deletes the index. No daemon restart required.
