# Rós Madair

A static-file query engine for graph data. `ros-madair-emit` compiles a graph +
resource corpus into a **DuckDB + Parquet substrate** — tile-row Parquet, melted
node-keyed stores, a columnar edge table, a per-resource entry table, a
concept catalog, and a signed manifest — served from a CDN or static host and
queried with no backend database. A reader (native `ros-madair-duck`, or
`ros-madair-wasm` → duckdb-wasm in the browser) compiles the typed query IR to
DuckDB SQL, fetches only the row groups it needs over HTTP Range, and hydrates
matches into a schema-shaped tree.

This document is the capability overview a *consumer* reads — what the substrate
is good at, and where its limits are. The internal design docs
(`spatial-grid-and-render-summary.md`, `graph-clustering-spike.md`) carry the
derivations; this one carries the contract.

## Scaling

The substrate has **one prune primitive: sort order.** Bundled DuckDB 1.1.x makes
`WRITE_BLOOM_FILTER` a no-op (consumer limitation #9), so a scattered probe reads
whole columns — only a query that anchors on a sorted/clustered key reads a slice.
Everything below is a consequence of that single fact. There are two independent
scaling axes, and a deployment has to get **both** right or the other does not
matter.

### Axis 1 — the read (how much the engine *scans*)

Every artifact is sorted so the hot access pattern anchors on its leading key:

| artifact | sorted by | the query it prunes for |
|---|---|---|
| `tiles_<g>.parquet` | `(cluster_key, resource_id)` | hydration + spatial/locality scans |
| `resources_<g>.parquet` (entry table) | `resource_id` | id-probe → entry lookup, counts, backlinks |
| `grid_<g>.parquet` | `cell` (Morton) | `GridBox` spatial superset |
| `concepts_<g>.parquet` | `(node_id, concept_id)` | `Concept` membership semijoin |
| `ordered_<g>.parquet` | `(node_id, value)` | `Range` over dates/numbers |
| `edges_<g>.parquet` | `src_node` | `HasLink` / `OnLink` / `cited_by` |

The one trap this layout sets, and the fix that closes it:

- **Tile hydration must go through the entry table.** Tiles are clustered by
  `cluster_key` (locality — geo ⊕ descriptor name), *not* by `resource_id`. So a
  bare `WHERE resource_id = ?` on `tiles` cannot prune on the leading sort key: it
  degrades toward a full scan (measured ≈ N^1.2–1.4 on the 763k corpus). The entry
  table carries `cluster_key` per resource, so the reader resolves
  `resource_id → cluster_key` first and hydrates with
  `WHERE cluster_key = ? AND resource_id = ?` — now the tiles scan prunes on the
  leading key. This is the **tile locator**; it is why `resources_<g>.parquet`
  carries `cluster_key` and why hydration is a two-step read.
- **Spatial is a superset, by design.** A bounding box lowers to the set of Morton
  `cell`s it covers (`cells_in_box`, capped at `MAX_BOX_CELLS = 16384`); the
  `GridBox` predicate reads only those cell ranges from the cell-sorted grid edge.
  The result is an **over-inclusive** superset of the box — over-inclusion is safe
  for the responder pattern (you never want to *miss* a building), and an exact
  `ST_Intersects` fine step is available on desktop where the spatial extension
  loads. (Mobile ships no spatial binary → coarse superset only.)
- **A head carries only the stores its data needs.** No concept fields → no
  `concepts_*.parquet`; no geo → no `grid_*.parquet`. A predicate over an absent
  store is a loud typed error at open/compile time, not a silent wrong answer.

### Axis 2 — the transport (how much the client *downloads*)

This axis is invisible until the corpus is large, then it dominates. The finding
that forced it (763k demo): a duckdb-wasm page that registers a file URL and
`CREATE VIEW … SELECT *` downloads the **whole file** — no Range header — because
duckdb-wasm's file-URL path is not range-aware. The same query that *should* touch
a few row groups pulled **472 MB**.

The fix is the transport, not the query: `INSTALL httpfs; LOAD httpfs;` and read
through a **full `https://` URL** (`read_parquet('https://…')`). httpfs *is*
range-aware — it reads the Parquet footer, then only the row groups the zone-map
keeps. Same corpus, same query: **472 MB → 7.59 MB.** This is a reader-side
contract, not an emit change:

- Serve the Parquet from a host that honours HTTP Range (R2, S3, any CDN).
- Register with httpfs + direct URLs, never `registerFileURL` + `CREATE VIEW`.
- Cache-bust with versioned, immutable paths (`<dataset>/<snapshot_id>/…`) so a new
  emit is a new path and the old one stays cacheable forever.

A deployment that nails Axis 1 but serves over the whole-file path still ships half
a gigabyte per query; a deployment that nails Axis 2 but leaves tiles unclustered
range-reads a slice that is itself the whole corpus. Both, or neither.

### The evidence (the locator gap)

A consumer's scale harness ran its real reader over fixed-shape synthetic worlds of
1,000 / 4,000 / 16,000 resources, counting the distinct bytes DuckDB read per page
and fitting the growth exponent (bytes ≈ N^k). Before any fix:

| Page | N=1,000 | N=4,000 | N=16,000 | k |
|---|---|---|---|---|
| View opening | 81K | 176K | 509K | 0.66 |
| Places lens | 157K | 499K | 1,852K | 0.89 |
| Events & Times lens | 235K | 822K | 3,127K | 0.93 |
| People lens | 261K | 951K | 1,121K | 0.53 |
| **A person** | 204K | 2,246K | **7,358K** | **1.29** |
| **An event** | 190K | 2,231K | **8,247K** | **1.36** |
| **A place** | 220K | 2,299K | **6,042K** | **1.20** |
| **The hub** | 187K | 2,170K | **9,112K** | **1.40** |
| Cold start onto a person | 642K | 2,416K | 8,002K | 0.91 |
| Name search (Pagefind) | 250K | 263K | 320K | 0.09 |

The entry pages (a person/event/place/hub) grow **super-linearly** — that is the
locator gap, and the headline. In file sizes at N=16,000, one person's page reads
**2.49 MB of the 2.58 MB** NPC tile file, **1.64 MB of the 2.63 MB** Event file and
**1.31 MB of the 1.43 MB** Place file — ~95% of each whole file to hydrate a few
dozen resources. Tiles are clustered by `cluster_key`, not `resource_id`, so a
resource-id hydrate can't prune: it reads whole tile files, and more of them as the
neighbourhood grows. That is exactly what the **tile locator** fixes —
`resource_id →` entry table (sorted by id, prunes) `→ cluster_key →` tiles
`WHERE cluster_key = ? AND resource_id = ?` (prunes on the leading key). The
remaining O(N) costs in the table are a consumer's own tables (an unsorted entry
scan, unpaged backlinks); the general fixes are the entry table sorted by id,
per-model counts in the manifest, and paged backlinks with a per-target count.

