// SPDX-License-Identifier: AGPL-3.0-or-later
//! Snapshot **signing** — the native, key-holding half of the attestation layer.
//!
//! The wire types and the verify policy live in `ros-madair-format`
//! ([`ros_madair_format::attest`]) so a browser can verify without linking this
//! crate. Here we hold a private Ed25519 key and turn a head's `snapshot_id`
//! into a signed [`AttestationBundle`].
//!
//! # Default-on, L0 (SSH-host-key style)
//!
//! A signing identity auto-generates on first use and persists — no user action,
//! exactly like an SSH host key. [`SigningIdentity::load_or_create`] reads the
//! key if present, else mints one and writes it `0600`. Every emit signs; the
//! public key rides inside the envelope so verification is self-contained. Key
//! pinning, steward counter-signatures, and revocation are the Phase-1 policy
//! layer over the *set* — not this module.

use std::fs;
use std::path::Path;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use ros_madair_format::attest::{
    AttestationBundle, Signature, Statement, Subject, DIGEST_KEY, PAYLOAD_TYPE, PREDICATE_AUTHORED,
    STATEMENT_TYPE,
};
use sha2::{Digest, Sha256};

use crate::chunks::hex;
use crate::EmitError;

/// A persisted Ed25519 signing identity for this owner/device.
pub struct SigningIdentity {
    key: SigningKey,
}

impl SigningIdentity {
    /// Load the identity at `path`, or mint and persist a fresh one (SSH-host-key
    /// style — zero user action). The file holds the base64 of the 32-byte Ed25519
    /// seed and is written `0600` on unix.
    ///
    /// A present-but-unreadable/short key file is an ERROR, not a silent re-mint:
    /// re-minting would rotate identity and orphan every head this owner already
    /// signed. Corruption should be loud.
    pub fn load_or_create(path: &Path) -> Result<Self, EmitError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let seed = STANDARD.decode(text.trim())?;
                let seed: [u8; 32] = seed.as_slice().try_into().map_err(|_| {
                    format!(
                        "signing key at {} is not a 32-byte ed25519 seed (refusing to re-mint and \
                         rotate identity — move it aside if it is genuinely corrupt)",
                        path.display()
                    )
                })?;
                Ok(Self {
                    key: SigningKey::from_bytes(&seed),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::create(path),
            Err(e) => Err(Box::new(e)),
        }
    }

    fn create(path: &Path) -> Result<Self, EmitError> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed)
            .map_err(|e| format!("could not generate a signing key seed: {e}"))?;
        let key = SigningKey::from_bytes(&seed);
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        fs::write(path, STANDARD.encode(seed))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { key })
    }

    /// Base64 of the 32-byte public key (rides in every signature envelope).
    pub fn public_key_b64(&self) -> String {
        STANDARD.encode(self.key.verifying_key().to_bytes())
    }

    /// Stable advisory key id: first 16 hex of sha256(public key bytes).
    pub fn keyid(&self) -> String {
        hex(&Sha256::digest(self.key.verifying_key().to_bytes()))[..16].to_string()
    }

    /// Produce the L0 bundle for a head: ONE `authored` attestation binding this
    /// `snapshot_id`, signed by this identity. `subject_name` is a human label for
    /// the subject (the head's `base_uri` is a good choice); it never affects
    /// verification, which keys on the `snapshot_id` digest.
    pub fn attest_snapshot(
        &self,
        snapshot_id: &str,
        subject_name: &str,
    ) -> Result<AttestationBundle, EmitError> {
        let statement = Statement {
            type_: STATEMENT_TYPE.to_string(),
            subject: vec![Subject {
                name: subject_name.to_string(),
                digest: [(DIGEST_KEY.to_string(), snapshot_id.to_string())]
                    .into_iter()
                    .collect(),
            }],
            predicate_type: PREDICATE_AUTHORED.to_string(),
            predicate: serde_json::json!({ "snapshot_id": snapshot_id }),
        };
        // Sign the PAE of the EXACT payload bytes we base64 into the envelope.
        let payload = serde_json::to_vec(&statement)?;
        let signed = ros_madair_format::attest::pae(PAYLOAD_TYPE, &payload);
        let sig = self.key.sign(&signed);
        Ok(AttestationBundle {
            attestations: vec![ros_madair_format::attest::Attestation {
                payload_type: PAYLOAD_TYPE.to_string(),
                payload: STANDARD.encode(&payload),
                signatures: vec![Signature {
                    keyid: Some(self.keyid()),
                    public_key: self.public_key_b64(),
                    sig: STANDARD.encode(sig.to_bytes()),
                }],
            }],
        })
    }
}

