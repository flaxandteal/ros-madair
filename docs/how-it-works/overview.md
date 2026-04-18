# Overview

## Executive Summary

Rós Madair is a browser-based query engine for heritage graph data. It answers
SPARQL-like queries over tens of thousands of resources without a server-side
database — all data lives as static files on a CDN or file server, and query
execution happens entirely in the browser via WebAssembly.

The key insight is a **two-level index**: a small summary index (~12 MB) loaded
once at startup tells the query planner which pages of data are relevant, and
then only those pages are partially fetched using HTTP Range requests. A query
over 160,000 resources that matches 6,900 results typically transfers ~540 KB
across ~100 HTTP requests, touching roughly 2% of the 22 MB total dataset.

Multi-predicate queries (e.g., "monuments of type X in townland Y") benefit
from **page-set intersection** at the planning stage — the planner identifies
pages that satisfy *all* predicates before fetching anything, cutting requests
by 60–70%.

Resources are assigned to pages using a **3D Hilbert space-filling curve**
over (longitude, latitude, concept-type), so geographically and semantically
similar resources land on the same page, minimising the number of pages a
typical query must touch.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│                  Static File Server                   │
│                                                       │
│  summary.bin (12 MB)     ← loaded once at init        │
│  dictionary.bin (15 MB)  ← loaded once at init        │
│  page_meta.json          ← loaded once at init        │
│  pages/                                               │
│    page_0000.dat  (0–2 MB each)                       │
│    page_0001.dat     ← fetched on demand via          │
│    ...                  HTTP Range requests            │
│    page_0110.dat                                      │
└──────────────────────────────────────────────────────┘
        │
        │  HTTPS (static files, CDN-friendly)
        ▼
┌──────────────────────────────────────────────────────┐
│                  Browser (WASM Client)                │
│                                                       │
│  1. Load summary index + dictionary + page metadata   │
│  2. Accept query (triple patterns)                    │
│  3. Plan: which pages to fetch, which predicates      │
│  4. Fetch: HTTP Range requests for page headers       │
│            + predicate blocks                         │
│  5. Execute: binary search within records             │
│  6. Return matching resource URIs                     │
└──────────────────────────────────────────────────────┘
```

The server plays no role in query execution. It serves static files — from S3,
a CDN, GitHub Pages, or even `python -m http.server` — and the WASM client
handles planning, fetching, and execution entirely in the browser.

## Query Flow

A query passes through four phases:

1. **Initialisation** (once per session): Load `summary.bin`, `dictionary.bin`,
   and `page_meta.json`. After this, the client holds the full summary and
   dictionary in memory. No page files have been fetched yet.

2. **Planning** (per query, zero network): Translate URIs to dictionary IDs,
   look up which pages contain relevant data in the summary index, intersect
   page sets for multi-predicate queries, and produce a fetch plan.

3. **Fetching** (per query, selective network): Make HTTP Range requests for
   page headers and specific predicate blocks. The client never downloads
   entire page files.

4. **Execution** (per query, in-memory): Binary search within loaded predicate
   blocks, intersect result sets across patterns, and resolve subject IDs back
   to URIs.

See [Query Execution](query-execution.md) for worked examples, and
[Data Structures](data-structures.md) for format details.

## Build Pipeline

The index is generated once at build time from alizarin graph definitions
and resource data. Three parallel transformations feed into five output
files:

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Alizarin Input                                   │
│  StaticGraph  (graph definitions: node schemas, predicates)          │
│  StaticTile   (resource data: concepts, dates, geometry, text, ...)  │
└──────────┬───────────────────────┬──────────────────────┬───────────┘
           │                       │                      │
  ┌────────▼─────────┐  ┌─────────▼──────────┐  ┌────────▼─────────┐
  │ Extract metadata │  │ Quantize values    │  │ Build dictionary │
  │                  │  │                    │  │                  │
  │ • centroid (geo) │  │ concept → dict ID  │  │ Every URI and    │
  │ • concept set    │  │ resource → dict ID │  │ literal gets a   │
  │ • graph ID       │  │ boolean → 0 / 1    │  │ sequential u32   │
  │                  │  │ date → days+offset │  │ integer ID       │
  └────────┬─────────┘  │ geo → Hilbert u32  │  │                  │
           │             └─────────┬──────────┘  └────────┬─────────┘
  ┌────────▼─────────┐             │                      │
  │ Assign pages     │             │                      │
  │                  │             │               ┌──────▼────────┐
  │ Tier 1: group by │             │               │dictionary.bin │
  │   graph ID       │             │               └───────────────┘
  │ Tier 2: sort by  │             │
  │   3D Hilbert     │  ┌──────────▼──────────┐
  │   (lng,lat,type) │  │ Build page records  │
  │ Slice: ~2000     │  │                     │
  │   resources/page │  │ PageRecord (8 bytes) │
  └────────┬─────────┘  │ = (object_val,      │
           │             │    subject_id)       │
           │             └──────────┬───────────┘
           │                        │
           └────────────┬───────────┘
                        │
           ┌────────────▼─────────────────────────────────┐
           │          Group by (page, predicate)           │
           │          Sort within each group               │
           └─────┬──────────────┬─────────────┬───────────┘
                 │              │             │
         ┌───────▼──────┐ ┌────▼────────┐ ┌──▼─────────────┐
         │ summary.bin  │ │ page files  │ │ resource_map   │
         │              │ │             │ │   .bin         │
         │ 3 sorted     │ │ page_XXXX   │ │                │
         │ quad copies  │ │   .dat      │ │ dict_id →      │
         │ (SPO,PSO,OPS)│ │ per-page    │ │   page_id      │
         └──────────────┘ │ predicate   │ └────────────────┘
                          │ blocks      │
         ┌──────────────┐ └─────────────┘
         │page_meta.json│
         │ page→bbox    │
         └──────────────┘
```

The build also writes `all.nt` (full N-Triples export) for verification
against an external SPARQL store like oxigraph.

## Page Assignment

Resources are assigned to pages at index-build time using a two-tier strategy:

### Tier 1: Graph Grouping

Resources are first grouped by their graph/model type (e.g., Heritage Place,
Person, Activity). Resources of the same type share the same predicate
schema, so grouping them together means page files have consistent predicate
blocks.

### Tier 2: Hilbert Space-Filling Curve

Within each graph group, resources are sorted by a **3D Hilbert curve**
over three axes:

1. **Longitude** (x): normalised from [-180°, 180°] to [0, 1024)
2. **Latitude** (y): normalised from [-90°, 90°] to [0, 1024)
3. **Concept bucket** (z): a hash of the resource's concept URIs, mapped
   to [0.0, 1.0) and scaled to [0, 1024)

The Hilbert curve (10-bit resolution per axis, Skilling's algorithm)
preserves locality: resources that are geographically close *and* share
similar concept types get nearby Hilbert indices and land on the same page.

The sorted sequence is then sliced into pages of ~2,000 resources each.

!!! info "Why this matters"
    A query for "all ringforts in County Down" benefits because ringforts in
    County Down are geographically clustered and share the same concept type —
    they'll occupy a small number of pages rather than being scattered across
    the entire index.
