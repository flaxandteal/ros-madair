// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Rós Madair static-artifact **format** — the types that describe what a
//! snapshot *is*, shared by the one writer (`ros-madair-emit`) and every
//! reader (`ros-madair-read` natively, a Tauri command, a browser WASM
//! consumer).
//!
//! # Why this crate exists
//!
//! The format types used to live inside the emitter: [`Manifest`] was
//! serialize-only and [`ChunkTile`] was serialize-only *and* crate-private. A
//! reader therefore could not use them — it had to hand-mirror the wire shape
//! (which the Gréasán consumer duly did, twice). A hand-mirrored copy of a wire
//! format is a silent-drift machine: the emitter changes a field, the mirror
//! does not, and the reader deserializes nonsense.
//!
//! So the format is now its own crate, with `Serialize + Deserialize` on
//! everything, and:
//!
//! # WASM
//!
//! Browser deployments **query, never emit**. This crate is held to
//! `wasm32-unknown-unknown` (core + handler declarations + serde; no rusqlite,
//! no `std::fs`) so a browser can parse a manifest and decode chunk tiles
//! without linking the emitter's bundled SQLite writer. Anything needing the
//! filesystem or SQLite belongs in `ros-madair-read` (native) or
//! `ros-madair-emit` (native), not here.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use alizarin_core::StaticTile;
use ros_madair_handlers::HandlerDecl;
use serde::{Deserialize, Serialize};

/// Snapshot attestations (authenticity over `snapshot_id`) — DSSE/in-toto types
/// and the WASM-safe verify policy. Behind the `attest` feature so it is an
/// opt-in extension, not a crypto tax on render/parse-only consumers. Signing
/// (native key generation) is in `ros-madair-emit`; this is the verify half.
#[cfg(feature = "attest")]
pub mod attest;

/// The on-disk FORMAT version (P17), stamped into every artifact and gated by
/// every reader. Bump it whenever the head schema, the chunk framing, or the
/// manifest shape changes in a way an older reader would MISread rather than
/// fail to parse — the hazard the msgpack named-map chunks make silent (a
/// dropped/renamed field decodes cleanly to a default).
///
/// One number, three gates: the head carries it in `PRAGMA user_version`, the
/// manifest in [`Manifest::format_version`], and every chunk in its framing
/// header ([`encode_chunk`]). A reader that finds a version it does not implement
/// REFUSES the snapshot (see `ros_madair_read`), so an emitter/reader skew is a
/// loud, actionable error (re-emit) instead of a wrong answer. `1` is the first
/// frozen version — the format as it stands now.
pub const FORMAT_VERSION: u32 = 1;

// The chunk version byte is a u8; keep the format version inside that range (the
// manifest carries the full u32, so this coarse tripwire never needs more).
const _: () = assert!(FORMAT_VERSION <= u8::MAX as u32);

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

/// What one `emit()` produced (the CLI prints this; not an on-disk artifact).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitSummary {
    pub snapshot_id: String,
    pub models: usize,
    pub resources: usize,
    pub tiles: usize,
    pub chunks: usize,
    pub concepts: usize,
    pub dict_terms: usize,
    pub head_db_bytes: u64,
}

/// The wire shape of one tile inside `chunks/<hash>.msgpack`.
///
/// A chunk file is [`encode_chunk`] of a `&[ChunkTile]` — the P17 framing header
/// followed by `rmp_serde::to_vec_named`, a msgpack **map** keyed by field name,
/// so absent optionals (`tileid`, `sortorder`) decode cleanly. That clean-decode
/// is exactly why the framing exists: a renamed/dropped field would otherwise
/// misread silently, so the version header lets a reader REFUSE a skewed chunk
/// ([`decode_chunk`]). THE ENCODING IS FROZEN: chunk bytes are content-hashed and
/// feed the snapshot id, and every chunk already on disk must stay readable at
/// this format version. Changing these fields is a [`FORMAT_VERSION`] bump.
///
/// `data` is a `BTreeMap` (not `StaticTile`'s `HashMap`) deliberately: map key
/// order decides the chunk content hash, and a `HashMap` would randomize the
/// hash — and hence the snapshot id — per process.
///
/// It is `Cow`-based so the emitter can serialize straight out of a
/// `&StaticTile` with no copying (`ChunkTile::from(&tile)` → all
/// `Cow::Borrowed`), while a reader decoding from bytes gets owned data
/// (`Cow::Owned`) and can `.into()` a [`StaticTile`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkTile<'a> {
    #[serde(default)]
    pub data: BTreeMap<Cow<'a, str>, Cow<'a, serde_json::Value>>,
    pub nodegroup_id: Cow<'a, str>,
    pub resourceinstance_id: Cow<'a, str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tileid: Option<Cow<'a, str>>,
    #[serde(default)]
    pub parenttile_id: Option<Cow<'a, str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sortorder: Option<i32>,
}