/// Sign an already-emitted head in place: read its `manifest.json`, sign the
/// `snapshot_id` with the identity at `key_path` (minting one on first use), and
/// write `attestations.json` beside the head. Returns the signed `snapshot_id`.
///
/// This is the default-on L0 step a caller runs right after `emit_parquet` /
/// `emit`: the emitter already wrote a self-describing manifest, so signing is a
/// pure add-on over its `snapshot_id` — no re-hashing, and it works for any head
/// (sqlite or parquet) that carries a manifest.
pub fn sign_head(head_dir: &Path, key_path: &Path) -> Result<String, EmitError> {
    let manifest_path = head_dir.join("manifest.json");
    let bytes = fs::read(&manifest_path).map_err(|e| {
        format!(
            "sign_head: cannot read {} ({e}) — sign runs AFTER emit writes the manifest",
            manifest_path.display()
        )
    })?;
    let manifest: ros_madair_format::Manifest = serde_json::from_slice(&bytes)?;
    if manifest.snapshot_id.is_empty() {
        return Err("sign_head: manifest carries no snapshot_id to sign".into());
    }
    let identity = SigningIdentity::load_or_create(key_path)?;
    let bundle = identity.attest_snapshot(&manifest.snapshot_id, &manifest.base_uri)?;
    fs::write(
        head_dir.join("attestations.json"),
        serde_json::to_vec_pretty(&bundle)?,
    )?;
    Ok(manifest.snapshot_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ros_madair_format::attest::{verify_bundle, Verdict};

    fn tmp_key() -> std::path::PathBuf {
        // A per-test unique path under the target dir; no external tempdir dep.
        let mut p = std::env::temp_dir();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("rm-signkey-{n}-{:p}.key", &p));
        p
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let bundle = id.attest_snapshot("deadbeefdeadbeef", "https://example.org/").unwrap();
        assert_eq!(
            verify_bundle(&bundle, "deadbeefdeadbeef"),
            Verdict::Trusted { authored: 1 }
        );
    }

    /// A tampered chunk moves the reader-computed snapshot_id off the signed
    /// subject → Untrusted. (Here: verify against a DIFFERENT id.)
    #[test]
    fn a_different_snapshot_is_untrusted() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let bundle = id.attest_snapshot("deadbeefdeadbeef", "https://example.org/").unwrap();
        assert!(!verify_bundle(&bundle, "0000000000000000").is_trusted());
    }

    /// Flip a signature byte → the Ed25519 check fails → Untrusted.
    #[test]
    fn a_tampered_signature_is_untrusted() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let mut bundle = id.attest_snapshot("deadbeefdeadbeef", "x").unwrap();
        // Corrupt one base64 char of the signature deterministically.
        let sig = &mut bundle.attestations[0].signatures[0].sig;
        let first = sig.chars().next().unwrap();
        let replacement = if first == 'A' { 'B' } else { 'A' };
        *sig = format!("{replacement}{}", &sig[1..]);
        assert!(!verify_bundle(&bundle, "deadbeefdeadbeef").is_trusted());
    }

    /// Swap the payload for one over a different snapshot but keep the old
    /// signature: PAE changes, signature no longer covers it → Untrusted. (Guards
    /// against a payload-substitution forgery.)
    #[test]
    fn a_substituted_payload_is_untrusted() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let a = id.attest_snapshot("deadbeefdeadbeef", "x").unwrap();
        let b = id.attest_snapshot("0000000000000000", "x").unwrap();
        let mut forged = a.clone();
        // Graft b's payload under a's signature.
        forged.attestations[0].payload = b.attestations[0].payload.clone();
        assert!(!verify_bundle(&forged, "0000000000000000").is_trusted());
        assert!(!verify_bundle(&forged, "deadbeefdeadbeef").is_trusted());
    }

    /// The identity persists: reloading the same file yields the same public key
    /// (re-minting would orphan already-signed heads).
    #[test]
    fn identity_persists_across_reload() {
        let path = tmp_key();
        let a = SigningIdentity::load_or_create(&path).unwrap();
        let b = SigningIdentity::load_or_create(&path).unwrap();
        assert_eq!(a.public_key_b64(), b.public_key_b64());
        let _ = fs::remove_file(&path);
    }
}
