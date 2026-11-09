# Hand-off: alizarin a144 in Rós Madair

**From:** alizarin `release/2.0` at `v2.0.0-alpha.144` (`f5e1e9e`).
**Audience:** RM maintainers.
**Short version:** RM needs no code change to keep working on a144 (f834255 already builds and passes on it).
Using the new **fold** primitive for layer spans is optional work, described in section 3. There is one
blocker outside RM: the a144 crates are not on crates.io yet (section 1).

## 1. Blocker: a144 crates are not on crates.io

The a144 release published every npm package, but **the crates.io publish failed** for `alizarin-core`. It
failed for `alizarin-geo-core` at a143 too. Internal Cargo deps are written `version = "2.0.0-alpha.N"`, which
is a caret requirement, and crates.io has `2.0.0-beta.9`, which sorts above every alpha. So `cargo publish`
verified against beta.9.

- `alizarin-core` and `alizarin-clm-core` **2.0.0-alpha.144 do not exist on crates.io**.
  `alizarin-extension-api` alpha.144 does.
- RM's `=2.0.0-alpha.144` pins therefore resolve only through the local `[patch.crates-io]` in `Cargo.toml`.
  CI and any machine without the sibling checkout will fail until alizarin republishes.
- RM's exact `=` pins are the right shape: they also stop beta.9 entering RM's graph through
  `alizarin-clm-core`'s caret requirement on core. Keep them.
- **Fixed in alizarin `acb5a3a`** (exact `=` pins on internal crate deps), released as **alpha.145**. 144
  won't be republished on crates.io. Move RM's pins to `=2.0.0-alpha.145` and drop the patch once 145 is on
  crates.io. a145 has the same code as a144 apart from the pins, so everything below applies unchanged.

## 2. a144 changes that touch RM (none need code changes)

| Change in a144 | RM exposure | Action |
|---|---|---|
| Reference node config moved out of core. `NodeConfigReference`, `as_reference` and `get_reference` are gone; extension configs are `NodeConfig::Extension { datatype, config, .. }`, read through the handler. | RM doesn't use these. | None. |
| Label resolution: the default config keys are only `rdmCollection`. A reference node's collection comes from the registered CLM handler (`collection_id`, via its `node_config` → `collectionId`). `build_alias_to_collection_map` and `resolve_labels_full` take `registry: Option<&ExtensionTypeRegistry>`. | RM doesn't call them. `reference_labels.rs` passes. | If RM ever calls them, pass `default_registry()`, or references lose their collection. |
| `datatype_index_spec`: the core `controlledList` fallback is gone (it only ever applied to concept datatypes). | RM's `reference` specs come from the CLM handler in the registry. | None, as long as a registry is passed (the `Option` call sites in `ros-madair-query` and `ros-madair-compile` do). |
| List display of multi-value references is the handler's `display_as_list`, cached per node. | `hydrate_tiles_with_labels` sets `node_config: None`, so references render as one joined string. That was already true on a143. | Optional: to render multi-value references as label arrays, the serialization context needs the node's `NodeConfig`. |
| `ABI_VERSION` 3 (`TypeHandlerInfo` gains node-config fields). | RM links handlers as Rust crates, not over the C ABI. | Only matters if `ros-madair-python` loads alizarin's C-ABI extension `.so`s: rebuild them at a144. |
| a143: `StaticTile.data` is a `BTreeMap`; output order is stable; `multi_tile_id(namespace, …)`. | Already absorbed in f834255. | None. |

## 3. Using fold for layer spans (optional)

alizarin now does **"reduce a datatype over a set of resources and return a summary."** Spans, roles and
the span nodegroup stay RM's (see `alizarin/AGGREGATE-FUNCTIONS-HANDOFF.md`).

### Summaries available

