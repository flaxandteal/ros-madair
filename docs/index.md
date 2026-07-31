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

A query consults a small indexed "head", then fetches only the tile fragments it
needs via HTTP Range requests — not the whole dataset.

</div>

<div class="rm-feature" markdown>

### Coarse Index + Tile Detail

An indexed head (spine / concept / value / geo / link) routes a query to the
resources and chunks that can match; the chunks carry the tile detail that
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
vocabularies into three artifacts:

1. **`head.sqlite`** — an indexed head (spine, concept, ordered-value, geo-bbox,
   and link tables): the coarse index a query plans against.
2. **`chunks/*.msgpack`** — content-hashed tile detail: the payload hydration
   turns back into a schema-shaped tree.
3. **`manifest.json`** — the layout and format-version contract every reader
   checks.

A reader queries the head to select the resources and chunks that can match,
fetches only those (over HTTP Range), and hydrates the tiles — overlay- and
reverse-traversal-aware.

!!! note "Direction: DuckDB + Parquet"
    The coarse/fine read engine above is being replaced by a DuckDB + Parquet
    substrate; the layered overlay model, reverse traversal (`cited_by`), and
    Arches tile-graph hydration are kept. See the
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

The head + chunks + manifest model, and where the DuckDB + Parquet direction
takes it.

</div>

</div>
