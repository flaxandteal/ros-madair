# Plan: Multi-Layer Integration (retired)

!!! warning "Retired — superseded"
    The original implementation plan on this page targeted the **deleted v1
    stack** (`IndexBuilder`, `SparqlStore`, `IndexReader`, per-layer
    `summary.bin` / `pages/` / shadow pages, `!pred` reverse predicates, and
    code changes to `ros-madair-client` / `ros-madair-alizarin`). None of those
    crates, APIs, or file formats exist any more, so the step-by-step plan is
    obsolete and has been removed to avoid misleading a reader.

## The surviving requirement

The *concept* this plan served — **layered overlays** — is one of the three
things the current direction explicitly keeps (alongside reverse traversal and
Arches tile-graph hydration). It survives in the v2 read path:

- **Base + overlay composition** — a shipped base artifact plus on-device
  overlay artifacts, unified with precedence, so an overlay can shadow or extend
  the base without rebuilding it. Implemented and tested in
  `ros-madair-read` (`layers.rs`; see `crates/ros-madair-read/tests/layers.rs`).
- **Cross-layer reverse traversal** — `cited_by` returns citers across layers
  (`crates/ros-madair-read/tests/reverse.rs`).

## Where it is heading

Under the DuckDB + Parquet substrate, overlays become sibling Parquet artifacts
unified with a precedence rule in SQL, and reverse traversal becomes a join.
See the
[README's Direction section](https://github.com/flaxandteal/ros-madair#direction-duckdb--parquet-substrate)
for how the kept layers rehome onto the substrate.