impl<'a> From<&'a StaticTile> for ChunkTile<'a> {
    fn from(tile: &'a StaticTile) -> Self {
        ChunkTile {
            data: tile
                .data
                .iter()
                .map(|(k, v)| (Cow::Borrowed(k.as_str()), Cow::Borrowed(v)))
                .collect(),
            nodegroup_id: Cow::Borrowed(&tile.nodegroup_id),
            resourceinstance_id: Cow::Borrowed(&tile.resourceinstance_id),
            tileid: tile.tileid.as_deref().map(Cow::Borrowed),
            parenttile_id: tile.parenttile_id.as_deref().map(Cow::Borrowed),
            sortorder: tile.sortorder,
        }
    }
}

impl From<ChunkTile<'_>> for StaticTile {
    fn from(c: ChunkTile<'_>) -> Self {
        StaticTile {
            data: c
                .data
                .into_iter()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            nodegroup_id: c.nodegroup_id.into_owned(),
            resourceinstance_id: c.resourceinstance_id.into_owned(),
            tileid: c.tileid.map(Cow::into_owned),
            parenttile_id: c.parenttile_id.map(Cow::into_owned),
            // Provisional edits are an authoring-time concept and are never
            // emitted into a chunk (the artifact is published data).
            provisionaledits: None,
            sortorder: c.sortorder,
        }
    }
}

// ---------------------------------------------------------------------------
// Chunk framing (P17): a versioned header before the msgpack body
// ---------------------------------------------------------------------------

/// Magic marker beginning every framed chunk. Distinct from any msgpack array
/// prefix (a chunk body is `Vec<ChunkTile>`, so it starts `0x9_`/`0xdc`/`0xdd`),
/// so a pre-P17 (unframed) chunk fails the magic check rather than being mistaken
/// for version `0x52` ('R').
const CHUNK_MAGIC: [u8; 3] = *b"RMC";
/// Header length: 3 magic bytes + 1 version byte.
const CHUNK_HEADER_LEN: usize = 4;

/// Serialize tiles into the framed chunk bytes written to
/// `chunks/<hash>.msgpack`: `RMC` magic + [`FORMAT_VERSION`] byte + the msgpack
/// body. The header is part of the hashed bytes (the emitter hashes what this
/// returns), so a version bump moves every chunk's content hash — and the
/// snapshot id — by construction: a new format is a new identity.
pub fn encode_chunk(tiles: &[ChunkTile]) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let body = rmp_serde::to_vec_named(&tiles)?;
    let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + body.len());
    out.extend_from_slice(&CHUNK_MAGIC);
    out.push(FORMAT_VERSION as u8);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode framed chunk bytes back to tiles, REFUSING anything not written by a
/// matching-version emitter (P17). This is the gate that turns a silent named-map
/// misread into a loud error: a foreign/pre-P17 file fails the magic, and a
/// different format version fails the version check, before a single field is
/// deserialized.
pub fn decode_chunk(bytes: &[u8]) -> Result<Vec<ChunkTile<'_>>, ChunkDecodeError> {
    if bytes.len() < CHUNK_HEADER_LEN || bytes[..CHUNK_MAGIC.len()] != CHUNK_MAGIC {
        return Err(ChunkDecodeError::BadMagic);
    }
    let found = u32::from(bytes[3]);
    if found != FORMAT_VERSION {
        return Err(ChunkDecodeError::VersionSkew {
            found,
            expected: FORMAT_VERSION,
        });
    }
    rmp_serde::from_slice(&bytes[CHUNK_HEADER_LEN..]).map_err(ChunkDecodeError::Body)
}