| Datatype | Summary | Needs |
|---|---|---|
| `number` | `{min, max, count}` | core |
| `date` | `{min, max, count}` (raw strings, ordered as the index orders them) | core |
| `edtf` | same shape; a range contributes both ends; values core can't order yet (`1850?`, `185X`, open ends) are **skipped and counted** | core |
| `geojson-feature-collection` | `{bbox: [min_lng, min_lat, max_lng, max_lat], geometry_types, count}` | **`alizarin-geo-core`** registered |

A bad value fails the fold with an error. An empty input gives no summary (`values: 0`).

### Don't use `fold_resources` in the emitter

`alizarin_core::fold::fold_resources(tiles: &dyn TileSource, ids, model, paths, registry)` needs
random-access tiles per resource. At emit scale, that means a `StaticResourceRegistry` of full resources,
which is the multi-GB OOM that pass 2's streaming removed. Use it only for small slices: tests, or the
on-device Responder over a duck-backed `TileSource`.

### Fold while streaming (pass 2)

The monoid underneath the runner is public: `alizarin_core::datatype_fold::{fold_identity, fold_values,
fold_merge}`. It is order-independent, so per-chunk or per-thread accumulators merge.

```rust
use alizarin_core::datatype_fold::{fold_identity, fold_merge, fold_values};

// Setup, per layer and per folded node (nodeid + datatype; config is the node's JSON config):
let mut acc = fold_identity(datatype, Some(&config), Some(&registry)).expect("datatype has a summary");
let (mut values_seen, mut skipped) = (0usize, 0usize);

// For each streamed resource in the layer:
let values: Vec<&serde_json::Value> = resource.tiles.iter().flatten()
    .filter_map(|t| t.data.get(nodeid))
    .filter(|v| !v.is_null())
    .collect();
values_seen += values.len();
let step = fold_values(datatype, acc, &values, Some(&config), Some(&registry))?; // Err = bad value
acc = step.summary;
skipped += step.skipped;

// Across chunks/threads: acc = fold_merge(datatype, acc_a, acc_b, Some(&config), Some(&registry))?
// At the end: values_seen == 0 means "no data", so write no span.
```

### Steps

1. **Register geo.** Add `alizarin-geo-core = "=2.0.0-alpha.145"` (no geo crate has published since a142,
   so use a `[patch.crates-io]` path to `../alizarin/ext/geo/core` until 145 lands). Call `alizarin_geo_core::register(&mut registry)` in both
   `default_registry()` and `registry_from_declarations()` in `ros-madair-handlers`, so the manifest
   round-trip keeps it.
2. **Accumulate in pass 2.** Keep one accumulator per (layer, folded node) as above. Layer membership and
   the node set are RM's decisions.
3. **Write span tiles.** Use RM's Layer schema (a cardinality-n spans nodegroup). Use
   `alizarin_core::multi_tile_id(Some(layer_namespace), layer_resource_id, spans_nodegroup_id, k)`, with `k`
   from a stable order, e.g. sorted (node, role), so re-emits give the same ids. Each span carries its role
   (an external SKOS concept: `display-extent`, `valid-time`, …), the datatype, the summary and provenance.
4. **Keep `emit/src/geo.rs`.** `extract_bbox` is the per-geometry index bbox. The geo fold is the extent
   across the layer. A test asserting the fold's bbox equals the union of `extract_bbox` over a fixture
   catches drift between them.

**Minimal first slice:** geometry only. Accumulate the geo fold over one layer and write one
`display-extent` span tile. This works today from Rust against the local a144 checkout.

## 4. Not yet available

- **Fold bindings (phase 2).** There is no `foldResources` on wasm, napi or Python yet, and no C-ABI fold.
  RM's Rust crates link core directly, so this doesn't block the emitter. `ros-madair-wasm` and
  `ros-madair-python` would need it to fold through alizarin's bindings.
- **A real EDTF parser** (granularity, uncertain dates), and hull or cell-set geometry summaries.
- **Incremental recompute** when a layer changes.
