// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Rós Madair static-artifact **format** — the types that describe what a
//! snapshot *is* (the manifest contract) plus the attestation **verify** types,
//! shared by the one writer (`ros-madair-emit`) and every reader (a native
//! verifier in `ros-madair-duck`, a browser WASM consumer).
//!
//! # Why this crate exists
//!
//! [`Manifest`] used to live inside the emitter, serialize-only, so a reader
//! could not use it — it had to hand-mirror the wire shape. A hand-mirrored copy
//! of a wire format is a silent-drift machine: the emitter changes a field, the
//! mirror does not, and the reader deserializes nonsense. So the contract is its
//! own crate now, with `Serialize + Deserialize` on everything.
//!
//! Alongside it (behind the `attest` feature) sit the attestation **verify**
//! types — the half a *reader* uses to check that a snapshot's `snapshot_id` was
//! signed by a trusted actor. Signing (private keys) is native, in
//! `ros-madair-emit`.
//!
//! # WASM
//!
//! Browser deployments **query and verify, never emit**. This crate is held to
//! `wasm32-unknown-unknown` (handler declarations + serde + verify-only
//! ed25519; no rusqlite, no `std::fs`) so a browser can parse a manifest and
//! verify its attestation without linking the emitter's bundled writer. Anything
//! needing the filesystem, DuckDB, or SQLite belongs in `ros-madair-duck` or
//! `ros-madair-emit` (both native), not here.

use ros_madair_handlers::HandlerDecl;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Snapshot attestations (authenticity over `snapshot_id`) — DSSE/in-toto types
/// and the WASM-safe verify policy. Behind the `attest` feature so it is an
/// opt-in extension, not a crypto tax on render/parse-only consumers. Signing
/// (native key generation) is in `ros-madair-emit`; this is the verify half.
#[cfg(feature = "attest")]
pub mod attest;

/// Native reader-side snapshot verification (recompute the id from files on disk,
/// then check the attestation). Held out of the wasm build — it reads the
/// filesystem; a browser reader verifies via [`attest::verify_bundle`] over
/// hashes it gathered itself. Used by both `ros-madair-emit`'s CLI and
/// `ros-madair-duck`'s read path, so the two agree on what "trusted" means.
#[cfg(all(not(target_arch = "wasm32"), feature = "attest"))]
pub mod verify;

/// The on-disk FORMAT version (P17), stamped into every artifact and gated by
/// every reader. Bump it whenever the head schema, the chunk framing, or the
/// manifest shape changes in a way an older reader would MISread rather than
/// fail to parse — the hazard the msgpack named-map chunks make silent (a
/// dropped/renamed field decodes cleanly to a default).
///
/// The manifest carries it in [`Manifest::format_version`], and a reader that
/// finds a version it does not implement REFUSES the snapshot, so an
/// emitter/reader skew is a loud, actionable error (re-emit) instead of a wrong
/// answer. `1` is the first frozen version — the format as it stands now.
///
/// `2` (alpha.16): the melt. The per-tile `concept_ids` JSON array and single
/// `q_ordered` column are gone from the tile row; concepts and dates now live in
/// melted `concepts_<slug>.parquet` / `ordered_<slug>.parquet` axis stores. A v1
/// reader would find no `concept_ids`/`q_ordered` columns and mis-answer every
/// facet/range query — exactly the silent-misread hazard this gates.
pub const FORMAT_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Snapshot id derivation (the ONE copy — writer and reader both call it)
// ---------------------------------------------------------------------------

/// Lowercase hex of a digest.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The canonical bytes of a manifest **for hashing**: compact JSON with
/// `snapshot_id` empty, and therefore omitted (the field is
/// `skip_serializing_if = "String::is_empty"`).
///
/// This is how the obvious self-reference resolves — the id must cover the
/// manifest, and the manifest must carry the id — hash the manifest *minus the
/// id*. Deterministic: struct field order is fixed and `fields` is a BTreeMap.
pub fn manifest_digest_bytes(manifest: &Manifest) -> Result<Vec<u8>, serde_json::Error> {
    debug_assert!(
        manifest.snapshot_id.is_empty(),
        "hash the manifest BEFORE setting the id it is being hashed to produce"
    );
    serde_json::to_vec(manifest)
}

/// The snapshot id: a hash over the hashes of every artifact — **including the
/// manifest** (id-excluded, see [`manifest_digest_bytes`]) — truncated to 16 hex.
///
/// The manifest is hashed like any other artifact so `snapshot_id` is a digest of
/// the *deployment* (handler set, tier definition, models), not merely the data:
/// two deployments that answer queries differently cannot claim the same id.
pub fn snapshot_id(artifacts: &[ArtifactEntry], manifest_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    for entry in artifacts {
        hasher.update(entry.sha256.as_bytes());
    }
    hasher.update(hex(&Sha256::digest(manifest_bytes)).as_bytes());
    hex(&hasher.finalize())[..16].to_string()
}

