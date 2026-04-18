# Installation

## Prerequisites

- **Rust** (stable, 1.75+) — [rustup.rs](https://rustup.rs)
- **wasm-pack** — `cargo install wasm-pack`
- **Python 3.10+** — for the builder and dev server

## Clone and Build

```bash
git clone https://github.com/flaxandteal/ros-madair.git
cd ros-madair
```

### Build the Core Crates

```bash
cargo build --release
```

This builds three crates:

| Crate | Purpose |
|-------|---------|
| `ros-madair-core` | Shared data structures, page format, Hilbert curves, quantisation |
| `ros-madair-client` | WASM browser client — planner, fetcher, executor |
| `ros-madair-builder` | PyO3 Python bindings for building indexes from Arches data |

### Build the WASM Client

```bash
wasm-pack build crates/ros-madair-client --target web --out-dir ../../example/pkg
```

This produces the WASM module and JS bindings in `example/pkg/`.

### Install the Python Builder (Optional)

If you want to build indexes from Python (e.g., from an Arches export):

```bash
cd crates/ros-madair-builder
pip install maturin
maturin develop --release
```

This installs the `ros_madair` Python package with the `IndexBuilder` class.

## Verify

```bash
# Run the core test suite
cargo test --workspace

# Check the WASM builds correctly
ls example/pkg/ros_madair_client_bg.wasm
```

## Next Steps

See the [Quick Start](quickstart.md) to build an index and run your first query.
