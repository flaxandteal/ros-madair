# Spike: graph-aware clustering for multi-hop locality

**Status:** spike / design note. Evidence from
`crates/ros-madair-duck/tests/clustering_spike.rs`
(`cargo test -p ros-madair-duck --test clustering_spike -- --ignored --nocapture`).

## Problem

Rós Madair's reason to exist over "duckdb-wasm + Parquet" is making *multi-hop*
graph queries scale statically. A traversal's HTTP-Range cost is dominated by how
many **row groups the frontier touches**, and that is a function of **tile
ordering**.

Today tiles are clustered by `geo ⊕ descriptor` (`ClusterDim::{Geo, Descriptor}`)
— a **value** order that is *uncorrelated with the edge graph*. So a connected
frontier (the set of resources a traversal lands on) scatters across the whole
file, and hydrating it reads most of the row groups. Generic zone-maps prune by
column *value*; they cannot prune by *connectivity*. Nobody gives you
connectivity locality for free — it has to be built at emit time. (RM already
does exactly this for the SKOS hierarchy via DFS-interval ordering; this extends
the same instinct to the resource edge graph.)

## Evidence

Synthetic community-structured graph (the shape heritage data has — a site's
features/events/actors link within the site): 100 000 resources, 1 000
communities, out-degree 8 intra + 1 inter, row groups of 2 048 → 49 groups.
Averaged over 64 two-hop seeds:

| tile ordering | row groups touched by a 2-hop frontier (avg 27 resources) |
|---|---|
| value (`hash(rid)`, proxy for geo/descriptor) | **20.7** |
| connectivity (`community, rid`) | **1.1** |

**≈19× fewer row groups.** The value order scatters a 27-resource frontier
across 21 of 49 groups; connectivity keeps it in ~1. On an HTTP-Range reader that
is ≈19× less data fetched to hydrate a 2-hop neighbourhood.

Method caveat: `hash(rid)` is a *fair* proxy for geo/descriptor clustering
because both are uncorrelated with the edge graph. A real corpus where geography
*correlates* with connectivity (neighbours are also geographically near) would
show a smaller — but still positive — gap; that is exactly what the follow-on
emit-level benchmark on aonach must measure.

## What this does and does not cover

- **Covers: frontier HYDRATION.** Tiles carry the fat `data` JSON; fetching a
  connected neighbourhood's records is the dominant byte cost, and that is what
  connectivity-clustering the tiles file attacks. "Show me the connected
  subgraph" gets ~19× cheaper.
- **Does NOT cover: the id-resolution scans.** The edge table (sorted
  `src_node, target_resource`) and the melted property stores (sorted
  `node_id, value`) are keyed for value lookup, not by `resource_id`/connectivity.
  A pure `SelectIds` multi-hop reads those, and reordering the TILES does nothing
  for them. Making the property-store scans frontier-local too means adding the
  resource's cluster ordinal as a secondary sort in those stores — a bigger
  follow-on, measured separately.
- **Orthogonal to the reverse edge index** (`redges_`), which is about untyped
  target-anchored access, not traversal locality.

## Design

`ClusterConfig.dimensions: Vec<ClusterDim>` already composes cluster axes per
graph at index time. Add a connectivity axis:

```rust
enum ClusterDim { Geo, Descriptor, Connectivity }  // new variant
```

At emit, when `Connectivity` is present, compute a per-resource **cluster
ordinal** from the edge graph and make it the PRIMARY sort key (blended with the
others as tie-breakers), so connected resources land in the same row groups.

### Choosing the ordinal (cheap → expensive)

1. **BFS/DFS forest order** — walk the undirected edge graph, emit visitation
   order. O(V+E), trivial, deterministic with a fixed start/tiebreak. Decent
   locality; the obvious first cut.
2. **Label propagation** — each node adopts the majority community label of its
   neighbours, iterated a few rounds; order by (label, rid). O(E·rounds), cheap,
   near-linear, good community locality. Needs a deterministic tiebreak for a
   reproducible `snapshot_id`. **Recommended default.**
3. **Recursive graph bisection / METIS-style** — best locality, superlinear
   cost, heavier dependency. Reserve for corpora where the win justifies it.

## Hard problems / risks (state them, don't bury them)

- **Single-axis.** You cluster well on ONE axis. Geo locality and graph locality
  compete; a resource with many relationships can only be adjacent to some of its
  neighbours. The per-graph config is the release valve: a traversal-heavy graph
  picks `Connectivity`, a map-first graph keeps `Geo`. This is a *choice per
  graph*, not a universal win.
- **Which edges?** Cluster on undirected all-edge connectivity by default;
  optionally weight by the declared/hot traversal predicates.
- **Determinism.** The ordinal MUST be deterministic (fixed seeds, total-ordered
  tiebreaks) or the `snapshot_id` is unstable. This constrains the algorithm.
- **Overlay/layers.** The base's ordinal does not cover overlay resources;
  overlays append and locality degrades gracefully (no correctness impact).
- **Emit cost.** Label propagation is cheap; a static publish can afford it.
  METIS may not be worth its cost/dependency.

## aonach is NOT a valid benchmark for this (measured)

Measuring the emitted aonach corpus (`ros-madair-docs/public/data`) settles it:

- **Real aonach models are tiny**: 3–17 resources each, **one row group each**.
  Below one row group there is nothing to prune — the whole (tiny) file loads
  regardless — so clustering has **zero** impact on the real demo. aonach is a
  *correctness/feature* demo, not a scale corpus.
- **The synthetic `_full` blow-up** (780 k resources, 2.1 M edges) is the only
  thing at scale, but its edge graph is a **star**: 2.1 M distinct sources →
  **32** distinct targets. A star has no community structure for connectivity
  clustering to exploit, and its traversal cost is dominated by **frontier
  cardinality** (a hub has ~66 k neighbours) — which *no* clustering fixes.

So aonach can neither demonstrate nor benefit from this. Validating clustering
needs a corpus with **scale (>1 row group/model) AND community structure AND
geo that does not already capture connectivity** — none of which aonach has.

## Recommended next step

Implement `ClusterDim::Connectivity` via **deterministic label propagation** and
wire it as the primary sort when configured. Benchmark it on a **synthetic
community-structured corpus** (extend this spike's generator through the real
emit path) or a genuine large graph corpus — explicitly NOT aonach. If the gap
holds, follow with the property-store secondary-ordinal sort so `SelectIds`
multi-hop goes local too.
