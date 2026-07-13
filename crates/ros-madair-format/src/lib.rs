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

use alizarin_core::StaticTile;
use ros_madair_handlers::HandlerDecl;
use serde::{Deserialize, Serialize};

/// Manifest schema version.
///
/// 4: adds the `handlers` block — the artifact declares the extension-type
/// handler set it was emitted with (I6).
/// 5: the snapshot id now covers the manifest itself (id-excluded), so a
/// manifest-only change — handler set, tier, declared field classes — moves
/// the id. Snapshot ids from version-4 emits are NOT comparable with these;
/// nothing was ever published from that digest, so there is no compatibility
/// path and none is wanted (a back-compat "legacy id" would enshrine a digest
/// that ignores half the artifact).
pub const MANIFEST_VERSION: u32 = 5;

/// `manifest.json` — the layout/compatibility contract of one snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    /// The snapshot digest (hex, 16 chars).
    ///
    /// Serialization skips this field when empty, which is *load-bearing*: the
    /// emitter hashes the manifest **with the id excluded** (this field left
    /// empty) to resolve the self-reference — the id covers the manifest, and
    /// the manifest carries the id. See `ros_madair_emit`'s
    /// `manifest_digest_bytes`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snapshot_id: String,
    pub base_uri: String,
    pub min_client_version: String,
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
    /// alias -> physical location, keyed for the query compiler (M2).
    pub fields: BTreeMap<String, FieldEntry>,
    pub resource_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldEntry {
    pub node_id: String,
    pub datatype: String,
    /// "concept" | "link-coarse" | "detail-only".
    /// concept: exact rows in `concept_tags`; hierarchy queries via
    ///   `concept BETWEEN vocab.dfs_enter AND vocab.dfs_leave` (P10).
    /// link-coarse: NO exact head table — only
    ///   `chunk_link_summary(chunk, node, min_target, max_target, n)`;
    ///   the client plans coarse against the summary and resurfaces
    ///   exact pairs from the tile chunks (P1/P2). Exact head link
    ///   tables would be an opt-in, schema-declared field class.
    /// Everything else (strings, numbers, dates, geo, …) is
    /// detail-only in this iteration.
    pub storage: String,
    /// "filterable" | "coarse" | "detail-only". Datatype inference is
    /// only a proposal; a schema declaration
    /// (EmitOptions.field_classes / --field-classes) overrides it and
    /// is recorded here, so the M2 compiler can read head membership
    /// from the manifest alone (M1 item 3).
    pub class: String,
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
/// A chunk file is `rmp_serde::to_vec_named(&Vec<ChunkTile>)` — a msgpack
/// **map** keyed by field name, so absent optionals (`tileid`, `sortorder`)
/// decode cleanly. THE ENCODING IS FROZEN: chunk bytes are content-hashed and
/// feed the snapshot id, and every chunk already on disk must stay readable.
/// Do not rename, reorder-with-meaning, or retype these fields.
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

    /// The reader must recover exactly what the writer put in — the whole
    /// point of publishing this type (the hand-mirrored copy it replaces had
    /// no such guarantee).
    #[test]
    fn chunk_tile_roundtrips_through_named_msgpack() {
        let original = tile();
        let encoded =
            rmp_serde::to_vec_named(&vec![ChunkTile::from(&original)]).expect("encode");
        let decoded: Vec<ChunkTile> = rmp_serde::from_slice(&encoded).expect("decode");
        assert_eq!(decoded.len(), 1);
        let back: StaticTile = decoded.into_iter().next().unwrap().into();
        assert_eq!(back.data, original.data);
        assert_eq!(back.nodegroup_id, original.nodegroup_id);
        assert_eq!(back.resourceinstance_id, original.resourceinstance_id);
        assert_eq!(back.tileid, original.tileid);
        assert_eq!(back.parenttile_id, original.parenttile_id);
        assert_eq!(back.sortorder, original.sortorder);
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
            manifest_version: MANIFEST_VERSION,
            snapshot_id: String::new(),
            base_uri: "https://example.org/".to_string(),
            min_client_version: "0.1.0".to_string(),
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
