---
hide:
  - navigation
  - toc
---

<div class="rm-hero" markdown>

# Rós Madair

<p class="rm-subtitle">Static-file queries over heritage graph data</p>

<p class="rm-tagline">
Compile Arches graph data into static artifacts served from a CDN, and query
them by fetching only the fragments each query needs — no server-side database,
no query endpoint.
</p>

<div class="rm-actions">
  <a href="getting-started/quickstart/" class="rm-btn-primary">Get Started</a>
  <a href="how-it-works/overview/" class="rm-btn-secondary">How It Works</a>
</div>

</div>

<div class="rm-accent-line"></div>

<div class="rm-stats" markdown>

<div class="rm-stat">
  <div class="rm-stat-value">0</div>
  <div class="rm-stat-label">backend databases</div>
</div>

<div class="rm-stat">
  <div class="rm-stat-value">CDN</div>
  <div class="rm-stat-label">static-file hosting</div>
</div>

<div class="rm-stat">
  <div class="rm-stat-value">Arches</div>
  <div class="rm-stat-label">heritage graph source</div>
</div>

</div>

<div class="rm-features" markdown>

<div class="rm-feature" markdown>

### Static-File Architecture

Artifacts live as flat files on any static host — S3, GitHub Pages, a CDN, or a
local `python -m http.server`. No database, no backend process, no API server.

</div>

<div class="rm-feature" markdown>

### Surgical Data Fetching

A query prunes Parquet row groups on their zone-map, then fetches only those via
HTTP Range requests (with column projection) — not the whole dataset.

</div>

<div class="rm-feature" markdown>

### Zone-Map + Tile Detail

Promoted, typed columns (concept / date / geo) carry per-row-group min/max stats
that *are* the index; a `data` JSON blob per row carries the tile detail that
hydration turns into a schema-shaped tree.

</div>

<div class="rm-feature" markdown>

### Locality-Clustered

Tiles are ordered by a per-graph cluster key (geospatial + descriptor name) so
that geographically and semantically similar data sits together, keeping the
fetched working set small.

</div>

</div>

---

## What is Rós Madair?

**Rós Madair** (Irish: *rose madder*) is a companion to
[Alizarin](https://github.com/flaxandteal/alizarin) — an ORM for
[Arches](https://www.archesproject.org/) heritage data management systems.
Where Alizarin provides a TypeScript/Rust SDK for working with Arches graphs
live, Rós Madair provides static, pre-built artifacts that can answer queries
without a running server.

The name follows the pigment theme: alizarin crimson and rose madder are
closely related red pigments, both derived from the madder root. Rós Madair
is the static, pre-ground pigment to Alizarin's live colour mixing.

### The Problem

Heritage datasets often contain tens or hundreds of thousands of records.
Standing up a full query backend (a triplestore, a database + API) requires
server infrastructure, ongoing maintenance, and non-trivial cost. For
read-only public datasets — museum collections, monument registries,
archaeological surveys — that is overkill.

### The Approach

The `ros-madair-emit` CLI compiles a data directory of graphs, resources, and
vocabularies into a **DuckDB + Parquet substrate**:

1. **`tiles_<slug>.parquet`** — one row per tile, with promoted typed columns
   (concept / date / geo) whose row-group stats are the zone-map, and a `data`
   JSON blob for hydration.
2. **`edges_<slug>.parquet`** — one row per link target (the columnar edge table
   links and reverse traversal semijoin), plus a **`concept_catalog.parquet`**
   for concept-hierarchy range joins.
3. **`manifest.json`** — the layout + format-version contract, carrying a signed
   `snapshot_id`.

A reader (`ros-madair-duck` natively; DuckDB-WASM in the browser) compiles the
typed query IR to SQL, prunes row groups on the zone-map, fetches only those
(over HTTP Range), filters exactly, and hydrates the matches — overlay- and
reverse-traversal-aware — verifying the manifest signature first.

!!! note "Direction: DuckDB + Parquet"
    This substrate **replaced** the v1 coarse/fine head+chunk read engine; the
    layered overlay model, reverse traversal (`cited_by`), tile-graph hydration,
    and snapshot signing are kept. The remaining slice is the DuckDB-WASM browser
    runtime. See the
    [README's Direction section](https://github.com/flaxandteal/ros-madair#direction-duckdb--parquet-substrate)
    for the rationale and roadmap.

## Next Steps

<div class="rm-features" markdown>

<div class="rm-feature" markdown>

### [Installation](getting-started/installation.md)

Build the workspace, set up the alizarin sibling checkout, and run the tests.

</div>

<div class="rm-feature" markdown>

### [Quick Start](getting-started/quickstart.md)

Emit static artifacts from Arches data with the `ros-madair-emit` CLI.

</div>

<div class="rm-feature" markdown>

### [How It Works](how-it-works/overview.md)

The tile-row Parquet substrate, the zone-map read path, and what stays
RM-specific on top of it.

</div>

</div>
