# Builder API

The `ros_madair` Python package provides the `IndexBuilder` class for
generating static index files from Arches graph data.

## Installation

```bash
cd crates/ros-madair-builder
pip install maturin
maturin develop --release
```

This installs the `ros_madair` Python package.

## `IndexBuilder`

### Constructor

```python
from ros_madair import IndexBuilder

builder = IndexBuilder(base_uri="https://example.org/")
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `base_uri` | `str` | Base URI for generated RDF identifiers. Resource URIs become `{base_uri}resource/{id}`, concept URIs become `{base_uri}concept/{id}`, etc. |

### `add_graph(graph_json)`

Register a graph (resource model) definition.

```python
with open("heritage_place.json") as f:
    builder.add_graph(f.read())
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `graph_json` | `str` | JSON string of an Arches graph definition (the format produced by `graphs/` in a prebuild export). Must include `graphid`, `nodes`, and `edges`. |

**Raises:** `ValueError` if the JSON is invalid or missing required fields.

The graph is parsed and its internal indices are built immediately.

### `add_resources(graph_id, resources_json)`

Add resources (with their tiles) for a previously registered graph.

```python
with open("heritage_place_resources.json") as f:
    builder.add_resources("22477f01-1c40-11ea-b786-3af9d3b32b71", f.read())
```

| Parameter | Type | Description |
|-----------|------|-------------|
| `graph_id` | `str` | UUID of the graph these resources belong to (must match a previously added graph). |
| `resources_json` | `str` | JSON array of resource objects. Each must have `resourceinstanceid` (or `resourceinstance_id`) and optionally `tiles` (array of tile objects with `data` maps). |

**Raises:** `ValueError` if the JSON is invalid or resources are missing IDs.

### `build(output_dir, page_size=None)`

Build the index and write all output files.

```python
builder.build("./static/mydata/", page_size=2000)
```

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `output_dir` | `str` | — | Directory to write output files. Created if it doesn't exist. |
| `page_size` | `int` or `None` | `2000` | Target number of resources per page. Larger values mean fewer pages (fewer HTTP requests) but more false positives per page. |

**Raises:** `ValueError` on I/O errors or serialization failures.

**Output files:**

| File | Description |
|------|-------------|
| `summary.bin` | Page-level summary quad index (~12 MB for 160K resources) |
| `dictionary.bin` | URI/literal ↔ u32 ID mapping (~15 MB for 200K terms) |
| `page_meta.json` | Page metadata (IDs, resource counts, bounding boxes) |
| `pages/page_XXXX.dat` | Per-page binary files with predicate-partitioned records |
| `all.nt` | Full N-Triples RDF export (for validation with external SPARQL engines) |

## Rust CLI Alternative

For building from an Arches prebuild export without Python:

```bash
cargo run --example build_from_prebuild -- \
    /path/to/prebuild \
    output/mydata \
    2000
```

| Argument | Description |
|----------|-------------|
| Path 1 | Arches prebuild directory (with `graphs/` and `business_data/`) |
| Path 2 | Output directory |
| Number | Target resources per page (default: 2000) |

The CLI produces the same output files as `IndexBuilder.build()`.

## Indexed Datatypes

Not all Arches datatypes are indexed. The builder indexes:

| Arches Datatype | Index Strategy | Notes |
|----------------|---------------|-------|
| `concept` | Dictionary ID lookup | Single concept reference |
| `concept-list` | Dictionary ID per item | Each concept in the list becomes a separate record |
| `concept-value` | Dictionary ID lookup | Same as `concept` |
| `resource-instance` | Dictionary ID lookup | Cross-resource reference |
| `resource-instance-list` | Dictionary ID per item | Each reference becomes a separate record |
| `boolean` | Direct quantization | `0` or `1` |
| `date` | Days since epoch | u32 day count |
| `geojson-feature-collection` | 2D Hilbert point | Centroid extracted, encoded as Hilbert index |

Other datatypes (`string`, `number`, `url`, `file-list`, `domain-value`,
`domain-value-list`, `edtf`, `node-value`, `annotation`, `non-localized-string`,
`semantic`) are included in the N-Triples RDF export but are **not indexed**
in the binary page files.

## Full Example

```python
import os
import json
from ros_madair import IndexBuilder

PREBUILD = "/path/to/arches-prebuild"
OUTPUT = "./static/index"

builder = IndexBuilder(base_uri="https://example.org/")

# Load all graphs
for filename in os.listdir(os.path.join(PREBUILD, "graphs")):
    if filename.endswith(".json"):
        with open(os.path.join(PREBUILD, "graphs", filename)) as f:
            builder.add_graph(f.read())

# Load business data (resources with tiles)
bd_dir = os.path.join(PREBUILD, "business_data")
for filename in os.listdir(bd_dir):
    if filename.endswith(".json"):
        with open(os.path.join(bd_dir, filename)) as f:
            data = json.load(f)
        graph_id = data["business_data"]["resources"][0]["resourceinstance"]["graph_id"]
        resources_json = json.dumps(data["business_data"]["resources"])
        builder.add_resources(graph_id, resources_json)

# Build with default page size (2000)
builder.build(OUTPUT)
print(f"Index written to {OUTPUT}")
```
