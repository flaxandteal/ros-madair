# Prunable spatial + a resource-level render summary

Two emit-side proposals that fell out of building the 763k demo (`/docs/aonach-763k`).
Both attack the same root constraint: **on this substrate the only thing that prunes
is sort order.** Bundled DuckDB 1.1.x makes `WRITE_BLOOM_FILTER` a no-op (consumer
limitation #9), so a scattered id-probe reads whole columns; only a query that
anchors on a sorted/clustered key reads a slice. Everything below is about giving
the two hot access patterns — "render a result set" and "resources near X of kind
Y" — a key they can prune on.

## What the demo measured (the grounding)

Substrate: 763,064 buildings → 6.98M tiles, ~1.3 GB, emitted with
`--partition-by-nodegroup` → **10 nodegroup partitions** per model.

| partition (nodegroup) | size | geo? | `__cache`? | `descriptor_name`? |
|---|---|---|---|---|
| `714732b8…` (footprint) | 143 MB | ✅ (clustered) | — | ✅ |
| `d6ed56b0…` (site link) | 79 MB | — | ✅ | ✅ |
| `d0d0c3f3…` (service_roles) | 12 MB | — | — | ✅ |
| 7 others | 77–90 MB | — | — | ✅ |

Costs observed, each traceable to "no key to prune on":

- **`Bbox` over the `tiles` view ≈ 70 MB.** The view is the UNION of all 10
  partitions. Only `714732b8` has geo; the other 9 have NULL geo stats, so the
  zone-map can't prune them — the Bbox reads `resource_id` across all ~7M tiles.
- **Dense-role corpus read ≈ 8 MB.** `Concept(service_roles = Shelter)` prunes the
  melted concept store (sorted `node_id, concept_id`) to the role slice — but that
  slice is **86k shelters corpus-wide**, all pulled to intersect with the box.
- **Fat `__cache` render ≈ up to 77 MB.** The ward lives in `__cache` on
  `d6ed56b0`, sorted by neither geo nor `resource_id`; fetching it for 30 scattered
  result ids reads that partition's whole `data` column.
- **`descriptor_name` is cheap.** Promoted on every tile row, and from `714732b8`
  it comes back geo-pruned. The resource's *own* name was never the problem.

The current cheap workaround (flatbush box ∩ `Concept` ids, name-only render from
the geo partition) removes the 70 MB and the 77 MB but still eats the **8 MB
dense-role corpus read** and **drops the ward**. The two proposals close exactly
those two gaps.

---

## Proposal 1 — resource-level render summary on an entry table

### Problem

Alizarin holds the render cache at **resource level** (`r.cache`, shaped
`{tileId: {nodeId: CacheEntry{title, descriptors}}}`). Emit **slices it per tile**
(`parquet.rs`: embeds only `cache.get(tileid)` on each tile row), so a resource's
cache ends up scattered across its ~10 tile rows in ~10 partitions. That slicing is
right for **deep hydration** (rebuild one tile → its related labels are in the same
row you already fetched) and wrong for **rendering a result set** (you want one
compact blob per resource, in one place, pullable N at a time). There is also no
per-resource row at all today — the resource identity (`legacyid`/slug, model,
flags) lives only inside hydration `data` (consumer limitation #1).

### Shape

Add one file per model (or one shared), **one row per resource**, no hydration JSON:

```
resources.parquet
  resource_id    (uuid)        -- the key
  graph_id       (uuid)        -- model scoping (closes consumer #1 "SQL not scoped to model")
  legacyid       (text)        -- slug / stable id for backlinks
  descriptor     (text)        -- the display name (already have it as descriptor_name)
  render_summary (json)        -- CURATED: the few related titles you'd show (e.g. ward),
                               --   NOT the full fat __cache
  flags          (json)        -- layers / General etc.
```

Clustered on whatever the common list/entry query anchors on (graph_id, then a
display-order key). Crucially it is **small** — no `data` blob — so even an
unpruned id-probe of 30 scattered ids reads a few MB, not 77.

### Emit changes

- A pass that, per resource, writes one `resources` row.
- `render_summary` = a **curated projection** of `r.cache` (the handful of related
  titles a card shows), not the whole cache. Decide the projection per graph
  (config): e.g. "the title of the linked Site" → ward.
- Keep the per-tile `__cache` slice for deep hydration, or shed it with
  `--no-render-cache` once the render path reads `resources.parquet` instead. (The
  demo proved the per-tile slice is the wrong home for *rendering*.)

### Query / hydrate changes

- A render step reads `resources.parquet WHERE resource_id IN (…)` → name + summary
  in one small read. No UNION, no 77 MB partition.
- Unlocks lists / counts / slug-backlinks and **model scoping** (filter by
  `graph_id`) — the broadest-payoff item on the consumer handoff.

### Pruning analysis

The win is **size, not keying**: resource-level-on-a-fat-partition would prune no
better. `resources.parquet` is cheap because it carries no hydration JSON. If it is
also clustered on `graph_id` + display order, entry-list queries prune to row
groups; the scattered-id render stays "small-table whole read," which is fine.

### Trade-offs

- `render_summary` is a **curation decision** — too much and the table fattens back
  up; too little and the render needs a second fetch anyway. Per-graph config.
- Duplicates the display name already in `descriptor_name`. Accept it — the entry
  table is the one place you *don't* want to reach into tiles for a header.
- Doesn't help "which resources are near X" — that's Proposal 2.

---

## Proposal 2 — grid-cell spatial edge (prunable spatial)

### Problem

Spatial has no sort-order key on the stores, so the only two options today are (a)
`Bbox` over the UNION (70 MB) or (b) flatbush box ∩ the whole role corpus (8 MB for
a dense role). Both read far more than the answer. We need spatial locality
expressed as **sort-order locality** so Parquet can prune it.

### Shape

An emit-time edge table that buckets each resource into a discrete grid cell:

```
grid_<graphid>.parquet
  cell          (int64)   -- quantized (lat,lng) bucket, e.g. Morton/Z-order of a fixed grid
  resource_id   (uuid)
  [role]        (text)    -- OPTIONAL: fold the common facet in for a one-read prune
  ORDER BY cell [, role]
```

Query becomes a **range read**:

1. box → intersecting cells — **pure arithmetic**, no index needed.
2. `SELECT resource_id FROM grid WHERE cell IN (…) [AND role = 'Shelter']` →
   prunes to a few row groups → the near-of-kind set directly, in **KB**.

A new predicate `OnCell { path, cells: [...] }` (or lower `Bbox` to it when a grid
exists) carries this through the typed IR → compiler, same as `OnLink`.

### Emit changes

- Compute a cell id from each resource's geometry centroid/bbox (reuse the geo
  extraction already in `emit::geo`). Grid origin + resolution are per-graph config.
- Write `grid_<graphid>.parquet` sorted by `cell` (optionally `cell, role` if we
  fold a facet). Additive — same pattern as `edges_`/`redges_`.

### Pruning analysis

`cell` is the sort key, so `cell IN (…)` prunes to the contiguous row groups holding
those cells — the one thing that *does* work without bloom filters. Folding `role`
as a secondary key means `cell IN (…) AND role = X` prunes twice: the 8 MB
dense-role corpus read collapses to the handful of cells the box touches (KB).

### Resolution knob (the real tuning)

- **Too coarse** → a cell pulls in far more than the box; you over-read then
  re-filter (flatbush, or exact geometry, as the fine step).
- **Too fine** → the `cell IN (…)` list for a radius explodes, and the edge table
  grows (more distinct cells → more row groups). 
- Pick resolution so a typical query box spans O(10) cells. Per-graph config; can
  ship multiple levels (a coarse + a fine grid) if one size won't do.

### Relationship to flatbush (a deliberate nudge)

This **partly subsumes flatbush**. "Which cells does a box touch" is arithmetic —
you don't need an R-tree for the *coarse* filter. So the earlier "delegate all
spatial to flatbush" decision shifts to:

- **grid edge** = the coarse, *prunable*, in-substrate spatial filter (RM's lane:
  it's just a sorted edge/range read — geography modelled as a graph key).
- **flatbush** = optional *fine* step only when you need sub-cell precision (exact
  point-in-footprint), over the already-small cell result.

That is more RM-shaped than Hilbert-clustering the tiles (which we dropped): a grid
cell is a connectivity bucket, and connectivity clustering is the clustering worth
building.

### Trade-offs

- New emit work (cell computation + a new artifact + a new predicate), and a new
  `FORMAT_VERSION` bump.
- Grid is **regular**, not data-adaptive — dense areas get crowded cells, sparse
  areas empty ones. Fine for pruning (empty cells cost nothing); only matters if
  one cell holds a huge share of resources (then it doesn't prune *within* the cell
  — fall back to flatbush/exact on that cell).
- Centroid bucketing mis-buckets large/overlapping footprints at cell edges; a
  bbox-to-cells (a footprint can occupy several cells → several edge rows) fixes it
  at the cost of fan-out.

---

## How they compose — the demo query, rewritten

"Hospitals near ground zero, show name + ward":

1. box → cells (arithmetic).
2. `grid WHERE cell IN (…) AND role = 'Hospital'` → the near-hospital ids (KB,
   pruned). *(Replaces flatbush box ∩ 8 MB corpus read.)*
3. `resources.parquet WHERE resource_id IN (those ids)` → name + ward from
   `render_summary` (small-table read, no 77 MB partition). *(Replaces the fat
   `__cache` render.)*
4. flatbush only if a cell is coarse enough to need exact in-box refinement.

Net: tens of MB → low single-digit MB, and the ward comes back.

## Phasing

1. **`resources.parquet` + `render_summary` first.** Smallest change, broadest
   payoff (lists, counts, model scoping, slug-backlinks, *and* the cheap render),
   and it's consumer-handoff #1. Doesn't need a new predicate — it's just a read.
2. **grid edge second.** Bigger (new artifact + `OnCell` predicate + compiler
   lowering + `FORMAT_VERSION` bump), but it's what makes the *spatial∩facet*
   intersect prunable and generalizes to the whole "near X, of kind Y" use case.

Both reuse muscles we already have: the entry table is a melt-store-style additive
writer; the grid edge is the `edges_`/`redges_` pattern with a computed key and a
new typed predicate.

## Open questions

- `render_summary` curation: per-graph config, or a convention (e.g. "titles of all
  resource-instance links")? How much before it's just `__cache` again?
- Grid: single resolution vs. multi-level; centroid vs. bbox-to-cells; Morton vs.
  a plain row-major cell id (Morton keeps nearby cells near in sort order → tighter
  `cell IN` ranges).
- Does the grid edge make RM's `Bbox`/`geo_*` columns redundant? If spatial is
  always grid+flatbush, we could drop `geo_*` promotion and the geo cluster key
  (already flagged in the flatbush decision) — the grid is the replacement.
