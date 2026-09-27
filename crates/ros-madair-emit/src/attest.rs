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
    ed25519_to_multibase, AttestationBundle, HeadTrust, Role, Signature, Statement, Subject,
    DIGEST_KEY, PAYLOAD_TYPE, PREDICATE_AUTHORED, STATEMENT_TYPE,
};
use ros_madair_format::{hex, Budgets, Manifest};
use sha2::{Digest, Sha256};

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

    /// The public key in `sec:publicKeyMultibase` form (`z6Mk…`) — the value to
    /// pin as a root or list in an actor→key registry.
    pub fn public_key_multibase(&self) -> String {
        ed25519_to_multibase(&self.key.verifying_key().to_bytes())
    }

    /// Stable advisory key id: first 16 hex of sha256(public key bytes).
    pub fn keyid(&self) -> String {
        hex(&Sha256::digest(self.key.verifying_key().to_bytes()))[..16].to_string()
    }

    /// Produce the bundle for a head: ONE attestation binding this `snapshot_id`,
    /// signed by this identity. `subject_name` is a human label for the subject
    /// (the head's `base_uri` is a good choice); it never affects verification,
    /// which keys on the `snapshot_id` digest.
    ///
    /// `attribution = None` → an anonymous L0 `authored` attestation. `Some((role,
    /// actor_id, actor_name))` → a NAMED attestation (`derived`/`endorsed`)
    /// carrying the actor URI and THIS key as `publicKeyMultibase`, so a reader
    /// can attribute the layer to the actor and (later) confirm the key.
    pub fn attest_snapshot(
        &self,
        snapshot_id: &str,
        subject_name: &str,
        attribution: Option<(Role, &str, &str)>,
    ) -> Result<AttestationBundle, EmitError> {
        let (predicate_type, predicate) = match attribution {
            None => (
                PREDICATE_AUTHORED.to_string(),
                serde_json::json!({ "snapshot_id": snapshot_id }),
            ),
            Some((role, actor_id, actor_name)) => (
                role.predicate_type().to_string(),
                serde_json::json!({
                    "snapshot_id": snapshot_id,
                    "actor": {
                        "id": actor_id,
                        "name": actor_name,
                        "publicKeyMultibase":
                            ed25519_to_multibase(&self.key.verifying_key().to_bytes()),
                    }
                }),
            ),
        };
        let statement = Statement {
            type_: STATEMENT_TYPE.to_string(),
            subject: vec![Subject {
                name: subject_name.to_string(),
                digest: [(DIGEST_KEY.to_string(), snapshot_id.to_string())]
                    .into_iter()
                    .collect(),
            }],
            predicate_type,
            predicate,
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
    let bundle = identity.attest_snapshot(&manifest.snapshot_id, &manifest.base_uri, None)?;
    fs::write(
        head_dir.join("attestations.json"),
        serde_json::to_vec_pretty(&bundle)?,
    )?;
    Ok(manifest.snapshot_id)
}

/// Make a parquet head self-describing and sign it, in one step — the build-time
/// packaging entry point (the `sign` CLI subcommand).
///
/// Unlike [`sign_head`] (which trusts the manifest emit already wrote),
/// `seal_and_sign` REBUILDS the manifest's content-derived parts by hashing every
/// file in the head — so a dataset packaged by a tool that wrote a stub manifest
/// (the old JS `package-parquet-layer.mjs`, `artifacts: []`) becomes a real,
/// verifiable head, and `graph.json` (copied in after emit) is covered too. It is
/// idempotent on a head emit already sealed. Existing `base_uri`/`handlers`/
/// `models`/`budgets`/`tier` are preserved from the current manifest when present.
///
/// `attribution = None` signs an anonymous L0 head; `Some((role, actor_id,
/// actor_name))` signs a NAMED one (a build-time packager passes e.g. `Derived` +
/// the Flax & Teal actor URI). Returns the (re)computed `snapshot_id`.
pub fn seal_and_sign(
    head_dir: &Path,
    key_path: &Path,
    attribution: Option<(Role, &str, &str)>,
) -> Result<String, EmitError> {
    let manifest_path = head_dir.join("manifest.json");
    let mut manifest: Manifest = match fs::read(&manifest_path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(_) => Manifest {
            snapshot_id: String::new(),
            format_version: ros_madair_format::FORMAT_VERSION,
            base_uri: "https://example.org/".to_string(),
            tier: None,
            handlers: vec![],
            models: vec![],
            artifacts: vec![],
            budgets: Budgets {
                max_result_rows: 1000,
                max_group_count: 500,
            },
        },
    };
    // Rebuild the self-describing parts so verify_head recomputes to this id.
    manifest.snapshot_id = String::new();
    manifest.format_version = ros_madair_format::FORMAT_VERSION;
    manifest.artifacts = crate::manifest::hash_parquet_artifacts(head_dir)?;
    let id = ros_madair_format::snapshot_id(
        &manifest.artifacts,
        &ros_madair_format::manifest_digest_bytes(&manifest)?,
    );
    manifest.snapshot_id = id.clone();
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;

    let identity = SigningIdentity::load_or_create(key_path)?;
    let bundle = identity.attest_snapshot(&id, &manifest.base_uri, attribution)?;
    fs::write(
        head_dir.join("attestations.json"),
        serde_json::to_vec_pretty(&bundle)?,
    )?;
    Ok(id)
}

/// Verify an emitted snapshot against its own attestations, RECOMPUTING the
/// `snapshot_id` from the artifacts on disk — the read-side gate.
///
/// A thin wrapper over [`ros_madair_format::verify::verify_snapshot`] (the ONE
/// verify implementation, shared with `ros-madair-duck`'s read path). Kept here
/// so the `verify` CLI subcommand and existing callers of `verify_head` keep
/// working. Returns the three-state [`HeadTrust`] — Verified / Unsigned / Failed;
/// an error is reserved for "cannot read the manifest at all".
pub fn verify_head(head_dir: &Path) -> Result<HeadTrust, EmitError> {
    ros_madair_format::verify::verify_snapshot(head_dir).map_err(|e| -> EmitError { Box::new(e) })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Verify helpers exercised only by these tests (verify_head now delegates to
    // the format crate, so they are not used by the non-test build).
    use ros_madair_format::attest::{attributions, verify_bundle, Verdict};

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
        let bundle = id
            .attest_snapshot("deadbeefdeadbeef", "https://example.org/", None)
            .unwrap();
        assert_eq!(
            verify_bundle(&bundle, "deadbeefdeadbeef"),
            Verdict::Trusted { authored: 1 }
        );
        // Anonymous: verifies, but names no actor.
        assert!(attributions(&bundle, "deadbeefdeadbeef").is_empty());
    }

    #[test]
    fn a_named_derived_attestation_attributes_the_layer() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let actor = "https://flaxandteal.co.uk/actor/flax-and-teal";
        let bundle = id
            .attest_snapshot(
                "deadbeefdeadbeef",
                "x",
                Some((Role::Derived, actor, "Flax & Teal")),
            )
            .unwrap();
        assert!(verify_bundle(&bundle, "deadbeefdeadbeef").is_trusted());
        let attrs = attributions(&bundle, "deadbeefdeadbeef");
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].actor_id, actor);
        assert_eq!(attrs[0].actor_name, "Flax & Teal");
        assert_eq!(attrs[0].role, Role::Derived);
    }

    /// The actor's claimed key is INSIDE the signed payload, so a forged
    /// attribution (claim a key you don't hold) requires tampering the payload -
    /// which breaks the signature. Result: not trusted, not attributed.
    #[test]
    fn a_forged_actor_key_is_rejected() {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let mut bundle = id
            .attest_snapshot(
                "deadbeefdeadbeef",
                "x",
                Some((Role::Endorsed, "urn:actor:x", "X")),
            )
            .unwrap();
        let att = &mut bundle.attestations[0];
        let mut stmt: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&att.payload).unwrap()).unwrap();
        stmt["predicate"]["actor"]["publicKeyMultibase"] =
            serde_json::json!(ed25519_to_multibase(&[0u8; 32])); // a key we don't hold
        att.payload = STANDARD.encode(serde_json::to_vec(&stmt).unwrap());
        assert!(!verify_bundle(&bundle, "deadbeefdeadbeef").is_trusted());
        assert!(attributions(&bundle, "deadbeefdeadbeef").is_empty());
    }

    /// A tampered chunk moves the reader-computed snapshot_id off the signed
    /// subject → Untrusted. (Here: verify against a DIFFERENT id.)
    #[test]
    fn a_different_snapshot_is_untrusted() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let bundle = id
            .attest_snapshot("deadbeefdeadbeef", "https://example.org/", None)
            .unwrap();
        assert!(!verify_bundle(&bundle, "0000000000000000").is_trusted());
    }

    /// Flip a signature byte → the Ed25519 check fails → Untrusted.
    #[test]
    fn a_tampered_signature_is_untrusted() {
        let id = SigningIdentity::load_or_create(&tmp_key()).unwrap();
        let mut bundle = id.attest_snapshot("deadbeefdeadbeef", "x", None).unwrap();
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
        let a = id.attest_snapshot("deadbeefdeadbeef", "x", None).unwrap();
        let b = id.attest_snapshot("0000000000000000", "x", None).unwrap();
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
