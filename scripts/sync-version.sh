#!/bin/bash
# Sync version from VERSION file to all package manifests
# Usage: ./scripts/sync-version.sh [version]
# If version is provided, update VERSION file first

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

# If version provided as argument, update VERSION file
if [ -n "$1" ]; then
    echo "$1" > "$ROOT_DIR/VERSION"
fi

VERSION=$(cat "$ROOT_DIR/VERSION" | tr -d '\n')

if [ -z "$VERSION" ]; then
    echo "Error: VERSION file is empty or missing"
    exit 1
fi

echo "Syncing version $VERSION across all packages..."

# Cargo-compatible version (Rust accepts semver with prerelease)
CARGO_VERSION="$VERSION"

# PEP 440 format: 0.1.0-alpha.4 -> 0.1.0a4
PEP440_VERSION=$(echo "$VERSION" | sed 's/-alpha\./a/' | sed 's/-beta\./b/' | sed 's/-rc\./rc/')

# Update root Cargo.toml (workspace package version)
if [ -f "$ROOT_DIR/Cargo.toml" ]; then
    sed -i "0,/^version = /s/^version = .*/version = \"$CARGO_VERSION\"/" "$ROOT_DIR/Cargo.toml"
    echo "  ✓ Cargo.toml (workspace)"
fi

# Update the inter-crate path-dep versions (path is used for local builds; the version
# is required for `cargo publish` to crates.io — keep it in lockstep with the workspace).
for f in "$ROOT_DIR"/crates/ros-madair-*/Cargo.toml; do
    [ -f "$f" ] || continue
    sed -i -E "s#(ros-madair-[a-z]+ = \{ path = \"[^\"]+\", version = \")[^\"]+\"#\1$CARGO_VERSION\"#g" "$f"
done
echo "  ✓ inter-crate dep versions"

# (No root npm package: the v1 client's package.json/package-lock/vite stack was
# removed with the dead js/ glue. The browser runtime is now ros-madair-wasm, whose
# npm package version comes from its Cargo.toml via wasm-pack — bumped by the Cargo
# version above, so there is nothing extra to sync here.)

# pyproject.toml (ros-madair-python) needs NO version edit: it declares
# `dynamic = ["version"]`, so maturin takes the version from the crate's Cargo.toml
# (`version.workspace = true` → the workspace version set above). Keeping a single
# source of truth is what stops the wheel drifting behind crates/npm (the a24-on-PyPI
# bug). The PEP 440 form is still computed for the log line / any external tooling.
echo "  ✓ crates/ros-madair-python/pyproject.toml (dynamic — tracks Cargo, no edit)"

echo ""
echo "Version synced to $VERSION (Cargo: $CARGO_VERSION, PEP 440: $PEP440_VERSION)"
