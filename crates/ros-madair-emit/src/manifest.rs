// SPDX-License-Identifier: AGPL-3.0-or-later
//! Artifact hashing and snapshot-id derivation.
//!
//! The manifest *types* themselves (the contract — plan M1.1) live in
//! `ros-madair-format`, so a reader (native, Tauri, or WASM in the browser)
//! can parse a manifest without linking the emitter's bundled SQLite writer.
//! This module is the writer-side half only.

use std::fs;
use std::path::Path;

use ros_madair_format::{ArtifactEntry, Manifest};
use sha2::{Digest, Sha256};

use crate::chunks::hex;
use crate::EmitError;

/// Hash the head — and ONLY the head (A6).
///
/// The chunks are content-addressed (`chunks/<sha256>.msgpack`) and every one of
/// those hashes is stored INSIDE `head.sqlite` (the `chunks` table). So hashing
/// the head already covers the whole chunk set transitively: change a chunk's
/// bytes → its content hash changes → the head's `chunks` table changes → the
/// head's own hash changes → the snapshot id moves. Listing 173 per-chunk
/// `{path, bytes, sha256}` entries in the manifest restated the content-addressing
/// and was the bulk of a 42 KB manifest — for the id AND for sync, since a client
/// fetches `head.sqlite` first and reads the chunk set from it.
///
/// `head.sqlite` itself is genuinely worth an entry: it is NOT content-addressed
/// by name, so its hash is the only external check on its bytes. (`closure.json`,
/// A2, is no longer emitted, so no longer hashed either.)
pub(crate) fn hash_artifacts(out: &Path) -> Result<Vec<ArtifactEntry>, EmitError> {
    let head = out.join("head.sqlite");
    let bytes = fs::read(&head)?;
    Ok(vec![ArtifactEntry {
        path: head
            .strip_prefix(out)
            .unwrap_or(&head)
            .to_string_lossy()
            .to_string(),
        bytes: bytes.len() as u64,
        sha256: hex(&Sha256::digest(&bytes)),
    }])
}

/// The canonical bytes of a manifest **for hashing**: compact JSON with
/// `snapshot_id` empty, and therefore omitted (the field is
/// `skip_serializing_if = "String::is_empty"`).
///
/// This is how the obvious self-reference resolves — the id must cover the
/// manifest, and the manifest must carry the id — hash the manifest *minus the
/// id*. Deterministic: struct field order is fixed and `fields` is a BTreeMap.
pub(crate) fn manifest_digest_bytes(manifest: &Manifest) -> Result<Vec<u8>, EmitError> {
    debug_assert!(
        manifest.snapshot_id.is_empty(),
        "hash the manifest BEFORE setting the id it is being hashed to produce"
    );
    Ok(serde_json::to_vec(manifest)?)
}

/// The snapshot id: a hash over the hashes of every artifact — **including the
/// manifest** (id-excluded, see [`manifest_digest_bytes`]).
///
/// The manifest used to be left out of the digest, purely because it is written
/// last. That made `snapshot_id` a hash of the *data* and not of the
/// *deployment*: the handler set (I6) and the tier definition could change while
/// the id stood still, so two deployments that answer queries differently could
/// claim the same identity — and drift detection built on the id would see
/// nothing. The manifest is a first-class artifact and is hashed like one.
pub(crate) fn snapshot_id(artifacts: &[ArtifactEntry], manifest_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    for entry in artifacts {
        hasher.update(entry.sha256.as_bytes());
    }
    hasher.update(hex(&Sha256::digest(manifest_bytes)).as_bytes());
    hex(&hasher.finalize())[..16].to_string()
}
