// SPDX-License-Identifier: AGPL-3.0-or-later
//! Records the *actual* version of the `alizarin-clm-core` this build links,
//! as `CLM_CORE_VERSION`, so `declared_handlers()` cannot drift from the
//! handler it really provides. (Cargo gives a crate no env var for a
//! dependency's version, and clm-core exports no `VERSION` const — and we may
//! not edit it — so read its manifest, following workspace inheritance.)
//!
//! Build scripts run on the host, so this costs nothing for the WASM target.

use std::path::{Path, PathBuf};

fn version_line(text: &str) -> Option<String> {
    // First `version = "…"` in the [package] / [workspace.package] table.
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("version") && l.contains('"'))
        .and_then(|l| l.split('"').nth(1).map(str::to_string))
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // alizarin alpha.128+ moved CLM from crates/alizarin-clm-core to
    // ext/alizarin-clm/core, and RM now deps on ../alizarin (not the sandbox).
    let clm = manifest_dir.join("../../../alizarin/ext/alizarin-clm/core/Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let version = read_version(&clm).unwrap_or_else(|| {
        println!("cargo:warning=ros-madair-handlers: could not read alizarin-clm-core version");
        "unknown".to_string()
    });
    println!("cargo:rustc-env=CLM_CORE_VERSION={version}");
}

fn read_version(clm_manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(clm_manifest).ok()?;
    println!("cargo:rerun-if-changed={}", clm_manifest.display());
    if text.contains("version.workspace") || text.contains("version = { workspace") {
        // Inherited: climb to the alizarin workspace root manifest.
        let root = clm_manifest.parent()?.join("../../Cargo.toml");
        let root_text = std::fs::read_to_string(&root).ok()?;
        println!("cargo:rerun-if-changed={}", root.display());
        let pkg = root_text.split("[workspace.package]").nth(1)?;
        version_line(pkg)
    } else {
        version_line(&text)
    }
}
