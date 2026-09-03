# Rós Madair

A static-file SPARQL query engine for heritage data. Pre-built binary indexes
are served from CDNs or static file hosts and queried entirely in the browser
via WebAssembly — no backend database required.

Typical queries transfer ~2% of the total dataset (~540 KB on a 22 MB /
160K-resource index).

> **Status — architecture in transition (branch `prune/duckdb-substrate`).**
> The coarse/fine read engine described in *How It Works* below is being
> **replaced by a DuckDB + Parquet substrate**. The v1 page-index island
> (`core`/`builder`/`client`/`alizarin`/`napi`) has been removed; the v2
> head+chunk read path is next. See
> [Direction: DuckDB + Parquet substrate](#direction-duckdb--parquet-substrate)
> for the rationale, the layout, and the roadmap. Sections between here and
> there describe the engine being retired.

## How It Works

The `ros-madair-emit` CLI compiles alizarin (Arches) graph + resource data into
static artifacts. A reader queries a small indexed "head", fetches only the tile
fragments it needs, and hydrates them into a schema-shaped tree — no backend
database.

```
alizarin graphs + resources + vocabularies (a data_dir)
        │  ros-madair-emit   (CLI)
        ▼
  head.sqlite       indexed spine / concept / value / geo / link tables — the coarse index
  chunks/*.msgpack  content-hashed tile detail — the hydration payload
  manifest.json     layout + format-version contract
        │  ros-madair-read (native) / a browser consumer
        ▼
  query the head to select resources + chunks → fetch only those (HTTP Range)
  → hydrate tiles into a schema-shaped JSON tree (overlay + cited_by aware)
```

This coarse/fine read engine is what the
[Direction](#direction-duckdb--parquet-substrate) replaces with DuckDB + Parquet;
the hydration, overlay, and reverse-traversal layers on top of it are kept.

## Crates

| Crate | Purpose |
|-------|---------|
| `ros-madair-handlers` | Datatype → index-class classification; the CLM `reference` handler |
| `ros-madair-format` | On-disk artifact format — manifest + versioned chunk framing (WASM-safe) |
| `ros-madair-emit` | CLI: compile a data_dir into head + chunks + manifest (and, additively, tile-row Parquet — see Direction) |
| `ros-madair-query` | Head-schema query compiler (`Concept` / `Range` / `Bbox` / `HasLink` → SQL) |
| `ros-madair-read` | Native read path — resolve, hydrate, layered overlay, reverse traversal (`cited_by`) |
| `ros-madair-python` | PyO3 bindings (`compile_query`, `hydrate_*`) |

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

Output (`<out_dir>`): `head.sqlite`, `chunks/`, `manifest.json`. Tiers
(`--tier <name>:<config.json>` with `exclude_nodegroups` / `exclude_models`)
emit an audience-scoped artifact that omits restricted data entirely.

## Documentation

Full documentation is available in the `docs/` directory, covering
[installation](docs/getting-started/installation.md),
[quick start](docs/getting-started/quickstart.md), and an
[architecture overview](docs/how-it-works/overview.md). The forward plan lives
in [Direction](#direction-duckdb--parquet-substrate) above.

## Direction: DuckDB + Parquet substrate

A direction plan, recorded from an investigation into whether Rós Madair's
read engine earns its keep. Short version: the coarse-prune-then-fine-scan
**read mechanism** does not — it is a reimplementation of what Parquet
zone-maps + DuckDB give natively — so it is being replaced. What is genuinely
RM-specific is kept and rehomed on top of that substrate.

### Why: the read engine is a reimplementation, verified

Both RM and DuckDB-over-Parquet serve static files off a CDN, fetch only what
is needed via HTTP Range, and prune coarse-then-fine. The original differential
harness (`duckdb_equiv.rs`) established this empirically; now that the substrate
read path exists, that equivalence is carried by `ros-madair-duck`'s own
`tests/parity.rs` (same emitter, same rows, both backends). It established:

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
  gets a `pre`/`submax` interval (the same trick the head already uses for
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
- **Done — slice 1 (additive Parquet emit).** `emit::parquet` writes tile-row
  Parquet alongside the head/chunk writer: per-graph cluster config, promoted
  columns, nodegroup partitioning, DFS hierarchical ordering + interval sidecar
  (`crates/ros-madair-emit/tests/parquet_emit.rs`). Verified against DuckDB —
  exact spatial fine step, partition pruning, subtree ranges.
- **Done — slice 2 (DuckDB query path).** `ros-madair-duck` compiles the SAME
  `Query` IR to DuckDB SQL over Parquet: all `Expr` variants (`All`/`Any`/`Not` →
  `INTERSECT`/`UNION`/`EXCEPT`), `Concept` `Is` + `DescendantOrSelfOf` (catalog
  DFS-interval join), `Range`, exact per-node `HasLink`, and `Bbox` with an
  **exact `ST_Intersects` fine step** (the head never implemented it), degrading
  to the coarse superset where the spatial extension is unavailable. Bundled
  libduckdb (json+parquet static, offline). Verified by `tests/parity.rs`
  (parity with the emitter's rows) and `tests/behavioral.rs` (every `Expr`
  variant + `count_records`).
- **Done — slice 3 (hydration/overlay/`cited_by` on Parquet).** `duck`'s
  `hydrate_layers` composes base+overlay tiles read from the Parquet `data`
  column (reusing `read`'s tile→tree hydration), and `cited_by`/`geo_points` do
  reverse link lookups over the promoted `link_targets`.
- **Done — path / multi-hop queries.** An additive edge table
  (`edges_<slug>.parquet`, one row per link target) plus the `OnLink` path
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
- **Next — slice 4.** Delete the old engine (`format` chunks, `query` head SQL,
  the emit head/chunk writer, `read::resolve`) once callers move to `duck`;
  rewire the Python binding; browser runtime → DuckDB-WASM (the SQL `duck`
  already compiles); update CI and the `js/` glue (which still reference the
  removed WASM client).

## Dependencies

Rós Madair depends on [alizarin-core](https://github.com/flaxandteal/alizarin)
for Arches graph and tile data structures. The alizarin repository must be
checked out as a sibling directory (see
[installation docs](docs/getting-started/installation.md) for details).

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

Copyright (C) 2026 Flax & Teal Limited.
