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
/// The L0 `predicateType`: "this head was authored/emitted by the key holder"
/// (anonymous — no named actor).
pub const PREDICATE_AUTHORED: &str = "https://flaxandteal.org/rosmadair/predicates/authored/v1";
/// "This layer was DERIVED from public records by the named actor" — the role a
/// packager (e.g. Flax & Teal building from open data) claims. Vouches for the
/// derivation, not for the upstream authority.
pub const PREDICATE_DERIVED: &str = "https://flaxandteal.org/rosmadair/predicates/derived/v1";
/// "This layer is ENDORSED by the named actor as the authoritative source" — the
/// stronger role the ORIGINAL upstream publisher claims when they attest their
/// own data. This is what distinguishes an F&T-derived-from-public-records layer
/// from one the source publisher vouches for themselves.
pub const PREDICATE_ENDORSED: &str = "https://flaxandteal.org/rosmadair/predicates/endorsed/v1";
/// The subject-digest algorithm key under which the `snapshot_id` is recorded.
/// (in-toto digests are `{alg: hex}`; ours is a first-class snapshot id, not a
/// generic hash, so it gets its own name.)
pub const DIGEST_KEY: &str = "snapshot_id";

/// What an attestation CLAIMS about its subject — the role of the signer.
/// Orthogonal to whether the signer's identity is CONFIRMED (that is a later
/// policy over the actor's declared key); this is what the signer asserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// Anonymous authorship — no named actor (the L0 default).
    Authored,
    /// Derived/produced from public records by a named actor.
    Derived,
    /// Endorsed by a named actor as the authoritative upstream source.
    Endorsed,
}

impl Role {
    pub fn predicate_type(self) -> &'static str {
        match self {
            Role::Authored => PREDICATE_AUTHORED,
            Role::Derived => PREDICATE_DERIVED,
            Role::Endorsed => PREDICATE_ENDORSED,
        }
    }
    pub fn from_predicate_type(pt: &str) -> Option<Role> {
        match pt {
            PREDICATE_AUTHORED => Some(Role::Authored),
            PREDICATE_DERIVED => Some(Role::Derived),
            PREDICATE_ENDORSED => Some(Role::Endorsed),
            _ => None,
        }
    }
}

/// The named actor carried in an attestation's predicate. `id` is the actor
/// resource URI (a `schema:Person`/`schema:Organization`); `public_key_multibase`
/// is the signer's key in the W3C `sec:publicKeyMultibase` form (`z6Mk…`), so it
/// aligns with the actor resource's declared `sec:assertionMethod` for the later
/// authority-confirmation step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredicateActor {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "publicKeyMultibase")]
    pub public_key_multibase: String,
}

/// The typed body of the in-toto `predicate` (an object; unknown keys ignored).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Predicate {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snapshot_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<PredicateActor>,
}

/// A verified attribution: a valid attestation NAMED an actor with a role, and
/// the key that signed it matches the key the actor claimed. (Whether that actor
/// URI is itself trusted — key pinning / confirmation against the actor
/// resource's declared key — is a separate policy layer.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribution {
    pub actor_id: String,
    pub actor_name: String,
    pub role: Role,
    /// The signer's key in `sec:publicKeyMultibase` form. A reader CONFIRMS the
    /// attribution by matching this against a trusted actor→key registry (rooted
    /// in a pinned key); until then it is self-asserted.
    pub public_key_multibase: String,
}

/// The multicodec varint prefix for an ed25519 public key (`0xed 0x01`).
const ED25519_MULTICODEC: [u8; 2] = [0xed, 0x01];

/// Encode an ed25519 public key as `sec:publicKeyMultibase` (base58btc `z` +
/// multicodec-prefixed key) — the `z6Mk…` form DID-key/VC use.
pub fn ed25519_to_multibase(pk: &[u8; 32]) -> String {
    let mut buf = Vec::with_capacity(34);
    buf.extend_from_slice(&ED25519_MULTICODEC);
    buf.extend_from_slice(pk);
    format!("z{}", bs58::encode(buf).into_string())
}

/// Decode a `sec:publicKeyMultibase` string back to the 32-byte ed25519 key.
pub fn multibase_to_ed25519(s: &str) -> Result<[u8; 32], String> {
    let rest = s
        .strip_prefix('z')
        .ok_or("not base58btc multibase (missing 'z' prefix)")?;
    let bytes = bs58::decode(rest)
        .into_vec()
        .map_err(|e| format!("bad base58: {e}"))?;
    if bytes.len() != 34 || bytes[..2] != ED25519_MULTICODEC {
        return Err("not a multicodec ed25519 public key".to_string());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[2..]);
    Ok(out)
}

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

