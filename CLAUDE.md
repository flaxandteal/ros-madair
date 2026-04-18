# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Rós Madair?

A static-file-based SPARQL query engine for heritage data. Pre-built binary indexes are served from CDNs/static hosts and queried entirely in the browser via WASM — no backend database required. Typical queries transfer ~2% of the total dataset (~540 KB on a 22 MB / 160K resource dataset).

## Build Commands

### Core library (check/test)
```bash
cargo check                          # type-check all crates
cargo test                           # run all tests
cargo test -p ros-madair-core        # test core crate only
```

### WASM client (browser runtime)
```bash
wasm-pack build crates/ros-madair-client --target web --release --out-dir ../../example/pkg
```
Requires `wasm-pack` and the `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`).

### PyO3 builder (index generation)
```bash
maturin develop -m crates/ros-madair-builder/Cargo.toml
```
Produces a Python extension module named `ros_madair`.

### Examples (integration-test style)
```bash
cargo run --example build_example                     # synthetic heritage data
cargo run --release --example build_from_prebuild -- <prebuild_path> <output_dir>
cargo run --example diagnose -- <index_dir>           # validate page files
```

### Documentation site
```bash
pip install -r requirements-docs.txt   # installs zensical (MkDocs successor)
zensical build --clean                 # output in site/
zensical serve                         # local preview
```

## Architecture

Three-crate Rust workspace:

```
ros-madair-core     Pure Rust library — all index data structures and algorithms
ros-madair-builder  PyO3 bindings — build-time index generation from alizarin data
ros-madair-client   WASM (wasm-bindgen) — browser-side SPARQL query execution
```

Both `builder` and `client` depend on `core`. They never depend on each other.

### External dependency: alizarin-core

All three crates ultimately depend on `alizarin-core` (heritage graph/tile ORM), referenced via relative path `../../../alizarin/crates/alizarin-core`. The alizarin repo must be checked out as a sibling at `../../../alizarin/` relative to each crate (i.e. alongside the parent `magic/` directory).

### Data flow

```
alizarin graphs/tiles
        │
        ▼
  IndexBuilder (PyO3 / examples)
        │
        ├─► dictionary.bin     term ↔ u32 ID mapping
        ├─► summary.bin        page-level routing index (SPO/PSO/OPS sorted)
        ├─► resource_map.bin   dict_id → page_id O(1) lookup
        ├─► page_meta.json     page inventory with bbox/resource count
        └─► pages/page_XXXX.dat  per-page binary files (sorted 8-byte records)
                │
                ▼
        SparqlStore (WASM)
          - loads summary + dictionary + resource_map on init
          - uses summary to plan which pages to fetch
          - HTTP Range requests fetch only needed predicate blocks
          - in-browser join produces results
```

### Core modules (ros-madair-core)

| Module | Purpose |
|--------|---------|
| `dictionary.rs` | Bidirectional term ↔ u32 encoding |
| `quantize.rs` | Value quantization (DictionaryId, Boolean, DaysSinceEpoch, HilbertGeo) into fixed 8-byte `PageRecord`s |
| `page_assignment.rs` | Groups resources into pages using Hilbert curve locality (graph_id → geo+concept sort) |
| `page_file.rs` | Binary page format: header + predicate blocks of sorted (object_val, subject_id) records |
| `summary_quads.rs` | Page-level routing: (page_s, pred, page_o, edge_count, subject_count) in 3 sort orders |
| `rdf_export.rs` | alizarin graphs/tiles → RDF triples → N-Triples serialization |
| `geo_convert.rs` | GeoJSON → WKT, centroid extraction |
| `hilbert.rs` | 3D Hilbert curve for spatial+semantic page locality |
| `resource_map.rs` | O(1) dict_id → page_id lookup table |

### Client modules (ros-madair-client)

| Module | Purpose |
|--------|---------|
| `lib.rs` | `SparqlStore` WASM class — main entry point |
| `fetch.rs` | HTTP full/range fetches for surgical data retrieval |
| `page_cache.rs` | Tracks loaded pages/predicates to avoid re-fetching |
| `planner.rs` | Summary-driven query planning — determines which pages to fetch |

### Key design decisions

- **Fixed-width quantization**: All values encoded as `(object_val: u32, subject_id: u32)` = 8 bytes. Enables binary search within predicate blocks. Quantization strategy (exact ID, date, boolean, geo) is determined by RDF datatype.
- **Summary-driven planning**: The client never fetches pages blindly. The summary index (~1-3 MB) routes queries to exact pages, cutting fetches by 60-70%.
- **Hilbert curve locality**: Resources grouped by (lng, lat, concept_type) via 3D Hilbert curve so pages contain geographically and semantically coherent subsets.
- **HTTP Range requests**: Only predicate block headers + relevant blocks are fetched, not entire page files.

## Binary format magic bytes

- Page file: `RMPG`
- Dictionary: starts with magic + entry_count
- Resource map: `RMRM`
- Summary: 3 sorted copies of 20-byte quads

## Example: Aonach Mór

`example/aonach_mor/` contains a city-resilience demo built on real Chennai geography with fictional labels. It has a full build pipeline (`build/01–15_*.py`) generating Arches resource models and business data, with deployable output in `static/`.

## CI

GitHub Actions workflow (`.github/workflows/deploy-pages.yml`) builds docs + WASM client + Arches index and deploys to GitHub Pages. Manual trigger only by default.

## License

AGPL-3.0-or-later (Flax & Teal Limited).
