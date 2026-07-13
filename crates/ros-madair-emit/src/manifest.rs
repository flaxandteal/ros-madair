// SPDX-License-Identifier: AGPL-3.0-or-later
//! Artifact hashing and snapshot-id derivation.
//!
//! The manifest *types* themselves (the contract — plan M1.1) live in
//! `ros-madair-format`, so a reader (native, Tauri, or WASM in the browser)
//! can parse a manifest without linking the emitter's bundled SQLite writer.
//! This module is the writer-side half only.

use std::fs;
use std::path::{Path, PathBuf};

use ros_madair_format::ArtifactEntry;
use sha2::{Digest, Sha256};

use crate::chunks::hex;
use crate::EmitError;

/// Hash every artifact under `out`; the snapshot id is derived from the
/// hash set. Returns the artifact entries and the snapshot id.
pub(crate) fn hash_artifacts(out: &Path) -> Result<(Vec<ArtifactEntry>, String), EmitError> {
    let mut artifacts = Vec::new();
    let mut hasher_all = Sha256::new();
    let mut paths: Vec<PathBuf> = vec![out.join("head.sqlite"), out.join("closure.json")];
    let mut chunk_paths: Vec<PathBuf> = fs::read_dir(out.join("chunks"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    chunk_paths.sort();
    paths.extend(chunk_paths);
    for path in &paths {
        let bytes = fs::read(path)?;
        let hash = hex(&Sha256::digest(&bytes));
        hasher_all.update(hash.as_bytes());
        artifacts.push(ArtifactEntry {
            path: path
                .strip_prefix(out)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string(),
            bytes: bytes.len() as u64,
            sha256: hash,
        });
    }
    let snapshot_id = hex(&hasher_all.finalize())[..16].to_string();
    Ok((artifacts, snapshot_id))
}
