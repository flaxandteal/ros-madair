# Quick Start

This guide emits static artifacts from Arches heritage data with the
`ros-madair-emit` CLI, then reads them back.

## 1. Build

```bash
cargo build --release
```

## 2. Arrange a data directory

`ros-madair-emit` consumes a data directory following the alizarin/Clódóir
convention:

```
<data_dir>/
  graphs/*.json          resource models
  resources/**/*.json    business data
  vocabularies/*.xml     SKOS (optional)
```

## 3. Emit

```bash
cargo run -p ros-madair-emit --release -- <data_dir> <out_dir> [--base-uri URI]
```

The output directory contains the Parquet substrate:

```
<out_dir>/
  tiles_<slug>.parquet    tile = row; promoted zone-mapped columns + a `data` JSON blob
  edges_<slug>.parquet    one row per link target
  concept_catalog.parquet  concept DFS intervals
  manifest.json           layout + FORMAT_VERSION contract; signed snapshot_id
```

### Sign and verify

```bash
cargo run -p ros-madair-emit -- sign <out_dir> <key_path>   # attests the snapshot_id
cargo run -p ros-madair-emit -- verify <out_dir>            # verified / unsigned / tampered
```

## 4. Read

`ros-madair-duck` opens the snapshot, compiles the `Query` IR to DuckDB SQL,
resolves matching resource ids, and hydrates their tiles into a schema-shaped
tree — verifying the manifest signature first. The fastest way to see the whole
flow is the self-contained example (no data files, no network):

```bash
cargo run -p ros-madair-duck --example substrate_end_to_end
```

It writes a tiny data_dir, emits + signs it, verifies the snapshot, opens it,
runs `Range`/`Bbox` queries, and hydrates a match. The read path is also
exercised end to end by `crates/ros-madair-duck/tests/` — `behavioral.rs` (every
`Expr` variant, `OnLink` multi-hop, layered overlay), `verify.rs` (the sign→verify
loop), and `reference_labels.rs` (label-resolved hydration).

## Where this is heading

The remaining slice is the browser runtime: run the same SQL `duck` compiles as
DuckDB-WASM. See the
[README's Direction section](https://github.com/flaxandteal/ros-madair#direction-duckdb--parquet-substrate).
