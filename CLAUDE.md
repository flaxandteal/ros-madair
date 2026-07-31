# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Rós Madair?

A static-file query engine for heritage (Arches) graph data. The
`ros-madair-emit` CLI compiles alizarin graph + resource data into static
artifacts (an indexed SQLite "head", content-hashed msgpack "chunks", and a
manifest) that are served from a CDN or static host and queried without a
backend database — a reader consults the head, fetches only the tile fragments
it needs via HTTP Range, and hydrates them into a schema-shaped tree.

> **Architecture in transition.** The coarse/fine read engine described below is
> being **replaced by a DuckDB + Parquet substrate**. The old v1 page-index
> stack (`ros-madair-core`, `-client`, `-builder`, `-alizarin`, `-napi`) has
> been **deleted**. See [Direction](#direction-duckdb--parquet-substrate) and
> the README's "Direction" section for the plan.

## Build Commands

```bash
cargo build --release            # build the workspace
cargo test --workspace           # run all tests
cargo test -p ros-madair-read    # test one crate
```

### Emit static artifacts

```bash
cargo run -p ros-madair-emit --release -- <data_dir> <out_dir> [--base-uri URI] [--tier <name>:<config.json>]
```

`data_dir` layout: `graphs/*.json` (resource models), `resources/**/*.json`
(business data), `vocabularies/*.xml` (SKOS, optional). Output: `head.sqlite`,
`chunks/`, `manifest.json`.

### Python bindings

```bash
maturin develop -m crates/ros-madair-python/Cargo.toml   # compile_query, hydrate_*
```

### Documentation site

```bash
pip install -r requirements-docs.txt   # zensical (MkDocs successor)
zensical serve                         # local preview
zensical build --clean                 # output in site/
```

## Architecture

Six-crate Rust workspace (the "v2 static-assets" stack):

| Crate | Purpose |
|-------|---------|
| `ros-madair-handlers` | Datatype → index-class classification; the CLM `reference` handler |
| `ros-madair-format` | On-disk artifact format — manifest + versioned chunk framing (held to `wasm32`) |
| `ros-madair-emit` | CLI/library: compile a data_dir into head + chunks + manifest |
| `ros-madair-query` | Head-schema query compiler (`Concept`/`Range`/`Bbox`/`HasLink` → SQL) |
| `ros-madair-read` | Native read path — resolve, hydrate, layered overlay, reverse traversal (`cited_by`) |
| `ros-madair-python` | PyO3 bindings |

`emit`, `query`, `read`, and `python` depend on `format` and/or `handlers`; none
of them depend on each other in a cycle.

### Data flow

```
alizarin graphs + resources + vocabularies (data_dir)
        │  ros-madair-emit
        ├─► head.sqlite      spine / concept_tags / value_tags / geo_bbox / link tables (coarse index)
        ├─► chunks/*.msgpack content-hashed tile detail (hydration payload)
        └─► manifest.json    layout + FORMAT_VERSION contract
                │  ros-madair-read
                ▼
        query the head → select resources + chunks → fetch only those (HTTP Range)
        → hydrate tiles into a schema-shaped tree (overlay + cited_by aware)
```

### Key crate internals

- `ros-madair-emit`: `input` (graph/resource loading), `head` (SQLite schema +
  concept/value/geo/link indexing, concept DFS intervals), `chunks` (msgpack
  chunk writer), `geo` (bbox extraction), `locality` (Hilbert cluster order),
  `closure`/`composability`/`manifest`, and `parquet` (the additive tile-row
  Parquet writer — first slice of the substrate).
- `ros-madair-read`: `lib` (open head, resolve, hydrate entry points), `layers`
  (multi-layer overlay composition + reverse traversal).
- `ros-madair-query`: the typed `Expr`/`Query` IR and its compilation to head SQL.

### Key design decisions

- **Datatype-driven index class.** A field's storage/class is a pure function of
  its datatype (`datatype_index_spec`), computed identically by emit and query:
  concept → `concept_tags`, ordered scalar (date) → `value_tags`, geometry →
  `geo_bbox`, link → coarse `chunk_link_summary`/`reverse_links`, everything else
  → detail-only in the chunks.
- **Coarse predicates return supersets.** Link (`HasLink`) and spatial (`Bbox`)
  filters over-approximate — the head answers a candidate set with no false
  negatives, and the consumer is expected to verify exactly on hydrated tiles.
  Negating a coarse predicate is a typed error (you cannot soundly negate an
  over-approximation).
- **Format-version gating.** `FORMAT_VERSION` is stamped into the head
  (`PRAGMA user_version`), the manifest, and every chunk's framing header; a
  reader refuses a version it does not implement rather than misreading.
- **Content-hashed chunks.** Chunk bytes are content-hashed and feed the
  snapshot id; the msgpack framing is frozen.

## Direction: DuckDB + Parquet substrate

An investigation established that the coarse-prune-then-fine-scan **read
mechanism** is a reimplementation of Parquet zone-maps + DuckDB — and for
spatial a less complete one (the exact-intersection fine step is unimplemented;
`resolve()` returns the bbox superset). The plan:

- **Replace** the read engine (`format` chunks, `query` head-SQL, the `emit`
  head/chunk writer, `read::resolve`) with **DuckDB + Parquet**. Tile = row =
  finest retrieval unit; RM chunk → Parquet **row group**; node-level index →
  **promoted typed columns** whose row-group min/max stats are the zone-map; RM
  Hilbert locality → an `ORDER BY` cluster key (per-graph, index-time config;
  default geospatial ⊕ descriptor name). Nodegroup is a **partition** axis (and
  lifts `nodegroup_id` into the URL path, enabling egress governance by L7 path
  allow-list); hierarchical nodegroups get **DFS-interval** ordering so a subtree
  is one range read.
- **Keep** what Parquet does not give: layered **base+overlay**, reverse
  traversal (`cited_by`), Arches **tile-graph hydration**. **Pagefind stays
  separate** for full-text (zone-maps have no order to prune text on).
- **Status:** v1 deleted; slice 1 (additive tile-row Parquet emit) built in
  `ros-madair-emit`'s `parquet` module + tests. Slices 2–4 (DuckDB query path,
  rehome hydration/overlay/cited_by, delete the old engine, browser →
  DuckDB-WASM) pending.

See the README's Direction section for the full findings, the differential
harness (`crates/ros-madair-read/tests/duckdb_equiv.rs`), and the roadmap.

## External dependency: alizarin-core

All crates depend on `alizarin-core` (heritage graph/tile ORM) via a relative
path. The alizarin repo must be checked out as a sibling alongside the parent
`magic/` directory.

## Example: Aonach Mór

`example/aonach_mor/` is a city-resilience demo built on real Chennai geography
with fictional labels, with a Python build pipeline generating Arches resource
models and business data.

## CI

The `deploy-pages.yml` workflow and the `js/` glue still reference the removed
v1 WASM client (`ros-madair-client`/`-alizarin`/`-napi`); the browser runtime is
slated to become DuckDB-WASM (substrate slice 4), and CI/js are to be reworked
then.

## License

AGPL-3.0-or-later (Flax & Teal Limited).
