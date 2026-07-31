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

The output directory contains:

```
<out_dir>/
  head.sqlite       indexed spine / concept / value / geo / link tables
  chunks/*.msgpack  content-hashed tile detail
  manifest.json     layout + format-version contract
```

### Tiers (audience-scoped artifacts)

`--tier <name>:<config.json>` emits a named tier as its own complete artifact
under `<out_dir>/<name>/`, applying `exclude_nodegroups` / `exclude_models`
before indexing so excluded data appears in **no** artifact of that tier — a
build-time way to keep restricted data off a public host entirely.

## 4. Read

The `ros-madair-read` crate resolves a query against the head and hydrates the
matching tiles into a schema-shaped tree. It is exercised end to end by the
crate's integration tests (`crates/ros-madair-read/tests/`), which emit a real
snapshot with the real emitter and read it back — the canonical worked examples
of the query + hydrate path, including range (`ordered.rs`), spatial
(`spatial.rs`), overlay (`layers.rs`), and reverse traversal (`reverse.rs`).

## Where this is heading

The emitter also writes an additive tile-row **Parquet** layout
(`ros-madair-emit`'s `parquet` module), the first slice of the DuckDB + Parquet
substrate that replaces the coarse/fine read engine. See the
[README's Direction section](https://github.com/flaxandteal/ros-madair#direction-duckdb--parquet-substrate).
