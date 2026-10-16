# Rós Madair

A static-file query engine for heritage (Arches) graph data. The
`ros-madair-emit` CLI compiles alizarin graph + resource data into a **DuckDB +
Parquet substrate** — tile-row Parquet with promoted, zone-mapped columns, a
columnar edge table, a concept catalog, and a signed manifest — served from a CDN
or static host and queried without a backend database. A reader compiles the
typed query IR to DuckDB SQL, fetches only the row groups it needs via HTTP
Range, and hydrates matches into a schema-shaped tree.

> **Status.** The v1 coarse/fine head+chunk read engine has been **deleted** (the
> substrate subsumes it). The `ros-madair-read` crate is gone (its tile-graph
> hydration folded into `ros-madair-duck`); `ros-madair-query` is now a
> backend-agnostic IR (no compiler); `ros-madair-format` is the snapshot contract
> + attestation verify. The browser runtime (DuckDB-WASM) has landed —
> `ros-madair-compile` (the pure IR→SQL compiler) builds for `wasm32` and
> `ros-madair-wasm` wraps it; see [Direction](#direction-duckdb--parquet-substrate).

## How It Works

The `ros-madair-emit` CLI compiles a `data_dir` (alizarin/Arches graphs +
resources + optional SKOS) into the substrate. A reader (`ros-madair-duck`
natively; DuckDB-WASM in the browser) compiles the `Query` IR to SQL, prunes row
groups on the zone-map, filters exactly, and hydrates the matches — no backend
database.

```
alizarin graphs + resources + vocabularies (a data_dir)
        │  ros-madair-emit   (CLI)
        ▼
  tiles_<graphid>.parquet  tile = row; only geo_* promoted (its row-group min/max
                         stats ARE the spatial zone-map); `data` = hydration JSON
  concepts_<graphid>.parquet  melted concept store (one row per node/concept_id)
  ordered_<graphid>.parquet   melted ordered-scalar store (dates/numbers), node-keyed
  edges_<graphid>.parquet  one row per link target — HasLink / OnLink / cited_by semijoin this
  concept_catalog.parquet  concept DFS intervals (hierarchy = a range join)
  manifest.json          layout + FORMAT_VERSION contract; signed snapshot_id
        │  ros-madair-duck (native) / DuckDB-WASM (browser)
        ▼
  compile the Query IR → DuckDB SQL (zone-map prune → exact filter, over HTTP Range)
  → hydrate matches into a schema-shaped JSON tree (overlay + cited_by aware)
  → verify the manifest signature before trusting a snapshot
```

## Crates

| Crate | Purpose |
|-------|---------|
| `ros-madair-handlers` | Datatype → index-class classification; the CLM `reference` handler |
| `ros-madair-format` | The snapshot **contract** (manifest types + `FORMAT_VERSION`, the shared snapshot-id derivation) and attestation **verify** (`format::verify`); held to `wasm32` |
| `ros-madair-emit` | CLI/library: compile a data_dir into the tile-row + edge Parquet substrate + concept catalog + manifest; `sign`/`verify`/`pubkey` subcommands |
| `ros-madair-query` | The typed `Query`/`Expr` IR (`Concept`/`Range`/`Bbox`/`HasLink`/`OnLink`/`OnTile`) + `ModelCatalog` (discovery) and `explain`. Backend-agnostic — no compiler |
| `ros-madair-compile` | **The** pure IR → DuckDB-SQL compiler (`compile_expr`/`compile_query`); no libduckdb, builds for `wasm32` |
| `ros-madair-duck` | Native executor of the compiled SQL over Parquet — **consumes `ros-madair-compile`** (exact spatial, links, DFS concept join, multi-hop `OnLink`, layered overlay), **tile-graph hydration**, `cited_by`, and reader-side snapshot **verify** |
| `ros-madair-wasm` | Browser runtime: wraps `ros-madair-compile` behind wasm-bindgen (npm `ros-madair`) so a page compiles the SAME SQL `duck` runs, over duckdb-wasm |
| `ros-madair-python` | PyO3 bindings (over `duck`) |

## Quick Start

### Build and test

```bash
cargo build --release
cargo test
```

### Emit static artifacts from a data_dir

```bash
cargo run -p ros-madair-emit --release -- <data_dir> <out_dir> [--base-uri URI]
```

The `data_dir` follows the alizarin/Clódóir convention:

```
<data_dir>/
  graphs/*.json          resource models
  resources/**/*.json    business data
  vocabularies/*.xml      SKOS (optional)
```

Output (`<out_dir>`, per-model files named by graph id):
`tiles_<graphid>.parquet`, `concepts_<graphid>.parquet`,
`ordered_<graphid>.parquet`, `edges_<graphid>.parquet`,
`concept_catalog.parquet`, `manifest.json`. Sign it with
`ros-madair-emit sign <out_dir> <key>` (attests the manifest `snapshot_id`);
`verify` / `pubkey` round out the attestation CLI.

### Runnable example: the whole flow in ~100 lines

```bash
cargo run -p ros-madair-duck --example substrate_end_to_end
```

Self-contained (no data files, no network): it writes a tiny `data_dir`, emits +
signs it, verifies the snapshot, opens it with `DuckReader`, runs `Range`/`Bbox`
queries from the IR, and hydrates a match into a JSON tree. Two more read-side
examples take an existing snapshot directory:
`--example cited-by` (reverse links) and `--example hydrate-perf`.

## Documentation

Full documentation — installation, quick start, how-it-works, and **live,
in-browser demos** (query, layers, roundtrip, and signature verification running
on duckdb-wasm) — lives in the separate
[`ros-madair-docs`](https://github.com/flaxandteal/ros-madair-docs) site
(Fumadocs on Next.js):

```bash
cd ../ros-madair-docs && npm install && npm run dev   # http://localhost:3000
```

The forward plan lives in [Direction](#direction-duckdb--parquet-substrate) above.

## Direction: DuckDB + Parquet substrate

Recorded from the investigation into whether Rós Madair's read engine earned
its keep. Short version: the coarse-prune-then-fine-scan **read mechanism** did
not — it was a reimplementation of what Parquet zone-maps + DuckDB give natively
— so it has been **replaced**. What is genuinely RM-specific (layered overlay,
reverse traversal, tile-graph hydration, snapshot signing) is kept and rehomed on
the substrate. This section keeps the rationale and the layout; the roadmap at the
end tracks what has landed.

### Why: the read engine was a reimplementation, verified

Both RM and DuckDB-over-Parquet serve static files off a CDN, fetch only what
is needed via HTTP Range, and prune coarse-then-fine. A differential harness
(`duckdb_equiv.rs`, later `duck`'s `parity.rs`) established this empirically by
running the old head backend and the duck backend over the same emitter rows;
with the head backend now deleted, `duck`'s `tests/{behavioral,descendant}.rs`
exercise the substrate directly. The investigation established:

- **Ranges / equality match exactly.** RM's `qvalue BETWEEN` on day-quantised
  dates == DuckDB `BETWEEN`. RM here is a hand-built SQLite index matching what
  Parquet derives for free from the file format.
- **Spatial diverges — RM is *less* correct.** RM's `resolve()` returns the
  bbox-overlap **superset**; its documented "client verifies exact intersection
  on hydrated tiles" fine step is **unimplemented anywhere in the repo** (only
  doc comments). DuckDB `ST_Intersects` is exact in one call. The emitted-Parquet
  path closes this: coarse bbox prune on the promoted columns, then exact
  `ST_Intersects` on the geometry parsed from the tile blob drops the false
  positive RM kept.
- **The remote/localization trick is native.** DuckDB `httpfs` over Parquet
  fetched **2.56% of a file** for a selective query (measured, byte-counting
  server), plus **column projection** RM's whole-tile chunks cannot do.

> ### ⚠️ Platform limitation: the exact spatial fine step is DESKTOP-ONLY
>
> The "exact `ST_Intersects`" above needs DuckDB's **`spatial` extension**, and
> that extension is **not available on mobile (aarch64-android)**:
>
> - `spatial` is a heavy **out-of-tree** extension that vendors GEOS + GDAL + PROJ.
>   It is **not** in the statically-linked amalgamation (only `json`, `parquet`,
>   `icu` are), and DuckDB publishes **no prebuilt `spatial.duckdb_extension` for
>   any android platform string** (its extension repo 404s for android). Building
>   one ourselves means cross-compiling GDAL for aarch64-android — a monolithic,
>   weeks-scale effort for a payload we do not need.
> - So the three `SpatialSource` modes resolve as: **`Auto`** (`INSTALL spatial;
>   LOAD spatial` over the network) is **dev/desktop only**; **`OfflineDir`** has
>   **no android binary to load**; **`None`** skips spatial and makes any
>   `Expr::Bbox` **error at query time**.
>
> **On mobile, `Expr::Bbox` must be COARSE-ONLY** — the bbox-overlap predicate on
> the promoted `geo_min/max_lng/lat` zone-map columns, with the `ST_Intersects`
> fine step **skipped**. This is a strict SUPERSET of true intersection (false
> positives kept, no false negatives), i.e. **recall-tolerant** — "places near
> here", never exact containment. If exact intersection is ever required on
> device, implement the fine step in **Rust `geo` (`geo::Intersects`)** over the
> geometry parsed from the tile blob — pure Rust, cross-compiles trivially — NOT
> the DuckDB extension. See `ros-madair-duck` `compile_expr`'s `Expr::Bbox` arm.

Zone-map pruning needs an *order* to prune on, so it applies to structured
fields (dates, coordinates, ids) and **not** to full text. That asymmetry is
why **Pagefind stays separate** — full-text wants an inverted index sharded for
HTTP, which Parquet is not. Use the structure whose layout matches the access
pattern.

### What is kept (Parquet does not hand you these)

- **Layered base + overlay** — on-device edits shadowing shipped data.
- **Reverse traversal** — `cited_by`.
- **Arches tile-graph hydration** — flat tiles → schema-shaped JSON tree.

These are rehomed to read their tiles from Parquet; their logic is unchanged.

### The layout

- **Tile = one row** — the finest unit of retrieval.
- **Row group = the RM "chunk"** — the unit DuckDB range-fetches and prunes.
- **Node-level index = promoted typed columns** (`q_ordered`, `concept_id`,
  `geo_*`); their per-row-group min/max stats in the Parquet footer *are* the
  zone-map. No `summary_quads`, no separate index build. Everything else stays
  in a `data` JSON blob for hydration — the same head/detail split, for free.
- **Locality = row order**, sorted by a **per-graph, index-time cluster key**.
  Default: a Z-order blend of geospatial-if-present ⊕ the resource descriptor
  name (a graph with no geometry collapses to name-order). This is the Parquet
  form of RM's Hilbert page assignment — `ORDER BY` in the writer, not a bespoke
  curve. Configurable per graph because different graphs have different needs.
- **Nodegroup = a partition axis** (Hive `nodegroup_id=<X>/` directories),
  orthogonal to the cluster sort. "Give me nodegroups X, Y, Z across N
  resources" reads **only those partitions**, skipping every other nodegroup's
  bytes.
- **Hierarchical nodegroups = DFS-interval (nested-set) order.** Each nodegroup
  gets a `pre`/`submax` interval (the same trick the v1 head used for
  concept hierarchies); sorting tiles by `ng_order` makes a whole subtree one
  contiguous range, so **"nodegroup X and its children" is a single range
  read**. An emitted `*.nodegroup_intervals.json` sidecar carries the bounds.

The unavoidable tension: one physical sort has one primary axis. Resource-first
clusters a resource's tiles (cheap hydration) but scatters a nodegroup;
nodegroup/DFS-first does the reverse. Partition at the boundary that matters and
order within it.

### Egress governance (why partitioning is worth more than reads)

In a single Parquet, `nodegroup_id` is opaque column bytes — filtering it at the
network edge would need a decode (a compute tier, losing "static"). **Partition
-by-nodegroup lifts `nodegroup_id` into the URL path**, so a hard rule like
"these nodegroups never leave the network" becomes a pure **L7 path allow-list**
(Envoy RBAC / CDN path rule / WAF) — default-deny, line-rate, no Parquet decode,
static Range requests intact, and structural rather than query-dependent (you
gate the object, not the query). Caveats: granularity is per-nodegroup; the
guarantee holds only if restricted content lives *solely* in its partition
(audit promoted columns, the denormalised `descriptor_name`, and links for
leaks); pair with an audience-scoped manifest; and the belt-and-braces option is
the emit-time **tier** (`exclude_nodegroups`) that never ships the bytes at all.

### Roadmap

- **Done — prune.** v1 island removed (~11.5k LOC: `quantize`/`summary_quads`/
  `page_file`/`hilbert`/`resource_map` and their bindings). v2 stack still green.
- **Done — slice 1 (tile-row Parquet emit).** `emit::parquet` is the emit
  back-half: per-graph cluster config, promoted columns, nodegroup partitioning,
  DFS hierarchical ordering + interval sidecar
  (`crates/ros-madair-emit/tests/parquet_emit.rs`). Verified against DuckDB —
  exact spatial fine step, partition pruning, subtree ranges.
- **Done — slice 2 (DuckDB query path).** `ros-madair-duck` compiles the
  `Query` IR to DuckDB SQL over Parquet: all `Expr` variants (`All`/`Any`/`Not` →
  `INTERSECT`/`UNION`/`EXCEPT`), `Concept` `Is` + `DescendantOrSelfOf` (catalog
  DFS-interval join), `Range`, exact per-node `HasLink`, and `Bbox` with an
  **exact `ST_Intersects` fine step**, degrading to the coarse superset where the
  spatial extension is unavailable. Bundled libduckdb (json+parquet static,
  offline). Verified by `tests/behavioral.rs` (every `Expr` variant +
  `count_records`) and `tests/descendant.rs`.
- **Done — slice 3 (hydration/overlay/`cited_by` on Parquet).** `duck`'s
  `hydrate_layers` composes base+overlay tiles read from the Parquet `data`
  column via the `hydrate` module (folded in from the retired `read` crate), and
  `cited_by`/`geo_points` do reverse link lookups over the `edges` table.
- **Done — path / multi-hop queries.** An additive edge table
  (`edges_<graphid>.parquet`, one row per link target) plus the `OnLink` path
  predicate: a cross-resource hop compiles to a leaf-first edge semijoin, nests
  for multi-hop chains, resolves **cross-model** (`resolve_ids_linked`), and
  crosses **layers** via `open_layers` — base+overlay composition of the
  `tiles`/`edges`/`concepts` views, so cross-layer traversal needs **no shadow
  records** (edges are id-based; unioning per-layer views reconnects a link in one
  layer to its target in another). Dot-qualified paths (`address.location`)
  resolve through the schema tree. `link_targets` is now consolidated onto the
  edge table — `HasLink`/`cited_by`/`geo_points` all query `edges`, and the
  per-tile JSON column is dropped (hydration reads `data`, links live only in
  `edges`). Covered by `tests/behavioral.rs` (OnLink, two-hop chain, cross-layer,
  cross-model, dotted paths, layered precedence) + the emit edge test.
  _Deferred:_ cardinality-n layer merge, and edge-side pruning (`src_node`
  partition / Bloom / dense ordinals).
- **Done — delete the old engine.** Removed `format`'s chunk framing, the emit
  head/chunk writer, the entire `ros-madair-read` crate (hydration folded into
  `duck`), and the `query` head-SQL compiler (the IR + `ModelCatalog` + `explain`
  stay). `ros-madair-python` ported onto `duck`. The snapshot-id derivation and
  reader-side **verify** now live in `format` (one copy, shared by writer and
  reader); `duck`'s `open_layers` verifies each layer and refuses a tampered one —
  closing the sign→verify loop (`duck/tests/verify.rs`).
- **Browser runtime → DuckDB-WASM.** The compiler now runs in the browser: the pure
  IR→SQL lowering was extracted into `ros-madair-compile` (no libduckdb; builds for
  `wasm32`), and `ros-madair-wasm` wraps it behind wasm-bindgen so a page compiles the
  SAME SQL `duck` runs natively and hands it to duckdb-wasm. CI builds the wasm target.
  The dead v1 `js/` glue (removed v1 WASM client) is deleted. Remaining: repoint the last
  two `ros-madair-docs` demos — concept-picker and layered-search — from hand-written SQL
  to the compiler (the site-references demo is already repointed).

## Caveats & architectural debt (to revise)

This section is deliberately blunt: the spatial layer and the Aonach Mór
"Responder" enrichment were explored iteratively and are **not settled**. Read
this before building on either.

- **Spatial ownership is UNDECIDED — and the shipped code conflicts with the
  recorded decision.** An earlier decision (memory `spatial-via-flatbush-not-
  geoclustering`) said spatial is *not* RM's job: a standalone R-tree (flatbush)
  owns geometry, RM composes the id-set via edges, and in-engine Z-order / `geo_*`
  / the DuckDB spatial extension are dropped. The code then went the *other* way:
  an in-engine **grid** (`grid_<graph>.parquet`) with a `GridBox` predicate that
  computes Morton cells via `ros_madair_handlers::cells_in_box` — RM doing
  geometry. A review panel scored grid (A) vs flatbush-delegate (B) and landed on
  **defer**: the distinguishing numbers are unmeasurable until footprints are real
  polygons (see next point). `GridBox` ships as the **interim** spatial predicate
  for the live 763k demo; whether it stays, is replaced by flatbush delegation, or
  becomes a *complementary* design (flatbush returns exact ids, RM's grid bounds a
  faceted search, intersect) is an **open owner decision**, not a settled one.
- **Footprints are POINTS, not polygons.** `emit_buildings.py` writes a
  single-point `FeatureCollection`; footprints are meant to be polygon/
  multipolygon. This shortcut makes the grid trivially correct on the current data
  and **masks every real spatial trade-off** — the panel's verdict is explicitly
  blocked on this. *Nothing honest about the spatial choice can be decided until
  the emitter writes real geometry.* The `emit` bbox→cells fan-out exists and is
  superset-correct for polygons, but is dormant on points.
- **Shelved ideas (do not resurrect without a measured reason):** a dense
  integer-ordinal shared between flatbush and the RM stores (brittle positional
  coupling — a silent-wrong-building risk in a safety path; only revisit if a
  measured byte problem forces it, and prefer an explicit id map to insertion
  order). A second review-panel re-run (more deliberation on point data the first
  panel already declared undiscriminating).
- **Known clean-up both spatial options agree on, NOT yet done:** drop the DuckDB
  spatial extension dependency + the legacy `Expr::Bbox`/`ST_Intersects` fine step
  + `geo_*` promotion. This is the real offline win and is orthogonal to the
  grid-vs-flatbush choice.
- **`GridBox` has a `MAX_BOX_CELLS` cap** that hard-errors (loudly, never a silent
  miss) on a near-regional query box; a real hazard-footprint size distribution
  was never checked against it.
- **The Aonach Mór "Responder" enrichment is HALF-BUILT.** The building model
  gained `insured_value` (TIV) + `capacity` nodes and emit writes them, but the
  live 763k substrate predates that and the rest is unwritten: organisations,
  person→building keyholder/contact links, hazard footprints, exposure, and
  insurance policies (generators planned per the port plan; models exist as CSV).
  The intended demo beats — field flow (tap→facilities+capacity+contacts, two
  **signed** owner-slices composed in-browser), the insurer **default-deny window**
  + **ΣTIV accumulation**, and the **break-it** beat (over-reach→deny is browser-
  doable; tamper→refuse is the native/app's job) — are the deliverable; spatial is
  only a pre-filter and is *not* on that critical path.
- **Live demos:** `/docs/aonach-mor` is the reliable tabbed resilience demo
  (sample + chunked "full city"); `/docs/aonach-763k` is the interim grid-based
  scale demo on the 763k substrate. The docs page proves *capability*, not the
  crypto-trust mechanic (which lives in the app).

## Dependencies

Rós Madair depends on [alizarin-core](https://github.com/flaxandteal/alizarin)
for Arches graph and tile data structures. The alizarin repository must be
checked out as a sibling directory (see the
[installation docs](https://github.com/flaxandteal/ros-madair-docs) for details).

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

Copyright (C) 2026 Flax & Teal Limited.
