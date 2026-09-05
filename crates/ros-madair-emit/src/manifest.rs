// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writer-side artifact hashing.
//!
//! The manifest *types* and the snapshot-id *derivation* (`hex`, `snapshot_id`,
//! `manifest_digest_bytes`) live in `ros-madair-format` — the ONE copy the
//! reader-side `verify` shares, so a signed id and a recomputed id cannot drift.
//! What remains here is the writer-only half: walking the emitted directory to
//! produce the artifact list the id is computed over.

use std::fs;
use std::path::Path;

use ros_madair_format::{hex, ArtifactEntry};
use sha2::{Digest, Sha256};

use crate::EmitError;

/// Hash the substrate's content files — every `*.parquet` and `*.json` EXCEPT
/// the derived `manifest.json`/`attestations.json` — recursively (a
/// nodegroup-partitioned model is a directory tree of parquet parts), sorted by
/// relative path for a process-stable digest.
///
/// The substrate has no single content-addressed artifact, so its identity IS
/// the set of tile/edge/catalog/graph/sidecar files. This feeds
/// [`ros_madair_format::snapshot_id`]: a hash over the artifact hashes plus the
/// (id-excluded) manifest — the digest signing attests over.
pub(crate) fn hash_parquet_artifacts(out: &Path) -> Result<Vec<ArtifactEntry>, EmitError> {
    let mut entries = Vec::new();
    collect_parquet_artifacts(out, out, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn collect_parquet_artifacts(
    root: &Path,
    dir: &Path,
    out: &mut Vec<ArtifactEntry>,
) -> Result<(), EmitError> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            // Skip the emit scratch and any dotfile dir; recurse partition trees.
            if !name.starts_with('.') {
                collect_parquet_artifacts(root, &path, out)?;
            }
            continue;
        }
        // The manifest and attestations are DERIVED from these files, so they are
        // never part of what the id is computed over (that would be circular).
        if name == "manifest.json" || name == "attestations.json" {
            continue;
        }
        let is_content = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("parquet") | Some("json")
        );
        if !is_content {
            continue;
        }
        let bytes = fs::read(&path)?;
        out.push(ArtifactEntry {
            path: path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/"),
            bytes: bytes.len() as u64,
            sha256: hex(&Sha256::digest(&bytes)),
        });
    }
    Ok(())
}
