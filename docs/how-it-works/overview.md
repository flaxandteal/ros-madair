# Overview

## Summary

Rós Madair compiles Arches (alizarin) graph data into a **DuckDB + Parquet
substrate** that is served from a CDN or static host and queried without a
backend database. A reader compiles the typed query IR to DuckDB SQL, prunes row
groups on the Parquet zone-map, fetches only those via HTTP Range, filters
exactly, and hydrates the matches into a schema-shaped tree.

## The pipeline

```
alizarin graphs + resources + vocabularies (a data_dir)
        │  ros-madair-emit   (CLI)
        ▼
  tiles_<slug>.parquet     tile = row; promoted columns (concept_ids / q_ordered / geo_*)
                           whose row-group min/max stats ARE the zone-map; `data` = hydration JSON
  edges_<slug>.parquet     one row per link target — HasLink / OnLink / cited_by semijoin this
  concept_catalog.parquet  concept DFS intervals (hierarchy = a range join)
  manifest.json            layout + FORMAT_VERSION contract; signed snapshot_id
        │  ros-madair-duck (native) / DuckDB-WASM (browser)
        ▼
  compile the Query IR → DuckDB SQL (zone-map prune → exact filter, over HTTP Range)
  → hydrate matches into a schema-shaped JSON tree (overlay + cited_by aware)
  → verify the manifest signature before trusting a snapshot
```

## The artifacts

- **`tiles_<slug>.parquet`** — one row per tile. A field's storage class is a
  pure function of its datatype (`ros-madair-handlers`): concept → the
  `concept_ids` JSON-array column, ordered scalar (date) → `q_ordered`, geometry
  → `geo_*`, everything else → the `data` JSON blob for hydration. The promoted
  columns' per-row-group min/max stats in the Parquet footer **are** the
  zone-map — no separate index.
- **`edges_<slug>.parquet`** — one row per link target. `HasLink`, the `OnLink`
  path predicate, and `cited_by` all semijoin this columnar edge table.
- **`concept_catalog.parquet`** — concept DFS intervals, so a hierarchy query
  (`DescendantOrSelfOf`) is a range join.
- **`manifest.json`** — the layout + `FORMAT_VERSION` contract, carrying a signed
  `snapshot_id`. A reader refuses a version it does not implement, and verifies
  the attestation before trusting the snapshot.

## The read path

`ros-madair-query` defines the typed IR (`Concept`, `Range`, `Bbox`, `HasLink`,
`OnLink`, `OnTile`) — plus `ModelCatalog` (a discovery surface for NL→IR
authoring) and `explain` (IR → English). It is backend-agnostic.
`ros-madair-duck` is the compiler and reader:

- **compiles** the IR to DuckDB SQL over Parquet and **resolves** it to resource
  ids (row-group prune, then exact filter — including exact `ST_Intersects` where
  the spatial extension is available);
- **hydrates** matching tiles into a schema-shaped tree (the `hydrate` module,
  folded in from the retired `ros-madair-read` crate);
- composes **layered overlays** (a base plus on-device overlays, precedence per
  resource+nodegroup), across which an `OnLink` hop can traverse;
- answers **reverse traversal** (`cited_by`); and
- **verifies** each layer's signed content (`format::verify`), refusing a
  tampered snapshot.

## Direction: DuckDB + Parquet

The coarse/fine head+chunk read engine that used to sit here has been **replaced**
by this substrate — it was a reimplementation of what Parquet zone-maps + DuckDB
give natively (and, for spatial, a less complete one). What is genuinely
RM-specific is kept: layered overlay, reverse traversal, tile-graph hydration,
and snapshot signing. Full-text search stays with a separate inverted index
(Pagefind), because zone-maps have no order to prune text on.

See the
[README's Direction section](https://github.com/flaxandteal/ros-madair#direction-duckdb--parquet-substrate)
for the findings, the tile-row Parquet layout, nodegroup partitioning and
hierarchical ordering, egress governance, and the roadmap (the remaining slice is
the DuckDB-WASM browser runtime).
