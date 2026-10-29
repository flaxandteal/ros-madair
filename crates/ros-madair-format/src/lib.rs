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
// v3 adds the additive `resources_<graphid>.parquet` entry table (per-resource
// slug/descriptor/render-summary spine) and `grid_<graphid>.parquet` spatial
// cell edge. A v2 reader would simply not know them; a reader that lowers a
// `GridBox` or reads a render summary needs v3, so the version gates it.
// v4 adds a `geo_descriptor` column (a geojson `Point` — the resource's
// representative location descriptor) to the grid edge, so a spatial result draws
// its pins off the cell-pruned scan without reading the tiles. Additive, but a v4
// reader expects the column; the version gates it.
// v5 adds a `value_ids` column (List<Utf8>) to `concept_catalog.parquet`: the
// prefLabel VALUE ids each concept owns. A tile stores the VALUE id (Arches:
// `concept.id != prefLabels.en.id`), while the catalog is keyed on the concept id,
// so without this bridge hydrate cannot resolve a reference/concept tile to its
// label and the raw UUID leaks. A v5 reader's `concept_labels` unnests it; the
// version gates it. (The edge `predicate` column + EDTF `q_lo/q_hi` + the
// entry-table reconcile remain pending and will share the next bump — not four.)
pub const FORMAT_VERSION: u32 = 5;

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
/// dodging it: it is READ — `ros-madair-duck`'s `open_layers` refuses a layer
/// whose version it does not implement (`format_version != FORMAT_VERSION`),
/// before it touches a single column. It exists because the format is now stable
/// enough to ship, and the specific hazard it closes is real: a dropped or
/// renamed Parquet column would otherwise surface as a cryptic "no such column"
/// (or worse, decode to a default) deep in a query. The version turns that into a
/// loud, actionable error at open time (re-emit).
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
    /// Owner-declared provenance & currency metadata (M-prov), present when the
    /// emitter was given it. See [`ProvenanceManifest`]. Optional and skipped
    /// when absent, so a head emitted without it hashes — and therefore
    /// `snapshot_id`s — exactly as before (purely additive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceManifest>,
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

/// Owner-declared provenance & currency metadata for one emitted head.
///
/// This is **presentation and policy**, NOT cryptographic identity: the
/// attestation's signing key is what *proves* who emitted a head; these fields
/// are the human-facing label, data-currency, recognition colour, and refresh
/// cadence a reader shows *beside* that proof. A reviewer-register UI shows the
/// key fingerprint; a field-register UI shows `owner_display_name`.
///
/// **Why this earns a manifest field when per-field layout did not** (see
/// [`ModelManifest`]): these are **not derivable** from the schema — they are
/// facts only the owner knows, declared at emit. And they are **domain-agnostic**:
/// every slice has an owner, a currency date, a display colour, and a refresh
/// cadence — a Goidelic-dictionary slice exactly as much as a building slice. RM
/// stores and compares them; it names no domain default (the "ageing after 6h"
/// threshold is the *owner's* policy expressed in `freshness_window_secs`, never
/// RM's).
///
/// All four are caller-provided at emit (flags/config), never wall-clock — so the
/// manifest, and therefore `snapshot_id`, stays reproducible for identical input.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvenanceManifest {
    /// Human-readable owner/source name for the field-register trust UI, e.g.
    /// `"City Council · Asset Register"`. The attestation carries the signer;
    /// this is the label shown next to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_display_name: Option<String>,
    /// The data's currency: when the OWNER asserts this content was valid — NOT
    /// the build time (owner-provided, so deterministic and meaningful). RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Owner-declared recognition colour (hex, e.g. `"#2F7F7A"`) for the per-fact
    /// provenance swatch. A recognition aid only — never the sole carrier of
    /// identity (always paired with the name in the UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_colour: Option<String>,
    /// Owner-declared freshness window, in seconds: past `as_of + window` a reader
    /// SHOULD present the slice as "ageing". The number is the owner's policy; RM
    /// only stores and compares it, and names no domain default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_window_secs: Option<u64>,
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
            provenance: None,
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

    /// Provenance is additive: `None` is skipped (so an undeclared head hashes
    /// exactly as a pre-provenance one would), and a declared block round-trips.
    #[test]
    fn provenance_is_additive_and_round_trips() {
        let base = Manifest {
            snapshot_id: String::new(),
            format_version: FORMAT_VERSION,
            base_uri: "https://example.org/".to_string(),
            tier: None,
            provenance: None,
            handlers: vec![],
            models: vec![],
            artifacts: vec![],
            budgets: Budgets {
                max_result_rows: 1000,
                max_group_count: 500,
            },
        };
        // None → the key never appears, so the manifest bytes (and snapshot_id)
        // are identical to a build that predates the field.
        let json = serde_json::to_string(&base).unwrap();
        assert!(!json.contains("provenance"), "{json}");

        let declared = Manifest {
            provenance: Some(ProvenanceManifest {
                owner_display_name: Some("City Council · Asset Register".to_string()),
                as_of: Some("2026-10-22T00:00:00Z".to_string()),
                owner_colour: Some("#2F7F7A".to_string()),
                freshness_window_secs: Some(2_592_000),
            }),
            ..base
        };
        let parsed: Manifest = serde_json::from_str(&serde_json::to_string(&declared).unwrap()).unwrap();
        let p = parsed.provenance.expect("provenance present");
        assert_eq!(p.owner_display_name.as_deref(), Some("City Council · Asset Register"));
        assert_eq!(p.freshness_window_secs, Some(2_592_000));
        // A pre-provenance reader (deserializing JSON lacking the key) gets None.
        let legacy: Manifest =
            serde_json::from_str(r#"{"format_version":3,"base_uri":"x","handlers":[],"models":[],"artifacts":[],"budgets":{"max_result_rows":1,"max_group_count":1}}"#)
                .unwrap();
        assert!(legacy.provenance.is_none());
    }
}
