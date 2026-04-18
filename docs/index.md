---
hide:
  - navigation
  - toc
---

<div class="rm-hero" markdown>

# Rós Madair

<p class="rm-subtitle">Browser-side SPARQL over static files</p>

<p class="rm-tagline">
Query heritage graph data directly from a CDN. No server-side database,
no SPARQL endpoint — just static files and a WASM client that plans,
fetches, and executes queries in the browser.
</p>

<div class="rm-actions">
  <a href="getting-started/quickstart/" class="rm-btn-primary">Get Started</a>
  <a href="how-it-works/overview/" class="rm-btn-secondary">How It Works</a>
</div>

</div>

<div class="rm-accent-line"></div>

<div class="rm-stats" markdown>

<div class="rm-stat">
  <div class="rm-stat-value">160K</div>
  <div class="rm-stat-label">resources queryable</div>
</div>

<div class="rm-stat">
  <div class="rm-stat-value">~2%</div>
  <div class="rm-stat-label">data transferred per query</div>
</div>

<div class="rm-stat">
  <div class="rm-stat-value">0</div>
  <div class="rm-stat-label">server-side computation</div>
</div>

</div>

<div class="rm-features" markdown>

<div class="rm-feature" markdown>

### Static-File Architecture

All index data lives as flat binary files on any static host — S3, GitHub
Pages, a CDN, or a local `python -m http.server`. No database, no backend
process, no API server.

</div>

<div class="rm-feature" markdown>

### Surgical Data Fetching

HTTP Range requests fetch only the predicate blocks needed for each query.
A typical query over 160K resources transfers ~540 KB — roughly 2% of
the total 22 MB dataset.

</div>

<div class="rm-feature" markdown>

### Summary-Driven Planning

A page-level summary index (loaded once at init) tells the query planner
exactly which pages contain relevant data. Multi-predicate queries benefit
from page-set intersection, cutting fetches by 60–70%.

</div>

<div class="rm-feature" markdown>

### Hilbert-Curve Locality

Resources are assigned to pages using a 3D Hilbert space-filling curve
over (longitude, latitude, concept-type). Geographically and semantically
similar resources land on the same page, minimising page spread.

</div>

</div>

---

## What is Rós Madair?

**Rós Madair** (Irish: *rose madder*) is a companion to
[Alizarin](https://github.com/flaxandteal/alizarin) — an ORM for
[Arches](https://www.archesproject.org/) heritage data management systems.
Where Alizarin provides a TypeScript/Rust SDK for working with Arches graphs
live, Rós Madair provides a static, pre-built query index that can answer
SPARQL-like pattern queries entirely in the browser.

The name follows the pigment theme: alizarin crimson and rose madder are
closely related red pigments, both derived from the madder root. Rós Madair
is the static, pre-ground pigment to Alizarin's live colour mixing.

### The Problem

Heritage datasets often contain tens or hundreds of thousands of records.
Deploying a full SPARQL endpoint (Fuseki, Blazegraph, etc.) requires
server infrastructure, ongoing maintenance, and non-trivial cost. For
read-only public datasets — museum collections, monument registries,
archaeological surveys — this is overkill.

### The Approach

Rós Madair pre-computes a two-level page-based index at build time:

1. A **summary index** (~12 MB for 160K resources) maps which pages
   contain which predicate/object combinations
2. **Page files** (~86 files, ~267 KB each) contain the actual records,
   partitioned by predicate and sorted for binary search

The WASM client loads the summary once, then answers queries by planning
which pages to fetch, making targeted HTTP Range requests for specific
predicate blocks, and executing pattern matching in-browser.

```
Static Files (CDN)              Browser (WASM)
─────────────────              ──────────────────
summary.bin  ──── load once ──→  Query Planner
dictionary.bin ── load once ──→  Term ↔ ID Mapping
pages/*.dat  ──── on demand ──→  Record Cache + Execution
                  (Range reqs)
```

## Next Steps

<div class="rm-features" markdown>

<div class="rm-feature" markdown>

### [Installation](getting-started/installation.md)

Build the Rust crates, install wasm-pack, and set up the toolchain.

</div>

<div class="rm-feature" markdown>

### [Quick Start](getting-started/quickstart.md)

Build an index from Arches data and run your first browser-side query.

</div>

<div class="rm-feature" markdown>

### [How It Works](how-it-works/overview.md)

Deep dive into the two-level index, query planning, and optimisation
techniques.

</div>

</div>
