# Quick Start

This guide walks through building an index from Arches heritage data and
running a query in the browser.

## 1. Build an Index

### From an Arches Prebuild Export

If you have a Arches prebuild directory (the standard `starches-builder`
layout with `graphs/` and `business_data/`):

```bash
cargo run --example build_from_prebuild -- \
    /path/to/prebuild \
    example/static/mydata \
    2000
```

Arguments:

| Arg | Meaning |
|-----|---------|
| `/path/to/prebuild` | Arches prebuild export directory |
| `example/static/mydata` | Output directory for the index |
| `2000` | Target resources per page (default: 2000) |

This produces:

```
example/static/mydata/
  summary.bin        # Page-level summary index (~12 MB for 160K resources)
  dictionary.bin     # URI ↔ integer ID mapping
  page_meta.json     # Page metadata (IDs, bounding boxes)
  pages/             # Per-page binary files
    page_0000.dat
    page_0001.dat
    ...
  all.nt             # N-Triples RDF export (for validation)
```

### From Python

```python
from ros_madair import IndexBuilder

builder = IndexBuilder()
builder.add_graph("/path/to/graph.json")
builder.add_business_data("/path/to/business_data/")
builder.build("output/myindex", page_size=2000)
```

## 2. Serve the Files

Any static file server with Range request support works. For development:

```bash
cd example
python3 serve.py 8080 .
```

Open `http://localhost:8080/` in a browser.

!!! note "Range request support"
    The server must support HTTP Range requests (206 Partial Content).
    Most production servers (nginx, Apache, S3, CloudFront) do this
    natively. The included `serve.py` handles it for development.

## 3. Run a Query

### From the Example UI

The example page at `http://localhost:8080/` provides a query input box
and pre-built example queries. Click **Load Index**, then try one of the
example buttons.

### From JavaScript

```javascript
import init, { SparqlStore } from './pkg/ros_madair_client.js';

await init();

const store = new SparqlStore('./static/mydata/');
await store.load_summary();

const results = await store.query_patterns(JSON.stringify([
    {
        s: '?place',
        p: 'https://example.org/node/monument_type_n1',
        o: 'https://example.org/concept/43bf135f-e369-f6f4-a85d-9a54fb1fa44b'
    }
]));

console.log(`Found ${results.length} monuments of type A`);
```

### Query Pattern Format

Patterns are JSON arrays of triple objects:

```json
[
    {"s": "?place", "p": "https://example.org/node/type", "o": "https://example.org/concept/church"},
    {"s": "?place", "p": "https://example.org/node/townland", "o": "https://example.org/concept/ballymena"}
]
```

- Values starting with `?` are **variables** — they match any value
- Other values are **bound URIs** — they match exactly
- Multiple patterns sharing a variable (e.g., `?place`) are intersected

## 4. Generate Validation Data

To verify query correctness, generate ground-truth validation data from
the raw index:

```bash
cargo run --example generate_validation -- example/static/mydata
```

This produces `validation.json` with expected result counts and resource
IDs for each example query. The example UI's **Run Validation** button
compares WASM query results against this ground truth.
