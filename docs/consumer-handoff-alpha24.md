# Rós Madair — consumer handoff (alpha.24)

For the downstream integrator agent. Status of the 10 reported limitations against
the **just-shipped alpha.24**, what to rely on vs keep owning, and the interface
shapes RM will read where it doesn't yet cover you. Every item below was verified
against the alpha.23/24 source; your list was accurate.

## Version / packaging facts (read first)

- **alpha.24 is live** on crates.io, npm, PyPI, + a single `ros-madair-emit-linux-amd64` GitHub asset.
- **Version lockstep is real.** The emit binary, the `ros-madair` npm compiler, and the crates all release at ONE workspace version, and a snapshot's `FORMAT_VERSION` (in `manifest.json`) must match the reader. **Pin everything to the same alpha.**
- **npm caveat:** the `latest` dist-tag still points at a19 (legacy bootstrap). Install `ros-madair@0.1.0-alpha.24` (or `@alpha`) explicitly — NOT bare `ros-madair`.

## Your 10 limitations — current disposition

Legend: **[RELY]** RM covers it now · **[KEEP]** still yours · **[PLANNED]** real gap we intend to close · **[BOUNDARY]** intentional delegation, stays yours.

1. **No entry table — [KEEP → PLANNED].** Accurate: `legacyid`/slug live only inside the hydration `data` blob; nothing per-resource is queryable; no `layers`/`General` flag. Keep publishing your `resources.parquet`. **Publish to this shape and RM will adopt it** as the slug + model-scoping spine when we build it: `resource_id, graph_id, legacyid, descriptors, flags (layers, General)`. This is our #1 leverage fix.
2. **Edges only from link fields — [KEEP → BOUNDARY + hook].** Accurate: edges come only from `IndexClass::Link` (resource-instance) nodes; `[[refs]]` in text classify `DetailOnly` → no edge, and `redges_` (a re-sort of the same table) inherits the blind spot. Markdown parsing isn't RM's job, so keep `mentions.parquet`. **Offered hook:** hand us declared in-text refs *as link values at emit* (a resource-instance node, or a mentions sidecar we lift) and they become real edges **+ reverse-indexed**.
3. **No browser layer composition + whole-tile replace — [PARTIAL / PLANNED].** Two true halves. (a) Browser shadow-set composition is now **scaffolded** (`lib/ros-madair.ts registerLayers`) but **unreleased — do not rely on it yet.** (b) The deeper one stands: RM composes by **replacing whole tiles per `(resource, nodegroup)`**; Alizarin **merges field-by-field and appends repeatables**. The browser port mirrors whole-tile semantics, so shipping it still won't field-merge. **Keep composing at build time + owning the overlay tables** until we (i) ship browser composition and (ii) decide whether RM offers field-merge overlays — a real semantic decision, still open.
4. **No access windowing — [KEEP → BOUNDARY].** Accurate and intentional: grants / client-filter / partitioning / encryption are consumer-side by design. **Offered contract:** publish `access.parquet` as `resource_id + scope/grant columns` and RM will scope reads by it. Propose the columns; we'll freeze the contract.
5. **Browser = compiler only; `__cache` always-on — [RELY (opt-out) / KEEP (hydration)].** Accurate. **Fixed in a24:** `__cache` is now opt-out — emit with `--no-render-cache` (or `ROS_MADAIR_EMIT_RENDER_CACHE=0`) to shed the ~49% inflation (that figure is *your* measurement — plausible, not independently confirmed; tell us if it's off). **Still yours:** the npm package is compiler-only — reading / hydration / `__cache` rendering are native-only. Keep your own hydration until **browser hydration** ships (a tracked gap).
6. **Dates = single day-count from the first key — [KEEP → PLANNED].** Accurate: `quantize_date(keys.first())` → one i64; EDTF intervals collapse to the first key, uncertainty dropped. **Negative / ancient years:** `days_from_civil` is signed i64 so it *should* compute, but there is **no test** — if your fictional calendar uses negative/very-old years, give us the range and we'll add a test matrix. Planned: a two-column `[lo,hi]` interval ordered store.
7. **No text search — [BOUNDARY].** Correct and intentional — Pagefind is the designed delegation (zone-maps have no order to prune text on). Stays yours.
8. **Many small files (~21) — [KEEP, aware].** Accurate: 5 kinds/model (`tiles/edges/redges/concepts/ordered`) + catalog + manifest. Fetching small files whole is sensible. A manifest-driven prefetch / melted-store consolidation is possible later (trades against zone-map granularity).
9. **No bloom filters — [BOUNDARY / tracked].** Accurate (confirmed empirically): bundled DuckDB 1.1.x `WRITE_BLOOM_FILTER` is a no-op, so **pruning is sort-order-only**. Design queries to anchor on a sorted/clustered column; a scattered id-probe won't prune. Fixable by a DuckDB bump — tracked, not scheduled.
10. **linux-amd64 only + version lockstep — [KEEP → PLANNED].** Accurate: one `linux-amd64` asset, no mac/arm/windows; a release matrix is planned. Version lockstep is real (FORMAT_VERSION gated in the manifest).

## Emit at scale (you'll want these a24 flags)

We hit an OOM emitting ~6.8M tiles at the default 512 MB cap. a24 adds:
- `--partition-by-nodegroup` — splits each model's one giant sort into per-nodegroup sorts.
- `ROS_MADAIR_EMIT_MEMORY_LIMIT=<N>GB` (default `512MB` for on-device) and `ROS_MADAIR_EMIT_THREADS`.
- `--no-render-cache` (see #5).

For a large corpus: `ros-madair-emit <data_dir> <out_dir> --partition-by-nodegroup --memory-limit 8GB`.

## Scope boundaries you can plan around

- **Spatial is not RM's job** (decided): model geography as **Place resources**, index with a **standalone flatbush** (static R-tree, offline-clean), feed Place-ids into RM as an **edge semijoin**. RM's one job = graph / hierarchy / edge / multi-hop retrieval.
- **Text → Pagefind.** **Access → yours.** Both intentional.

## Interface contracts RM will read (publish to these shapes)

- `resources.parquet`: `resource_id, graph_id, legacyid, descriptors, flags`.
- in-text refs → the edge table, via an ingestion hook (resource-instance node or mentions sidecar).
- `access.parquet`: `resource_id + scope/grant columns`.

Propose exact columns on each and we'll freeze the contract.

## Our top-3 leverage (your validation welcome)

1. **`resources.parquet` entry table** — unlocks lists / counts / slug-backlinks *and* model-scoping; smallest change, broadest payoff.
2. **Field-merge overlay semantics** — the deepest divergence from Alizarin; it's what forces you to own composition.
3. **Browser hydration + `__cache` opt-out** — makes the npm package a reader, not just a compiler (opt-out shipped in a24).