/// Why a chunk would not decode — a framing/version failure kept distinct from a
/// body-decode failure, so a reader can say "wrong format version" rather than a
/// bare msgpack error.
#[derive(Debug)]
pub enum ChunkDecodeError {
    /// No `RMC` magic — a foreign file, or a pre-P17 unframed chunk.
    BadMagic,
    /// Framed, but a format version this reader does not implement.
    VersionSkew { found: u32, expected: u32 },
    /// Header was fine; the msgpack body did not decode.
    Body(rmp_serde::decode::Error),
}

impl fmt::Display for ChunkDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChunkDecodeError::BadMagic => write!(
                f,
                "not a Rós Madair chunk (missing RMC header) — a foreign or \
                 pre-versioning artifact; re-emit with a current ros-madair-emit"
            ),
            ChunkDecodeError::VersionSkew { found, expected } => write!(
                f,
                "chunk format version {found} but this reader implements \
                 {expected} — the artifact and the reader are skewed; re-emit"
            ),
            ChunkDecodeError::Body(e) => write!(f, "chunk body did not decode: {e}"),
        }
    }
}

impl std::error::Error for ChunkDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile() -> StaticTile {
        StaticTile {
            data: [
                ("b".to_string(), serde_json::json!({"en": {"value": "x"}})),
                ("a".to_string(), serde_json::json!(null)),
            ]
            .into_iter()
            .collect(),
            nodegroup_id: "ng".to_string(),
            resourceinstance_id: "res".to_string(),
            tileid: Some("t1".to_string()),
            parenttile_id: None,
            provisionaledits: None,
            sortorder: Some(3),
        }
    }

    /// The reader must recover exactly what the writer put in — through the
    /// framed [`encode_chunk`]/[`decode_chunk`] path the emitter and reader use.
    #[test]
    fn chunk_tile_roundtrips_through_framed_chunk() {
        let original = tile();
        let encoded = encode_chunk(&[ChunkTile::from(&original)]).expect("encode");
        let decoded = decode_chunk(&encoded).expect("decode");
        assert_eq!(decoded.len(), 1);
        let back: StaticTile = decoded.into_iter().next().unwrap().into();
        assert_eq!(back.data, original.data);
        assert_eq!(back.nodegroup_id, original.nodegroup_id);
        assert_eq!(back.resourceinstance_id, original.resourceinstance_id);
        assert_eq!(back.tileid, original.tileid);
        assert_eq!(back.parenttile_id, original.parenttile_id);
        assert_eq!(back.sortorder, original.sortorder);
    }

    /// P17: an unframed (pre-versioning) chunk — bare msgpack — is REFUSED, not
    /// silently decoded. This is the whole point: the named-map body would
    /// otherwise decode cleanly.
    #[test]
    fn a_pre_versioning_chunk_is_refused() {
        let bare = rmp_serde::to_vec_named(&vec![ChunkTile::from(&tile())]).unwrap();
        // Bare msgpack starts with an array marker, never the RMC magic.
        assert!(matches!(
            decode_chunk(&bare),
            Err(ChunkDecodeError::BadMagic)
        ));
    }

    /// P17: a chunk framed at a DIFFERENT version is refused with a skew error,
    /// naming both versions.
    #[test]
    fn a_version_skewed_chunk_is_refused() {
        let mut framed = encode_chunk(&[ChunkTile::from(&tile())]).unwrap();
        framed[3] = framed[3].wrapping_add(7); // corrupt the version byte
        match decode_chunk(&framed) {
            Err(ChunkDecodeError::VersionSkew { found, expected }) => {
                assert_eq!(expected, FORMAT_VERSION);
                assert_ne!(found, expected);
            }
            other => panic!("expected VersionSkew, got {other:?}"),
        }
    }

    /// Chunk encoding must be byte-stable across processes (the content hash
    /// is the chunk's identity and feeds the snapshot id): key order comes
    /// from the BTreeMap, not from `StaticTile`'s HashMap iteration order.
    #[test]
    fn chunk_encoding_is_key_order_stable() {
        let a = rmp_serde::to_vec_named(&vec![ChunkTile::from(&tile())]).unwrap();
        let b = rmp_serde::to_vec_named(&vec![ChunkTile::from(&tile())]).unwrap();
        assert_eq!(a, b);
    }

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
