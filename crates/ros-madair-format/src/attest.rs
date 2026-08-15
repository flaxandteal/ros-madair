// SPDX-License-Identifier: AGPL-3.0-or-later
//! Snapshot **attestations** — the authenticity layer over a head's
//! [`snapshot_id`](crate::Manifest::snapshot_id).
//!
//! `snapshot_id` already gives INTEGRITY (two snapshots differ ⇒ ids differ).
//! What it cannot answer is AUTHENTICITY: *who* emitted this head, and has the
//! artifact been swapped for a same-shaped forgery at rest or in transit. That
//! is what this module adds, and it is deliberately a FORMAT concern (like the
//! manifest and the chunk framing), not a domain model: a reader — native,
//! Tauri, or WASM in a browser — must be able to VERIFY without linking the
//! emitter.
//!
//! # Shape (decided; see `Greasan-Testbed-Handoff.md`)
//!
//! A detached **DSSE** envelope (`payloadType` + base64 `payload` +
//! `signatures`) whose payload is an **in-toto Statement** binding a
//! `predicateType` to a `subject` — here the subject digest is the head's
//! `snapshot_id`. An [`AttestationBundle`] is a **set** of these: L0 emits ONE,
//! `predicateType = authored`, and the set shape is on purpose so extra
//! predicates/signers (steward counter-signatures, revocation) bolt on in a
//! later phase with zero re-architecture.
//!
//! # Trust model — L0 (TOFU, self-contained)
//!
//! The signer's Ed25519 **public key travels inside the envelope**
//! ([`Signature::public_key`]), so verification is self-contained and needs no
//! key distribution: it proves the payload was signed by the holder of that key
//! AND that the statement's subject still matches the snapshot in hand. Tamper a
//! chunk and the reader-computed `snapshot_id` moves off the signed subject →
//! [`verify_bundle`] returns [`Verdict::Untrusted`]. Pinning a specific key,
//! counter-signatures, and revocation are the Phase-1 policy layer; the *set*
//! structure and [`verify_bundle`]'s policy shape already accommodate them.
//!
//! # WASM
//!
//! Verify-only here — no key generation, no filesystem — so it holds to
//! `wasm32-unknown-unknown` like the rest of this crate. Signing (private-key
//! generation and storage) is native and lives in `ros-madair-emit`.

use std::collections::BTreeMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ed25519_dalek::{Signature as DalekSig, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The DSSE `payloadType` for an in-toto Statement payload.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
/// The in-toto Statement `_type`.
pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
/// The L0 `predicateType`: "this head was authored/emitted by the key holder".
pub const PREDICATE_AUTHORED: &str = "https://flaxandteal.org/rosmadair/predicates/authored/v1";
/// The subject-digest algorithm key under which the `snapshot_id` is recorded.
/// (in-toto digests are `{alg: hex}`; ours is a first-class snapshot id, not a
/// generic hash, so it gets its own name.)
pub const DIGEST_KEY: &str = "snapshot_id";

/// A set of [`Attestation`]s over one head. The wire artifact (`attestations.json`
/// alongside the head) that a reader loads and hands to [`verify_bundle`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttestationBundle {
    pub attestations: Vec<Attestation>,
}

/// One DSSE envelope: a `payload` (base64 of an in-toto [`Statement`]) with its
/// `payloadType`, plus the detached [`Signature`]s over the DSSE
/// pre-authentication encoding ([`pae`]) of that payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attestation {
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// Base64 (standard) of the in-toto Statement JSON. Kept base64 (not inlined
    /// JSON) because the signature is over the exact bytes — re-serializing an
    /// inlined object could change them and break the signature.
    pub payload: String,
    pub signatures: Vec<Signature>,
}

/// A detached signature, carrying the verifying key so L0 verification is
/// self-contained (see the module trust-model note).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    /// A stable id for the key (first 16 hex of sha256(pubkey)). Advisory; the
    /// bytes in [`public_key`](Self::public_key) are what verification uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyid: Option<String>,
    /// Base64 of the 32-byte Ed25519 public key.
    #[serde(rename = "publicKey")]
    pub public_key: String,
    /// Base64 of the 64-byte Ed25519 signature over `pae(payloadType, payload)`.
    pub sig: String,
}

/// The in-toto Statement carried as the DSSE payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub type_: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    #[serde(default)]
    pub predicate: serde_json::Value,
}

/// One subject a statement is about: a name and its digest set. For us the
/// digest carries [`DIGEST_KEY`] → the head's `snapshot_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subject {
    pub name: String,
    pub digest: BTreeMap<String, String>,
}

impl Subject {
    /// Does this subject's digest bind the given `snapshot_id`?
    pub fn covers(&self, snapshot_id: &str) -> bool {
        self.digest.get(DIGEST_KEY).is_some_and(|d| d == snapshot_id)
    }
}

/// The verdict of [`verify_bundle`]. `Trusted` means the L0 policy held: at least
/// one `authored` attestation over THIS snapshot carried a valid signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Trusted { authored: usize },
    Untrusted { reason: String },
}

