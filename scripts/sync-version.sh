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

# Update package.json
if [ -f "$ROOT_DIR/package.json" ]; then
    node -e "
        const fs = require('fs');
        const pkg = JSON.parse(fs.readFileSync('$ROOT_DIR/package.json', 'utf8'));
        pkg.version = '$VERSION';
        fs.writeFileSync('$ROOT_DIR/package.json', JSON.stringify(pkg, null, 2) + '\n');
    "
    echo "  ✓ package.json"
fi

# Update pyproject.toml (ros-madair-builder)
if [ -f "$ROOT_DIR/crates/ros-madair-builder/pyproject.toml" ]; then
    sed -i "s/^version = .*/version = \"$PEP440_VERSION\"/" "$ROOT_DIR/crates/ros-madair-builder/pyproject.toml"
    echo "  ✓ crates/ros-madair-builder/pyproject.toml"
fi

echo ""
echo "Version synced to $VERSION (Cargo: $CARGO_VERSION, PEP 440: $PEP440_VERSION)"
