# Plan: Multi-Layer Integration — Adding & Consuming Layers

## Overview

Rós Madair now supports multi-layer indexes: a **base layer** (e.g. a dictionary of Words) plus **corpus overlays** (e.g. Examples, Audio, Bibliography that reference base Words). Each layer is an independently built index with its own dictionary, summary, pages, and resource map. Queries can run across all layers transparently, with optional layer filtering.

This document covers:
1. How to **build** a corpus layer
2. How to **load** it across all three consumers (WASM, PyO3, local Rust)
3. **Client code changes** still needed for full layer support

---

## Part 1: Building a Corpus Layer

A corpus layer is just a regular Rós Madair index built from a subset of resources. Nothing special is needed at build time — shadow pages are automatically created for external references.

### PyO3 (Python)

```python
import ros_madair

# Build base layer (e.g. Words)
base = ros_madair.IndexBuilder("https://example.org/")
base.add_graph(words_graph_json)
base.add_resources("word-graph-id", words_json)
base.add_vocabulary_xml(skos_xml, "https://example.org/")
base.build("/output/base")

# Build corpus layer (e.g. Examples that reference Words)
corpus = ros_madair.IndexBuilder("https://example.org/")
corpus.add_graph(examples_graph_json)
corpus.add_resources("example-graph-id", examples_json)
# Vocabulary can be shared or omitted if the base layer has it
corpus.add_vocabulary_xml(skos_xml, "https://example.org/")
corpus.build("/output/corpus")
```

Shadow pages are created automatically: if an Example tile contains a `ResourceInstance` reference to a Word UUID that isn't in the corpus index, `assign_shadow_pages()` assigns that external UUID to a shadow page so reverse predicates (`!pred`) are emitted.

### Rust binary (`ros-madair-build`)

Same principle — run the build binary once per layer with different input data and output directories.

### What goes into each layer's output

Each layer produces the standard set of files:
- `dictionary.bin`, `summary.bin`, `resource_map.bin`, `page_meta.json`
- `pages/page_XXXX.dat`, `tiles/tile_XXXX.dat`
- Optionally: `concept_intervals.bin`, `concept_tree.bin`, `concept_hierarchy.json`
- `all.nt` (RDF export)

---

## Part 2: Loading Layers in Each Consumer

### A. WASM Client (`SparqlStore`)

```javascript
// 1. Create store (base_url is now ignored in constructor — load_summary takes it)
const store = new SparqlStore("ignored");

// 2. Load base layer
await store.loadSummary("https://cdn.example.org/base/");

// 3. Add corpus overlay(s)
await store.addLayer("https://cdn.example.org/corpus/", "examples");
await store.addLayer("https://cdn.example.org/audio/", "audio");

// 4. Query across all layers (no layer filter)
const results = await store.queryPatterns(patternsJson);

// 5. Query a specific layer only
const results = await store.queryPatterns(patternsJson, "examples");

// 6. Introspection
store.layerCount();  // 3
store.layerNames();  // ["base", "examples", "audio"]
```

### B. PyO3 (`IndexReader`)

```python
import ros_madair

# 1. Open base layer
reader = ros_madair.IndexReader("/output/base", "https://example.org/")

# 2. Add corpus overlay(s)
reader.add_layer("/output/corpus", "https://example.org/", "examples")

# 3. Query across all layers
resource_ids = reader.query("references", None)  # no layer filter

# 4. Query a specific layer
resource_ids = reader.query("references", None, layer="examples")

# 5. Compound query with layer filter
resource_ids = reader.query_compound(patterns_json, layer="examples")

# 6. Introspection
reader.layer_count()  # 2
```

### C. Local Rust (`LocalQueryEngine`)

```rust
// 1. Open base layer
let mut engine = LocalQueryEngine::open(Path::new("/output/base"), "https://example.org/")?;

// 2. Add corpus overlay(s)
engine.add_layer(Path::new("/output/corpus"), "https://example.org/", "examples")?;

// 3. Query across all layers (returns full URIs)
let uris = engine.query_patterns_multi(&patterns, None)?;

// 4. Query specific layer
let uris = engine.query_patterns_multi(&patterns, Some("examples"))?;

// 5. Base-layer-only query (returns dict IDs — backward compat)
let ids = engine.query_patterns(&patterns)?;
```

---

## Part 3: Client Code Changes Needed

### 3.1 SparqlStore API Breaking Change

**Problem**: The `SparqlStore` constructor signature changed. Previously:
```javascript
const store = new SparqlStore("https://cdn.example.org/base/");
await store.loadSummary();  // used the URL from constructor
```

Now:
```javascript
const store = new SparqlStore("https://cdn.example.org/base/");  // URL is IGNORED
await store.loadSummary("https://cdn.example.org/base/");        // URL must be passed here
```

The constructor stores an empty `layers: Vec<Layer>` and discards `base_url`. If `loadSummary()` is called without an argument, it will use `""` as the base URL.

**Fix needed** (`crates/ros-madair-client/src/lib.rs`):