impl Verdict {
    pub fn is_trusted(&self) -> bool {
        matches!(self, Verdict::Trusted { .. })
    }
}

/// DSSE v1 pre-authentication encoding (PAE): what is actually signed, so a
/// signature can never be confused across `payloadType`s or truncated payloads.
///
/// `"DSSEv1" SP len(type) SP type SP len(payload) SP payload`, all lengths as
/// ASCII decimal byte counts. The signature covers THIS, not the raw payload.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + payload_type.len() + 32);
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

/// Verify one signature (embedded pubkey) over `signed` (already the PAE bytes).
fn verify_sig(signed: &[u8], s: &Signature) -> Result<(), String> {
    let pk = STANDARD
        .decode(&s.public_key)
        .map_err(|e| format!("public key not base64: {e}"))?;
    let pk: [u8; 32] = pk
        .as_slice()
        .try_into()
        .map_err(|_| "public key is not 32 bytes".to_string())?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|e| format!("invalid public key: {e}"))?;
    let sig = STANDARD
        .decode(&s.sig)
        .map_err(|e| format!("signature not base64: {e}"))?;
    let sig: [u8; 64] = sig
        .as_slice()
        .try_into()
        .map_err(|_| "signature is not 64 bytes".to_string())?;
    vk.verify_strict(signed, &DalekSig::from_bytes(&sig))
        .map_err(|e| format!("signature does not verify: {e}"))
}

/// Is this attestation a VALID `authored` statement over `snapshot_id`? Ok on
/// success; Err carries why it did not count (used to surface a reason when the
/// bundle as a whole is untrusted).
fn check_authored(att: &Attestation, snapshot_id: &str) -> Result<(), String> {
    if att.payload_type != PAYLOAD_TYPE {
        return Err(format!("unexpected payloadType {:?}", att.payload_type));
    }
    let payload = STANDARD
        .decode(&att.payload)
        .map_err(|e| format!("payload not base64: {e}"))?;
    let stmt: Statement = serde_json::from_slice(&payload)
        .map_err(|e| format!("payload is not an in-toto statement: {e}"))?;
    if stmt.type_ != STATEMENT_TYPE {
        return Err(format!("unexpected statement _type {:?}", stmt.type_));
    }
    if stmt.predicate_type != PREDICATE_AUTHORED {
        return Err(format!("predicate {:?} is not authored", stmt.predicate_type));
    }
    if !stmt.subject.iter().any(|s| s.covers(snapshot_id)) {
        return Err(format!(
            "no subject binds snapshot {snapshot_id} (attestation is for a different head)"
        ));
    }
    // Sign/verify over the PAE of the EXACT payload bytes we decoded.
    let signed = pae(&att.payload_type, &payload);
    let mut last = "attestation carries no signatures".to_string();
    for s in &att.signatures {
        match verify_sig(&signed, s) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(format!("no valid signature: {last}"))
}

/// The L0 verification policy: **≥1 valid `authored` attestation** over exactly
/// this `snapshot_id`. Returns [`Verdict::Trusted`] with the count, or
/// [`Verdict::Untrusted`] with the most specific reason seen.
///
/// This is intentionally a policy over the whole SET, not a per-signature check:
/// Phase 1 tightens it (require a pinned/steward key, N-of-M counter-signatures)
/// by changing THIS function, leaving the wire shape and every emitter untouched.
pub fn verify_bundle(bundle: &AttestationBundle, snapshot_id: &str) -> Verdict {
    let mut authored = 0usize;
    let mut reason = if bundle.attestations.is_empty() {
        "no attestations present".to_string()
    } else {
        "no attestation vouches for this snapshot".to_string()
    };
    for att in &bundle.attestations {
        match check_authored(att, snapshot_id) {
            Ok(()) => authored += 1,
            Err(e) => reason = e,
        }
    }
    if authored >= 1 {
        Verdict::Trusted { authored }
    } else {
        Verdict::Untrusted { reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PAE is the DSSE v1 spec vector shape: lengths are BYTE counts in ASCII.
    #[test]
    fn pae_matches_dsse_v1() {
        let got = pae("http://example.com/HelloWorld", b"hello world");
        assert_eq!(got, b"DSSEv1 29 http://example.com/HelloWorld 11 hello world");
    }

    /// A malformed bundle (garbage base64) is Untrusted, never a panic.
    #[test]
    fn garbage_is_untrusted_not_a_panic() {
        let bundle = AttestationBundle {
            attestations: vec![Attestation {
                payload_type: PAYLOAD_TYPE.to_string(),
                payload: "!!!not base64!!!".to_string(),
                signatures: vec![],
            }],
        };
        assert!(!verify_bundle(&bundle, "abc").is_trusted());
    }

    #[test]
    fn empty_bundle_is_untrusted() {
        let v = verify_bundle(&AttestationBundle::default(), "abc");
        assert_eq!(
            v,
            Verdict::Untrusted {
                reason: "no attestations present".to_string()
            }
        );
    }
}
