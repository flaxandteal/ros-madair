// SPDX-License-Identifier: AGPL-3.0-or-later
//! Records the *actual* version of the `alizarin-clm-core` this build links,
//! as `CLM_CORE_VERSION`, so `declared_handlers()` cannot drift from the
//! handler it really provides. (Cargo gives a crate no env var for a
//! dependency's version, and clm-core exports no `VERSION` const — and we may
//! not edit it.)
//!
//! `alizarin-clm-core` is now a crates.io registry dependency (no sibling
//! checkout to read), so resolve the version from the workspace `Cargo.lock`
//! (exact, for local + CI builds); when this crate is built standalone from the
//! registry there is no workspace lockfile, so fall back to the version
//! requirement declared in our own `Cargo.toml`.
//!
//! Build scripts run on the host, so this costs nothing for the WASM target.

use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let version = clm_version_from_lock(&manifest_dir)
        .or_else(|| clm_requirement_from_own_manifest(&manifest_dir))
        .unwrap_or_else(|| {
            println!(
                "cargo:warning=ros-madair-handlers: could not determine alizarin-clm-core version"
            );
            "unknown".to_string()
        });
    println!("cargo:rustc-env=CLM_CORE_VERSION={version}");
}

/// The exact resolved version from the workspace `Cargo.lock` (workspace builds).
fn clm_version_from_lock(manifest_dir: &Path) -> Option<String> {
    let lock = manifest_dir.join("../../Cargo.lock");
    let text = std::fs::read_to_string(&lock).ok()?;
    println!("cargo:rerun-if-changed={}", lock.display());
    // Walk `[[package]]` blocks; in each, `name` precedes `version`. Capture the
    // version of the block whose name is alizarin-clm-core.
    let mut in_target = false;
    for line in text.lines().map(str::trim) {
        if line == "[[package]]" {
            in_target = false;
        } else if line == "name = \"alizarin-clm-core\"" {
            in_target = true;
        } else if in_target && line.starts_with("version") {
            return line.split('"').nth(1).map(str::to_string);
        }
    }
    None
}

/// The declared version requirement from our own `Cargo.toml` — always present,
/// including when this crate is built from the published registry (no lockfile).
fn clm_requirement_from_own_manifest(manifest_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).ok()?;
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("alizarin-clm-core"))
        .and_then(|l| l.split('"').nth(1).map(str::to_string))
}