Option A — Store `base_url` from constructor and use as default in `load_summary`:
```rust
pub struct SparqlStore {
    layers: Vec<Layer>,
    default_base_url: Option<String>,  // from constructor
}

#[wasm_bindgen]
impl SparqlStore {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: &str) -> Self {
        Self {
            layers: Vec::new(),
            default_base_url: Some(base_url.to_string()),
        }
    }

    pub async fn load_summary(&mut self, base_url: Option<String>) -> Result<(), JsValue> {
        let url = base_url
            .or_else(|| self.default_base_url.take())
            .unwrap_or_default();
        // ... existing logic using `url`
    }
}
```

Option B — Make `load_summary` require the URL (breaking change, update all callers). This is cleaner but requires updating all JS call sites.

**Recommendation**: Option A for backward compatibility.

### 3.2 Alizarin Glue — Hardcoded Layer 0

**File**: `crates/ros-madair-alizarin/src/lib.rs`

Three methods call `store.dictionary(0)`, `store.resource_map(0)`, `store.base_url(0)`:

- `connect_tile_source()` — creates tile source from base layer only
- `prefetch_tiles_for_resource()` — fetches tiles from base layer only

**What to change**: For tile prefetching, the resource may live in any layer. Use the store's `find_resource()` method to determine which layer holds the resource, then use that layer's dictionary/resource_map/base_url.

```rust
#[wasm_bindgen]
pub async fn prefetch_tiles_for_resource(
    handle: &TileSourceHandle,
    store: &SparqlStore,
    resource_uri: &str,
) -> Result<(), JsValue> {
    // Use find_resource to locate the correct layer
    let (layer_index, dict_id, page_id) = store.find_resource(resource_uri)?;

    if handle.source.has_tile_file(page_id) {
        return Ok(());
    }

    let base_url = store.base_url(layer_index)
        .ok_or_else(|| JsValue::from_str("Layer not loaded"))?;
    let tile_url = format!("{}tiles/tile_{:04}.dat", base_url, page_id);

    let bytes = ros_madair_client::fetch::fetch_full(&tile_url).await
        .map_err(|e| JsValue::from_str(&e))?;

    handle.source.insert_tile_file(page_id, bytes);
    Ok(())
}
```

**Note**: `find_resource` is currently `fn` (non-pub, Rust-only). It needs to be made `pub` or the alizarin crate needs access. Since `ros-madair-alizarin` already depends on `ros-madair-client`, making `find_resource` `pub` on `SparqlStore` is the path of least resistance.

For `connect_tile_source`, the tile source needs dict + rmap to resolve resource URIs to pages. With multiple layers, either:
- The `GrowableTileSource` needs to support multiple dictionaries/resource maps, or
- `connect_tile_source` is called per-layer, or
- The tile source is populated lazily via `prefetch_tiles_for_resource` (which already handles multi-layer)

**Recommendation**: Keep `connect_tile_source` using base layer for now. Fix `prefetch_tiles_for_resource` to use `find_resource`. The tile source's dict/rmap are only used for `has_tile_file` and `insert_tile_file` which operate on page IDs — the page ID is already resolved by `find_resource`.

### 3.3 IndexReader — Base-Layer-Only Methods

Several `IndexReader` methods only operate on the base layer. For full multi-layer support, these need layer-aware variants:

| Method | Current | Needed |
|--------|---------|--------|
| `read_tile_blob(resource_id)` | Base only | Search all layers (like WASM `loadTilesForResource`) |
| `read_tiles_json(resource_id)` | Base only | Same |
| `page_for_resource(resource_id)` | Base only | Return `(page_id, layer_name)` or search all |
| `list_resource_ids()` | Base only | Accept optional layer filter |
| `resources_for_page(page_id)` | Base only | Accept optional layer index |
| `resolve_term(dict_id)` | Base only | Accept optional layer index |
| `lookup_term(term)` | Base only | Search all layers |

**Implementation approach** for each:

#### `read_tile_blob` / `read_tiles_json`

Build the engine (which loads all layers), then search each layer's dictionary + resource map for the resource. Read the tile file from whichever layer owns it.

```rust
#[pyo3(signature = (resource_id, layer=None))]
fn read_tile_blob(&self, resource_id: &str, layer: Option<&str>) -> PyResult<Option<Vec<u8>>> {
    // Try base layer first
    if layer.is_none() || layer == Some("base") {
        if let result @ Some(_) = self.read_tile_blob_from_layer(
            resource_id, &self.index_dir, &self.dict, &self.resource_map, &self.base_uri
        )? {
            return Ok(result);
        }
    }
    // Try extra layers
    for (dir, base_uri, name) in &self.extra_layers {
        if layer.is_some() && layer != Some(name.as_str()) {
            continue;
        }
        // Load dict + rmap for this layer (or cache them)
        // ... search and return if found
    }
    Ok(None)
}
```

**Trade-off**: This requires loading each extra layer's dictionary and resource map to look up the resource. Currently extra layers are lazy-loaded. Two approaches:
1. Eagerly load dict + rmap for all layers at `add_layer()` time (more memory, faster lookups)
2. Load on demand in `read_tile_blob` (slower first call, less memory)

