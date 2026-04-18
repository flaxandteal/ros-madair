# Rós Madair

A static-file SPARQL query engine for heritage data. Pre-built binary indexes
are served from CDNs or static file hosts and queried entirely in the browser
via WebAssembly — no backend database required.

Typical queries transfer ~2% of the total dataset (~540 KB on a 22 MB /
160K-resource index).

## How It Works

```
alizarin graphs/tiles
        |
        v
  IndexBuilder (Python / Rust)
        |
        +-> dictionary.bin       term <-> u32 ID mapping
        +-> summary.bin          page-level routing index (SPO/PSO/OPS)
        +-> resource_map.bin     dict_id -> page_id O(1) lookup
        +-> page_meta.json       page inventory with bounding boxes
        +-> pages/page_XXXX.dat  per-page binary files (sorted 8-byte records)
                |
                v
        SparqlStore (WASM, in-browser)
          - loads summary + dictionary on init (~22 MB)
          - uses summary to plan which pages to fetch (zero network)
          - HTTP Range requests fetch only needed predicate blocks
          - in-memory join produces results
```

Query execution has four phases:

1. **Init** — load summary + dictionary (once)
2. **Plan** — translate URIs, lookup summary index, intersect page sets (no network)
3. **Fetch** — HTTP Range requests for relevant predicate blocks only (~2% of data)
4. **Execute** — binary search + set intersection in memory

## Crates

| Crate | Purpose |
|-------|---------|
| `ros-madair-core` | Shared data structures, page format, Hilbert curves, quantisation |
| `ros-madair-client` | WASM browser client — planner, fetcher, executor |
| `ros-madair-builder` | PyO3 Python bindings for building indexes from Arches data |

## Quick Start

### Build

```bash
cargo build --release
```

### Build an index from an Arches prebuild

```bash
cargo run --release --example build_from_prebuild -- \
    /path/to/prebuild \
    output/myindex
```

### Build the WASM client

```bash
wasm-pack build crates/ros-madair-client --target web --release --out-dir ../../example/pkg
```

### Query from JavaScript

```javascript
import init, { SparqlStore } from './pkg/ros_madair_client.js';

await init();
const store = new SparqlStore('./static/myindex/');
await store.load_summary();

const results = await store.query_patterns(JSON.stringify([
    { s: '?place', p: 'https://example.org/node/monument_type_n1',
      o: 'https://example.org/concept/43bf135f-e369-f6f4-a85d-9a54fb1fa44b' }
]));
```

### Python builder

```bash
cd crates/ros-madair-builder
pip install maturin
maturin develop --release
```

```python
from ros_madair import IndexBuilder

builder = IndexBuilder()
builder.add_graph("path/to/graph.json")
builder.add_business_data("path/to/business_data/")
builder.build("output/myindex", page_size=2000)
```

## Documentation

Full documentation is available in the `docs/` directory, covering
[installation](docs/getting-started/installation.md),
[data structures](docs/how-it-works/data-structures.md),
[query execution](docs/how-it-works/query-execution.md), and
[optimisations](docs/how-it-works/optimisations.md).

## Dependencies

Rós Madair depends on [alizarin-core](https://github.com/flaxandteal/alizarin)
for Arches graph and tile data structures. The alizarin repository must be
checked out as a sibling directory (see
[installation docs](docs/getting-started/installation.md) for details).

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

Copyright (C) 2026 Flax & Teal Limited.
