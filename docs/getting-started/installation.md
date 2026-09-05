# Installation

Rós Madair is a Rust workspace. The current stack is six crates:

| Crate | Purpose |
|-------|---------|
| `ros-madair-handlers` | Datatype → index-class classification; the CLM `reference` handler |
| `ros-madair-format` | The snapshot contract (manifest + `FORMAT_VERSION` + snapshot-id derivation) and attestation **verify**; held to `wasm32` |
| `ros-madair-emit` | CLI/library: compile a data directory into the tile-row + edge Parquet substrate + concept catalog + manifest; `sign`/`verify`/`pubkey` |
| `ros-madair-query` | The typed `Query`/`Expr` IR + `ModelCatalog` (discovery) and `explain` — backend-agnostic |
| `ros-madair-duck` | The IR → DuckDB SQL over Parquet, tile-graph hydration, `cited_by`, and reader-side snapshot verify |
| `ros-madair-python` | PyO3 bindings (over `duck`) |

## Prerequisites

- **Rust** (stable) — <https://rustup.rs/>
- **Python 3.10+** — only for the PyO3 bindings (`ros-madair-python`) and the
  docs site.

## The alizarin sibling checkout

Every crate depends on
[`alizarin-core`](https://github.com/flaxandteal/alizarin) via a relative path.
The alizarin repository must be checked out as a sibling directory alongside the
parent `magic/` directory:

```
magic/
  alizarin/                     # <- clone here
  RosMadair-sandbox-parquet/    # this repo
```

## Build and test

```bash
cargo build --release
cargo test --workspace
```

## Python bindings (optional)

```bash
pip install maturin
maturin develop -m crates/ros-madair-python/Cargo.toml
```

This produces an importable extension module exposing the `duck`-backed reader
(`Graph` + `Reader`: resolve, count, resource tiles, hydrate).

## Documentation site

The docs are built with [Zensical](https://github.com/squidfunk/zensical) (the
MkDocs successor):

```bash
pip install -r requirements-docs.txt
zensical serve            # local preview
zensical build --clean    # output in site/
```