**Recommendation**: Eagerly load dict + rmap in `add_layer()`. Store them alongside the path:

```rust
struct ExtraLayer {
    index_dir: PathBuf,
    base_uri: String,
    name: String,
    dict: Dictionary,
    resource_map: ResourceMap,
}
```

This keeps layer loading simple and makes tile/resource lookups work without rebuilding the engine.

#### `list_resource_ids`

```rust
#[pyo3(signature = (layer=None))]
fn list_resource_ids(&self, layer: Option<&str>) -> Vec<String> {
    // If layer specified, list only from that layer
    // If None, list from all layers
}
```

#### `page_for_resource`

```rust
#[pyo3(signature = (resource_id))]
fn page_for_resource(&self, resource_id: &str) -> Option<(u32, String)> {
    // Returns (page_id, layer_name), searches all layers
}
```

### 3.4 Clódóir Integration

**File**: `../Clódóir/clodoir/rosmadair_provider.py`

Currently `RosMadairProvider.__init__` creates a single `IndexReader`. For multi-layer:

```python
class RosMadairProvider:
    def __init__(self, data_dir: Path, overlays: list[tuple[Path, str, str]] = None):
        self._reader = ros_madair.IndexReader(str(data_dir), base_uri)
        for overlay_dir, overlay_uri, overlay_name in (overlays or []):
            self._reader.add_layer(str(overlay_dir), overlay_uri, overlay_name)
```

**Query changes**: Pass `layer` parameter when the caller wants to restrict queries:

```python
def query_resources(self, slug, path, filter_value=None, layer=None):
    resource_ids = self._reader.query(pred_alias, obj_uri, layer=layer)
```

**Store changes** (`clodoir/store.py`): The `ClodoirStore` orchestrates overlays. It would configure `RosMadairProvider` with overlay paths during initialization, based on the overlay configuration in `store.yaml` or similar.

### 3.5 `hasResourceMap` — WASM

**File**: `crates/ros-madair-client/src/lib.rs`

Currently checks only the first layer:
```rust
pub fn has_resource_map(&self) -> bool {
    self.layers.first().map_or(false, |l| l.resource_map.is_some())
}
```

Should check all layers or accept a layer index. Low priority — mostly used for feature detection.

### 3.6 LocalQueryEngine — Convenience Method Gaps

`query_predicate()` and `query_patterns()` are base-layer-only. They're used by `IndexReader.query()` in the single-layer fast path. No change strictly needed, but for API consistency, consider adding a `query_predicate_multi()` or just having `IndexReader` always use `query_patterns_multi()` (removing the fast path).

**Recommendation**: Keep the fast path. The single-layer case is the common case and avoids URI resolution overhead.

---

## Part 4: Priority Order for Implementation

### P0 — Must fix (broken or confusing API)

1. **SparqlStore constructor backward compat** (§3.1): Store `base_url` from constructor, use as default in `load_summary`. ~15 lines changed.

2. **`find_resource` visibility** (§3.2): Make `SparqlStore::find_resource` `pub` so alizarin can use it. 1 line changed.

3. **`prefetch_tiles_for_resource` multi-layer** (§3.2): Use `find_resource` to determine correct layer. ~10 lines changed.

### P1 — Should fix (functional gaps)

4. **IndexReader tile reading across layers** (§3.3): Make `read_tile_blob`/`read_tiles_json` search all layers. Requires eagerly loading extra layer dict + rmap in `add_layer()`. ~60 lines.

5. **IndexReader `page_for_resource` multi-layer** (§3.3): Search all layers, return layer name. ~15 lines.

6. **Clódóir `add_layer` plumbing** (§3.4): Wire overlay config through to `IndexReader.add_layer()`. ~20 lines in Python.

### P2 — Nice to have (completeness)

7. **IndexReader `list_resource_ids` with layer filter** (§3.3): ~10 lines.
8. **IndexReader `resolve_term` / `lookup_term` layer-aware** (§3.3): ~15 lines.
9. **`hasResourceMap` layer-aware** (§3.5): ~5 lines.
10. **Concept tree access for extra layers**: Only meaningful if corpus layers have their own vocabulary. Low priority — typically shared.

---

## Part 5: Testing Checklist

After implementing changes:

- [ ] `cargo check` — all 4 crates compile
- [ ] `cargo test` — all existing tests pass (109+)
- [ ] Backward compat: `SparqlStore("url")` + `loadSummary()` still works (no arg)
- [ ] WASM: `addLayer()` + `queryPatterns()` cross-layer returns results from both layers
- [ ] WASM: `queryPatterns(json, "layerName")` restricts to named layer
- [ ] WASM: `prefetch_tiles_for_resource()` fetches from correct layer's CDN path
- [ ] PyO3: `IndexReader.add_layer()` + `query()` works
- [ ] PyO3: `read_tiles_json()` finds resource in non-base layer
- [ ] Integration: build base + corpus indexes, load both, query reverse predicates across shadow pages
- [ ] Clódóir: overlay config loads correctly, queries return cross-layer results