/// `manifest.json` — the layout/compatibility contract of one snapshot.
///
/// # The version field (P17)
///
/// An earlier iteration REMOVED the version field, deliberately: the old
/// `manifest_version`/`min_client_version` pair was written and never read, and
/// a version field nothing checks is worse than none — it advertises a
/// discipline that does not exist. The objection was never "no versions", it was
/// "no DISHONEST versions", and specifically that versioning one artifact of four
/// (manifest, but not the head schema, chunks, or closure) is ceremony.
///
/// [`format_version`](Self::format_version) answers that objection instead of
/// dodging it: it is READ — `ros_madair_read` refuses a snapshot whose version
/// it does not implement — and it is gated COHERENTLY, the same
/// [`FORMAT_VERSION`] stamped into the head (`PRAGMA user_version`) and every
/// chunk (framing header). It exists because the format is now stable enough to
/// ship, and the specific hazard it closes is real: chunk payloads are msgpack
/// named maps, so a dropped or renamed field decodes CLEANLY to a default — a
/// skewed reader misreads rather than failing. The version turns that silent
/// misread into a loud, actionable error (re-emit).
///
/// (`snapshot_id` still moves on any format change — the digest covers the
/// manifest — but the id only tells you two snapshots DIFFER, not that a reader
/// cannot READ one. The version is the thing a reader checks.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// The snapshot digest (hex, 16 chars).
    ///
    /// Serialization skips this field when empty, which is *load-bearing*: the
    /// emitter hashes the manifest **with the id excluded** (this field left
    /// empty) to resolve the self-reference — the id covers the manifest, and
    /// the manifest carries the id. See `ros_madair_emit`'s
    /// `manifest_digest_bytes`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snapshot_id: String,
    /// The on-disk [`FORMAT_VERSION`] this snapshot was emitted at (P17). A
    /// reader refuses a version it does not implement rather than misreading
    /// drifted fields. `#[serde(default)]` so a pre-P17 manifest deserializes to
    /// `0` — which no reader implements, so it is caught, not silently trusted.
    #[serde(default)]
    pub format_version: u32,
    pub base_uri: String,
    /// Present when this artifact graph was emitted as a named tier
    /// (M1.5): records the tier name and what was excluded, so the M2
    /// compiler can check tier permissions from the manifest alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<TierManifest>,
    /// The extension-type handlers this artifact was emitted with (I6). The
    /// query side rebuilds its registry from these rather than assuming a
    /// default: a registry mismatch is otherwise silent — a field that was
    /// never indexed (handler absent at emit) still compiles to valid SQL for
    /// a compiler that has the handler, and returns zero rows.
    pub handlers: Vec<HandlerDecl>,
    pub models: Vec<ModelManifest>,
    pub artifacts: Vec<ArtifactEntry>,
    pub budgets: Budgets,
}

impl Manifest {
    /// The model whose graph id is `graph_id`, if this snapshot carries it.
    /// Readers use it to pick the right spine table on a multi-model head.
    pub fn model_for_graph(&self, graph_id: &str) -> Option<&ModelManifest> {
        self.models.iter().find(|m| m.graph_id == graph_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierManifest {
    pub name: String,
    #[serde(default)]
    pub exclude_nodegroups: Vec<String>,
    #[serde(default)]
    pub exclude_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub slug: String,
    pub graph_id: String,
    pub spine_table: String,
    pub resource_count: usize,
    // NO per-field layout map. A field's storage/class is a pure function of its
    // datatype (concept → concept_tags, link → coarse, everything else →
    // detail-only), computed identically by emit and reader from `graph.json`
    // via `datatype_index_spec`. Materializing it here was a verbatim copy of
    // the graph plus a derivable value — I6 ("layout derivable from the schema")
    // in fact, not just in principle. If a corpus ever needs to index a strict
    // SUBSET of its concept fields, that is a pruned search graph (prune_graph),
    // not a per-field flag.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budgets {
    pub max_result_rows: u32,
    pub max_group_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `snapshot_id` is skipped when empty — this is what lets the emitter
    /// hash the manifest without the id it is about to compute.
    #[test]
    fn manifest_omits_empty_snapshot_id() {
        let manifest = Manifest {
            snapshot_id: String::new(),
            format_version: FORMAT_VERSION,
            base_uri: "https://example.org/".to_string(),
            tier: None,
            handlers: vec![],
            models: vec![],
            artifacts: vec![],
            budgets: Budgets {
                max_result_rows: 1000,
                max_group_count: 500,
            },
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(!json.contains("snapshot_id"), "{json}");
        // …and a manifest carrying an id round-trips (a reader must parse it).
        let with_id = Manifest {
            snapshot_id: "deadbeefdeadbeef".to_string(),
            ..manifest
        };
        let json = serde_json::to_string(&with_id).unwrap();
        let parsed: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.snapshot_id, "deadbeefdeadbeef");
    }
}
