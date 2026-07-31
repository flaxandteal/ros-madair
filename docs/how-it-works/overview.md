# Overview

## Summary

Rós Madair compiles Arches (alizarin) graph data into static artifacts that are
served from a CDN or static host and queried without a backend database. A query
consults a small indexed head, fetches only the tile fragments it needs via HTTP
Range requests, and hydrates them into a schema-shaped tree.

## The pipeline

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

## The artifacts

- **`head.sqlite`** — the coarse index. A field's storage class is a pure
  function of its datatype (`ros-madair-handlers`): concept → `concept_tags`,
  ordered scalar (date) → `value_tags`, geometry → `geo_bbox`, link →
  `chunk_link_summary` / `reverse_links`, everything else → detail-only. This is
  what a query plans against.
- **`chunks/*.msgpack`** — the detail. Tiles are grouped into content-hashed
  chunks; a chunk is the unit fetched and decoded. Everything not head-indexed
  lives here for hydration.
- **`manifest.json`** — the layout and format-version contract. A reader refuses
  a format version it does not implement rather than misreading drifted fields.

## The read path

`ros-madair-query` compiles a typed query (`Concept`, `Range`, `Bbox`,
`HasLink`) to SQL over the head schema. `ros-madair-read`:

- **resolves** the query to a set of resource ids;
- **hydrates** their tiles into a schema-shaped tree;
- composes **layered overlays** (a base artifact plus on-device overlays, with
  precedence); and
- answers **reverse traversal** (`cited_by`).

Some predicates are *coarse* — link and spatial-bbox filters over-approximate
(they admit false positives), so the head returns a candidate set that the
consumer is expected to verify exactly on the hydrated tiles.

## Direction: DuckDB + Parquet

An investigation established that the coarse-prune-then-fine-scan **read
mechanism** here is a reimplementation of what Parquet zone-maps + DuckDB give
natively — and, for spatial, a less complete one (the exact-intersection fine
step is unimplemented). The direction is therefore to **replace the read engine
with a DuckDB + Parquet substrate** and keep only what Parquet does not hand you:
the layered overlay model, reverse traversal (`cited_by`), and Arches
tile-graph hydration. Full-text search stays with a separate inverted index
(Pagefind), because zone-maps have no order to prune text on.

See the
[README's Direction section](https://github.com/flaxandteal/ros-madair#direction-duckdb--parquet-substrate)
for the findings, the tile-row Parquet layout, nodegroup partitioning and
hierarchical ordering, egress governance, and the roadmap.
