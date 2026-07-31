# Installation

Rós Madair is a Rust workspace. The current stack is six crates:

| Crate | Purpose |
|-------|---------|
| `ros-madair-handlers` | Datatype → index-class classification; the CLM `reference` handler |
| `ros-madair-format` | On-disk artifact format — manifest + versioned chunk framing (WASM-safe) |
| `ros-madair-emit` | CLI: compile a data directory into head + chunks + manifest |
| `ros-madair-query` | Head-schema query compiler (`Concept` / `Range` / `Bbox` / `HasLink` → SQL) |
| `ros-madair-read` | Native read path — resolve, hydrate, layered overlay, reverse traversal |
| `ros-madair-python` | PyO3 bindings (`compile_query`, `hydrate_*`) |

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

This produces an importable extension module exposing `compile_query` and the
`hydrate_*` entry points.

## Documentation site

The docs are built with [Zensical](https://github.com/squidfunk/zensical) (the
MkDocs successor):

```bash
pip install -r requirements-docs.txt
zensical serve            # local preview
zensical build --clean    # output in site/
```
