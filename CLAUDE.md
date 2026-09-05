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
cargo test -p ros-madair-duck    # test one crate
```

### Emit static artifacts

```bash
cargo run -p ros-madair-emit --release -- <data_dir> <out_dir> [--base-uri URI]
```

`data_dir` layout: `graphs/*.json` (resource models), `resources/**/*.json`
(business data), `vocabularies/*.xml` (SKOS, optional). Output: `tiles_<slug>.parquet`,
`edges_<slug>.parquet`, `concept_catalog.parquet`, `manifest.json`. Sign a
snapshot with `ros-madair-emit sign <dir> <key>` (attests the manifest
`snapshot_id`); `verify`/`pubkey` round it out.

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

Six-crate Rust workspace. The v1 head/chunk read engine has been **deleted**
(slice 3/4); the substrate is now the only read path:

| Crate | Purpose |
|-------|---------|
| `ros-madair-handlers` | Datatype → index-class classification; the CLM `reference` handler |
| `ros-madair-format` | The snapshot **contract** (manifest types + `FORMAT_VERSION`) and attestation **verify** types; held to `wasm32` so a reader can parse + verify without linking the emitter |
| `ros-madair-emit` | CLI/library: compile a data_dir into the tile-row + edge Parquet substrate + concept catalog + manifest; `sign`/`verify`/`pubkey` attest the manifest `snapshot_id` |
| `ros-madair-query` | The typed `Query`/`Expr` IR (`Concept`/`Range`/`Bbox`/`HasLink`/`OnLink`/`OnTile`) + the `ModelCatalog` discovery surface and `explain` (IR → English). Backend-agnostic: no compiler. |
| `ros-madair-duck` | The `Query` IR → DuckDB SQL over tile-row Parquet (exact spatial, exact links, concept-catalog DFS join, `OnLink` multi-hop path predicate over an edge table, layered base+overlay), **plus tile-graph hydration** (`hydrate` module, folded in from the retired `read` crate), reverse traversal (`cited_by`), and reader-side snapshot **verify** (`open_layers` refuses a tampered layer) |
| `ros-madair-python` | PyO3 bindings (over `duck`) |

`emit`, `query`, and `python` depend on `format` and/or `handlers`; `duck`
depends on `query` (the IR), `handlers` (the `reference` handler for hydration),
and `format` (reader-side verify). None form a cycle. `query` and `duck` share
**one IR**; `duck` is its only compiler.

### Data flow

```
alizarin graphs + resources + vocabularies (data_dir)
        │  ros-madair-emit
        ├─► tiles_<slug>.parquet   tile = row; promoted columns (concept_id/q_ordered/geo_*)
        │                          whose row-group min/max stats ARE the zone-map; `data` = hydration JSON
        ├─► edges_<slug>.parquet   one row per link target (the edge table HasLink/OnLink/cited_by semijoin)
        ├─► concept_catalog.parquet  concept DFS intervals (hierarchy = a range join)
        └─► manifest.json          layout + FORMAT_VERSION contract; signed snapshot_id
                │  ros-madair-duck
                ▼
        compile the Query IR → DuckDB SQL over the Parquet (row-group zone-map prune → exact filter)
        → hydrate matching tiles into a schema-shaped tree (overlay + cited_by aware)
```

### Key crate internals

- `ros-madair-emit`: `input` (graph/resource loading), `geo` (bbox extraction),
  `closure` (SKOS + concept closure), `parquet` (the tile-row writer — streams
  through a DuckDB staging table + `COPY … ORDER BY` cluster key — plus the
  columnar edge table `edges_<slug>.parquet`, one row per link target, that the
  path/multi-hop query compiler semijoins), `manifest` (artifact hashing +
  snapshot id), `attest` (build-time signing over the manifest).
- `ros-madair-query`: the typed `Expr`/`Query` IR (incl. `OnLink`/`OnTile`), the
  `ModelCatalog` discovery surface (paths → predicate family, for NL→IR
  authoring), and `explain` (IR → English). The v1 head-SQL compiler is still
  present but unused — nothing consumes it; it is slated for removal.
- `ros-madair-duck`: `DuckReader` + `compile_expr` — the only compiler for that
  same IR, lowering `Concept`/`Range`/`Bbox`/`HasLink`/`OnLink`/`All`/`Any`/`Not`
  to DuckDB SQL over the tile-row Parquet (`resolve_ids`, `count_records`), plus
  `hydrate_layers`/`cited_by`/`geo_points` read from the Parquet `data` column.
  `OnLink` compiles to an edge-table semijoin and nests for multi-hop chains;
  `resolve_ids_linked` supplies extra models for cross-model hops; `open_layers`
  composes base+overlay `tiles`/`edges`/`concepts` views so a query — and a hop —
  crosses layers. Dot-qualified paths (`address.location`) resolve through the
  schema tree. The `hydrate` module (folded in from the retired `read` crate)
  turns recovered tiles into a schema-shaped, label-resolved JSON tree via
  alizarin's Display-mode tree builder — the part Parquet does not give. Bundled
  libduckdb (json+parquet static, offline); spatial loads from a local
  `.duckdb_extension`.

### Key design decisions

- **Datatype-driven index class.** A field's storage/class is a pure function of
  its datatype (`datatype_index_spec`), computed identically by emit and query:
  concept → `concept_id` promoted column, ordered scalar (date) → `q_ordered`,
  geometry → `geo_*`, link → the `edges` table, everything else → detail-only in
  the tile's `data` JSON. Promoted columns' row-group min/max stats ARE the
  zone-map — no separate index.
- **Exact filters, no coarse superset.** DuckDB prunes row groups by the zone-map
  then filters exactly (`ST_Intersects` for spatial, per-node link check), so a
  result is the exact set — the v1 head's coarse-superset-then-verify dance is
  gone. (`Bbox`/`HasLink` still cannot be soundly negated over the pruned scan,
  so a coarse negation remains a typed error.)
- **Format-version gating.** `FORMAT_VERSION` lives in the manifest; a reader
  refuses a version it does not implement rather than misreading.
- **Signed snapshot id.** The `snapshot_id` is a hash over every artifact's hash
  plus the (id-excluded) manifest; the derivation lives in `format` (one copy,
  shared by writer and reader). `emit sign` attests it; `duck`'s `open_layers`
  recomputes it from the files via `format::verify` and refuses a layer whose
  signed content was altered (unsigned/local builds still open).

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
- **Status:**
  - **Slice 1 — done.** Additive tile-row Parquet emit in `ros-madair-emit`'s
    `parquet` module (+ tests).
  - **Slice 2 — done.** `ros-madair-duck`: the `Query` IR → DuckDB SQL over
    tile-row Parquet (all `Expr` variants, exact spatial via `ST_Intersects`,
    exact per-node `HasLink`, `DescendantOrSelfOf` as a concept-catalog DFS-interval
    join), and hydrate/overlay/`cited_by` rehomed on Parquet. Verified by
    `crates/ros-madair-duck/tests/{behavioral,descendant}.rs`.
  - **Slice 2.5 — path / multi-hop queries — done.** An additive edge table
    (`edges_<slug>.parquet`, emit) + the `OnLink` predicate (query IR) compiled to
    leaf-first edge semijoins (duck): cross-resource, nested for multi-hop chains,
    cross-model (`resolve_ids_linked`), and cross-layer via `open_layers`
    (base+overlay composition of the `tiles`/`edges`/`concepts` views — so
    cross-layer traversal needs **no shadow records**). Dot-qualified paths also
    landed, and `link_targets` is now consolidated onto the edge table
    (`HasLink`/`cited_by`/`geo_points` all query `edges`; the per-tile JSON column
    is dropped). Deferred: cardinality-n layer merge, edge-side pruning (`src_node`
    partition / Bloom / dense ordinals), and per-node concept promotion (a
    nodegroup with two concept nodes).
  - **Slice 3 — done.** The old engine is deleted: `format`'s chunk framing, the
    emit head/chunk writer (the `head`/`chunks`/`composability`/`locality`
    modules), the entire `ros-madair-read` crate (its tile-graph **hydration**
    folded into `ros-madair-duck`'s `hydrate` module; resolve/overlay/`cited_by`
    already lived in `duck`), and the `query` head-SQL compiler (the `Query`/`Expr`
    IR + `ModelCatalog` + `explain` stay; `duck` is the only compiler).
    `ros-madair-python` was ported onto `duck`. `format` is trimmed to the manifest
    **contract** + attestation **verify** types, and the snapshot-id derivation now
    lives there (the ONE copy the writer and reader share). `duck`'s `open_layers`
    verifies each layer via `format::verify` and refuses a tampered one — closing
    the sign→verify loop end-to-end.
  - **Slice 4 — pending.** Browser runtime → DuckDB-WASM (the same SQL `duck`
    compiles, run in the browser; the `ros-madair-docs` demos already do this by
    hand pending a WASM/JS path for the compiler).

See the README's Direction section for the full findings and the roadmap. The
old differential harnesses (`duckdb_equiv.rs`, then `duck`'s `parity.rs`) tested
the duck backend against the v1 head backend; with the head backend deleted,
`duck`'s `tests/{behavioral,descendant}.rs` exercise it directly.

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
