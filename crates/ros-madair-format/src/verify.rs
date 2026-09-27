// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native reader-side snapshot verification — the read gate that closes the
//! sign→verify loop.
//!
//! [`verify_snapshot`] recomputes a snapshot's `snapshot_id` from the files on
//! disk and checks it against the signed attestation. It lives in the format
//! crate (not the emitter) so a reader — `ros-madair-duck` natively, or the
//! emit CLI's `verify` — trusts a snapshot **without linking the writer**, and
//! so writer and reader share the ONE [`crate::snapshot_id`] derivation (a
//! signed id and a recomputed id cannot drift). It is native-only: a browser
//! reader verifies via [`crate::attest::verify_bundle`] over hashes it gathered
//! itself.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::attest::{attributions, verify_bundle, AttestationBundle, HeadTrust, Verdict};
use crate::{hex, manifest_digest_bytes, snapshot_id, ArtifactEntry, Manifest};

/// Verification could not even read the snapshot's manifest — a malformed
/// snapshot, distinct from an *untrusted* one (which is a [`HeadTrust`] verdict).
#[derive(Debug)]
pub enum VerifyError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Io(e) => write!(f, "cannot read snapshot manifest: {e}"),
            VerifyError::Json(e) => write!(f, "snapshot manifest/attestations did not parse: {e}"),
        }
    }
}

impl std::error::Error for VerifyError {}

impl From<std::io::Error> for VerifyError {
    fn from(e: std::io::Error) -> Self {
        VerifyError::Io(e)
    }
}

impl From<serde_json::Error> for VerifyError {
    fn from(e: serde_json::Error) -> Self {
        VerifyError::Json(e)
    }
}

/// Verify an emitted snapshot against its own attestations, RECOMPUTING the
/// `snapshot_id` from the artifacts on disk — the read-side gate.
///
/// Two independent checks, in order:
///  1. **Integrity/self-consistency.** Re-hash every file the manifest lists and
///     re-derive the `snapshot_id` from those hashes plus the manifest (the exact
///     [`snapshot_id`] derivation the emitter used). If a listed artifact is
///     missing or its bytes changed, the recomputed id moves off
///     `manifest.snapshot_id` → [`HeadTrust::Failed`] "altered".
///  2. **Authenticity.** With the manifest proven self-consistent, check the
///     attestation set over that id ([`verify_bundle`]). No `attestations.json`
///     → [`HeadTrust::Unsigned`], distinct from a tamper so a UI can word the two
///     differently.
///
/// Returns a three-state [`HeadTrust`] — Verified / Unsigned / Failed. A
/// [`VerifyError`] is reserved for "cannot read the manifest at all" (a malformed
/// snapshot, not an untrusted one).
pub fn verify_snapshot(dir: &Path) -> Result<HeadTrust, VerifyError> {
    let manifest_path = dir.join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;

    // 1. UNSIGNED comes FIRST. An absent attestations.json means nobody vouched
    //    for this snapshot, so there is nothing to check against — Unsigned
    //    (yellow), FULL STOP. This must precede the recompute: a layer packaged
    //    before signing carries a stub/empty-artifacts manifest, and recomputing
    //    its id would spuriously "not match" and cry tamper.
    let att_path = dir.join("attestations.json");
    let bundle: AttestationBundle = match fs::read(&att_path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(_) => return Ok(HeadTrust::Unsigned),
    };

    // 2. Signed. From here a mismatch is a real alarm. A signed snapshot must
    //    carry a snapshot_id (sign refuses to sign an empty one).
    if manifest.snapshot_id.is_empty() {
        return Ok(HeadTrust::Failed {
            reason: "signed snapshot carries no snapshot_id".to_string(),
        });
    }

    // Re-hash exactly the files the manifest lists (in its order — emit sorted
    // them, so the recomputed vec matches byte-for-byte when untampered).
    let mut recomputed = Vec::with_capacity(manifest.artifacts.len());
    for a in &manifest.artifacts {
        let bytes = match fs::read(dir.join(&a.path)) {
            Ok(b) => b,
            Err(_) => {
                return Ok(HeadTrust::Failed {
                    reason: format!("artifact {} is missing (snapshot is incomplete)", a.path),
                })
            }
        };
        recomputed.push(ArtifactEntry {
            path: a.path.clone(),
            bytes: bytes.len() as u64,
            sha256: hex(&Sha256::digest(&bytes)),
        });
    }
    // Re-derive the id: hash the id-less manifest carrying the recomputed
    // artifacts, exactly as emit did. Any changed byte in a listed file moves it.
    let mut idless = manifest.clone();
    idless.snapshot_id = String::new();
    idless.artifacts = recomputed.clone();
    let recomputed_id = snapshot_id(&recomputed, &manifest_digest_bytes(&idless)?);
    if recomputed_id != manifest.snapshot_id {
        return Ok(HeadTrust::Failed {
            reason:
                "content does not match the manifest — this snapshot has been altered since it \
                     was signed"
                    .to_string(),
        });
    }

    // 3. Content matches the signed manifest; check who vouches for it.
    match verify_bundle(&bundle, &manifest.snapshot_id) {
        Verdict::Trusted { authored } => Ok(HeadTrust::Verified {
            authored,
            attributions: attributions(&bundle, &manifest.snapshot_id),
        }),
        Verdict::Untrusted { reason } => Ok(HeadTrust::Failed { reason }),
    }
}