/// The read-side trust state of a whole head, as three distinct outcomes the UI
/// renders as a green / yellow / red shield. Distinct from [`Verdict`] (the
/// signature policy) because a reader must tell an UNSIGNED layer (normal for
/// old/third-party data) from an ALTERED one (a real alarm): both are "not
/// trusted", but only one is a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadTrust {
    /// A valid attestation over the current content — green shield. `authored` is
    /// the count of valid attestations; `attributions` are the NAMED ones (actor
    /// + role), empty for a purely anonymous/L0 signature.
    Verified {
        authored: usize,
        attributions: Vec<Attribution>,
    },
    /// No `attestations.json` — an unsigned layer (old or third-party). Yellow
    /// shield: enable with a soft heads-up, not an alarm.
    Unsigned,
    /// Verification did NOT hold — content altered since signing, a missing
    /// artifact, or an invalid/foreign signature. Red shield; `reason` says which.
    Failed { reason: String },
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

/// Verify one signature (embedded pubkey) over `signed` (already the PAE bytes),
/// returning the 32-byte signer key on success (so a named attestation can bind
/// the signer to the actor it claims).
fn verify_sig(signed: &[u8], s: &Signature) -> Result<[u8; 32], String> {
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
        .map_err(|e| format!("signature does not verify: {e}"))?;
    Ok(pk)
}

/// Is this attestation a VALID vouch over `snapshot_id`? Ok(Some) = valid AND
/// names an actor (attribution); Ok(None) = valid but anonymous; Err carries why
/// it did not count. A named attestation must be signed by the SAME key the actor
/// claims (`publicKeyMultibase`), else it is a forged claim of identity.
fn check_attestation(att: &Attestation, snapshot_id: &str) -> Result<Option<Attribution>, String> {
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
    let role = Role::from_predicate_type(&stmt.predicate_type)
        .ok_or_else(|| format!("unrecognised predicate {:?}", stmt.predicate_type))?;
    if !stmt.subject.iter().any(|s| s.covers(snapshot_id)) {
        return Err(format!(
            "no subject binds snapshot {snapshot_id} (attestation is for a different head)"
        ));
    }
    // Sign/verify over the PAE of the EXACT payload bytes we decoded; keep the key.
    let signed = pae(&att.payload_type, &payload);
    let mut last = "attestation carries no signatures".to_string();
    let mut signer: Option<[u8; 32]> = None;
    for s in &att.signatures {
        match verify_sig(&signed, s) {
            Ok(k) => {
                signer = Some(k);
                break;
            }
            Err(e) => last = e,
        }
    }
    let signer = signer.ok_or_else(|| format!("no valid signature: {last}"))?;

    // A named actor must have signed with the key it claims.
    let predicate: Predicate = serde_json::from_value(stmt.predicate).unwrap_or_default();
    match predicate.actor {
        Some(actor) => {
            let claimed = multibase_to_ed25519(&actor.public_key_multibase)?;
            if claimed != signer {
                return Err(format!(
                    "actor {} claims a key it did not sign with (forged attribution)",
                    actor.id
                ));
            }
            Ok(Some(Attribution {
                actor_id: actor.id,
                actor_name: actor.name,
                role,
                public_key_multibase: actor.public_key_multibase,
            }))
        }
        None => Ok(None),
    }
}

/// The NAMED, self-consistent attributions in a bundle over `snapshot_id` (actor
/// + role for each valid attestation that named an actor). Anonymous valid
/// attestations contribute nothing here. Deduped is the caller's concern.
pub fn attributions(bundle: &AttestationBundle, snapshot_id: &str) -> Vec<Attribution> {
    bundle
        .attestations
        .iter()
        .filter_map(|att| check_attestation(att, snapshot_id).ok().flatten())
        .collect()
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
        match check_attestation(att, snapshot_id) {
            Ok(_) => authored += 1,
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

    /// The sec:publicKeyMultibase form is the DID-key `z6Mk…` ed25519 encoding
    /// and round-trips exactly; a malformed key is rejected, not silently zeroed.
    #[test]
    fn multibase_roundtrips_ed25519() {
        let key = [7u8; 32];
        let mb = ed25519_to_multibase(&key);
        assert!(mb.starts_with("z6Mk"), "ed25519 did:key form, got {mb}");
        assert_eq!(multibase_to_ed25519(&mb).unwrap(), key);
        assert!(multibase_to_ed25519("Qm-no-z-prefix").is_err());
        assert!(multibase_to_ed25519("z1111").is_err()); // wrong multicodec/length
    }
}