### The harness method (a standing gate)

The measurement *method* is the most valuable part, and it is general:

- **Corpora** at sizes ≥4× apart in a fixed shape, so only N varies. Include a
  **hub** whose backlinks grow with N — busy resources are where O(N) hides.
- **Trace the reads**, not a wall-clock: the reader under
  `strace -e trace=openat,pread64,close`, reads attributed to files by descriptor
  and cut per page by marker files. Report distinct bytes, total reads, files
  opened, and coalesced requests (reads within 16 KB merged ≈ what a range reader
  asks for).
- **Fail on growth, not on size.** The gate is the exponent k between the smallest
  and largest corpus, per measure per page; fail above **k = 0.2**. A fixed byte
  budget passes a whole-graph read at small N — an exponent does not.
- **Run against the native reader.** Which row groups a plan touches is the same
  for any reader, so the native read trace gates the locator (Axis 1) for every
  consumer. The wire (Axis 2) needs the browser engine against a Range-honouring
  server; same k-gate, separate rig.
- Use corpora past ~100k rows (or emit with small row groups) so footer/manifest
  growth shows rather than hiding inside a single row group.

This is the gate the substrate should keep: every lookup pattern — a resource by
id, by slug, its backlinks, a model listed by name, a count — flat as N grows.

### What the emitter gives a consumer for free

- **Per-model resource counts** in the manifest (`ModelManifest.resource_count`) —
  a consumer lists corpus sizes without scanning anything.
- **The entry table** (`resources_<g>.parquet`): `resource_id, graph_id, legacyid,
  descriptor, cluster_key`, one small row per resource, sorted by `resource_id` —
  the spine a consumer paginates, counts, and slug-backlinks on without touching a
  hydration `data` column.
- **The grid edge** also carries a per-resource `descriptor` and (v4) a
  `geo_descriptor` GeoJSON point, so a map can render pins from the spatial result
  *without* hydrating the full tile tree.

## Format version

The on-disk layout is stamped with `FORMAT_VERSION` (currently **4**) in the
manifest. A reader refuses a layer whose version it does not implement
(`open_layers` checks `format_version != FORMAT_VERSION` before it touches a
column) — a dropped or renamed Parquet column surfaces as a loud "re-emit" at open
time, never as a cryptic failure deep in a query. Pre-v4 snapshots are not read;
re-emit.
