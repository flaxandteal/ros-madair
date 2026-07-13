// SPDX-License-Identifier: AGPL-3.0-or-later
//! The **native read path** for an emitted Rós Madair snapshot: given a head
//! directory and a resource UUID, recover that resource's tiles and hydrate
//! them into a schema-aware JSON tree.
//!
//! This is what a Tauri command (Gréasán) or a Python/CLI consumer does. It
//! used to exist only as `ros-madair-emit`'s `hydrate_entry` *example*, which
//! meant every consumer copied it — including a private re-declaration of the
//! chunk wire format. It is library code now; the wire types it needs are in
//! `ros-madair-format` (WASM-buildable), and this crate adds only the native
//! half: rusqlite + `std::fs`.
//!
//! Browser consumers do **not** use this crate — they query, and fetch chunks
//! over HTTP. That is why the format types are not in here.
//!
//! # The path (what a hydration actually costs)
//!
//! 1. open `head.sqlite` strictly READ-ONLY;
//! 2. resource UUID → `dict.term_id` → spine `rid` (the spine carries the
//!    term_id → rid mapping; `rid` is the emitter's sequential resource
//!    counter, NOT the dict id);
//! 3. `fragment_dir JOIN chunks` → the chunk hashes holding this rid's tiles;
//! 4. read `chunks/<hash>.msgpack` (a `Vec<ChunkTile>`, `to_vec_named`),
//!    keeping only the tiles whose `resourceinstance_id` is the target —
//!    chunks are content-addressed and pack up to 256 tiles from *many*
//!    resources;
//! 5. hydrate with the partial-safe `alizarin_core::resource_tiles_to_tree`.

use std::collections::BTreeSet;
use std::path::Path;

use alizarin_core::graph::{StaticGraph, StaticResourceMetadata};
use alizarin_core::json_conversion::resource_tiles_to_tree;
use alizarin_core::StaticTile;
use ros_madair_format::{ChunkTile, Manifest};
use rusqlite::{Connection, OpenFlags};

/// Everything that can go wrong reading a snapshot.
#[derive(Debug)]
pub enum ReadError {
    /// The head DB could not be opened or queried.
    Sqlite(rusqlite::Error),
    /// A chunk file (or the manifest) could not be read.
    Io(std::io::Error),
    /// A chunk file is not a valid `Vec<ChunkTile>` msgpack payload —
    /// i.e. the artifact was written by an incompatible emitter.
    Chunk {
        hash: String,
        source: rmp_serde::decode::Error,
    },
    /// `manifest.json` exists but is not a manifest this build understands.
    Manifest(serde_json::Error),
    /// No resource with this UUID in this snapshot (not in `dict`, or in
    /// `dict` but in no spine — e.g. it is a concept URI, or the resource was
    /// excluded by a tier).
    UnknownResource(String),
    /// The head DB has no `spine_*` table at all: not a Rós Madair head.
    NoSpine,
    /// The tiles were recovered but did not hydrate against the given graph
    /// (usually: `build_indices()` was never called, or the graph is not the
    /// model these tiles belong to).
    Hydration(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Sqlite(e) => write!(f, "head.sqlite: {e}"),
            ReadError::Io(e) => write!(f, "artifact read failed: {e}"),
            ReadError::Chunk { hash, source } => {
                write!(f, "chunk {hash}.msgpack is not decodable as tiles: {source}")
            }
            ReadError::Manifest(e) => write!(f, "manifest.json is not readable: {e}"),
            ReadError::UnknownResource(uuid) => {
                write!(f, "resource '{uuid}' is not in this snapshot")
            }
            ReadError::NoSpine => write!(
                f,
                "head.sqlite has no spine_* table — not a Rós Madair head"
            ),
            ReadError::Hydration(e) => write!(f, "hydration failed: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<rusqlite::Error> for ReadError {
    fn from(e: rusqlite::Error) -> Self {
        ReadError::Sqlite(e)
    }
}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        ReadError::Io(e)
    }
}

/// Open a snapshot's head DB strictly read-only.
pub fn open_head(head_dir: &Path) -> Result<Connection, ReadError> {
    Ok(Connection::open_with_flags(
        head_dir.join("head.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

/// The snapshot's manifest, if it is present beside the head.
///
/// Absent is not an error: a head directory is usable without it (the spine
/// tables are discoverable from `sqlite_master`), the manifest only makes the
/// model → spine mapping explicit.
pub fn load_manifest(head_dir: &Path) -> Result<Option<Manifest>, ReadError> {
    let path = head_dir.join("manifest.json");
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(ReadError::Manifest)
}

/// Spine tables to search for a resource, best candidate first.
///
/// # Multi-model heads
///
/// A head may carry several models, one `spine_<slug>` table each. `rid` is a
/// single sequential counter shared across ALL models (see the emitter's
/// `next_rid`), and `fragment_dir` is keyed by that global `rid` — so a
/// resource UUID lives in exactly one spine, and searching the spines in turn
/// is correct regardless of model count. When the manifest is present and
/// names a model with this `graph_id`, its `spine_table` goes first: that is
/// the intended, O(1) route, and the scan below is only the fallback for a
/// head shipped without its manifest.
///
/// (The previous implementation — `hydrate_entry.rs` — did
/// `SELECT name FROM sqlite_master WHERE name LIKE 'spine_%' LIMIT 1` and so
/// silently answered "unknown resource" for every model but the alphabetically
/// first one on a multi-model head. That is fixed here, not preserved.)
fn spine_candidates(
    conn: &Connection,
    head_dir: &Path,
    graph: Option<&StaticGraph>,
) -> Result<Vec<String>, ReadError> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'spine_%' \
         ORDER BY name",
    )?;
    let mut tables: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    if tables.is_empty() {
        return Err(ReadError::NoSpine);
    }
    if let (Some(graph), Some(manifest)) = (graph, load_manifest(head_dir)?) {
        if let Some(model) = manifest.model_for_graph(graph.graph_id()) {
            if let Some(pos) = tables.iter().position(|t| *t == model.spine_table) {
                tables.swap(0, pos);
            }
        }
    }
    Ok(tables)
}

/// Resolve a resource UUID to its spine `rid`.
fn resolve_rid(
    conn: &Connection,
    head_dir: &Path,
    uuid: &str,
    graph: Option<&StaticGraph>,
) -> Result<i64, ReadError> {
    let term_id: Option<i64> = conn
        .query_row("SELECT term_id FROM dict WHERE term = ?1", [uuid], |r| {
            r.get(0)
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    let Some(term_id) = term_id else {
        return Err(ReadError::UnknownResource(uuid.to_string()));
    };
    for spine in spine_candidates(conn, head_dir, graph)? {
        let rid: Option<i64> = conn
            .query_row(
                &format!("SELECT rid FROM {spine} WHERE term_id = ?1"),
                [term_id],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        if let Some(rid) = rid {
            return Ok(rid);
        }
    }
    Err(ReadError::UnknownResource(uuid.to_string()))
}

/// One resource's tiles, recovered from the chunks, in stable order.
///
/// Steps 1–4 of the read path: everything short of hydration, for consumers
/// that want the tiles rather than a tree (a diff, a re-index, a partial
/// render). `graph` is optional and is only a hint for spine selection on a
/// multi-model head — tile recovery itself needs no schema.
pub fn resource_tiles_with_graph(
    head_dir: &Path,
    uuid: &str,
    graph: Option<&StaticGraph>,
) -> Result<Vec<StaticTile>, ReadError> {
    let conn = open_head(head_dir)?;
    let rid = resolve_rid(&conn, head_dir, uuid, graph)?;

    // rid -> chunk hashes. DISTINCT: one resource's tiles for a nodegroup all
    // land in one chunk, but two nodegroups may share a chunk.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT c.hash FROM fragment_dir f \
         JOIN chunks c ON c.chunk = f.chunk WHERE f.rid = ?1",
    )?;
    let hashes: BTreeSet<String> = stmt
        .query_map([rid], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    let mut tiles: Vec<StaticTile> = Vec::new();
    for hash in &hashes {
        let bytes = std::fs::read(head_dir.join("chunks").join(format!("{hash}.msgpack")))?;
        let chunk: Vec<ChunkTile> =
            rmp_serde::from_slice(&bytes).map_err(|source| ReadError::Chunk {
                hash: hash.clone(),
                source,
            })?;
        for tile in chunk {
            if tile.resourceinstance_id == uuid {
                tiles.push(tile.into());
            }
        }
    }
    // Stable tile order: chunk membership order is an emitter accident.
    tiles.sort_by(|a, b| {
        (a.nodegroup_id.as_str(), a.tileid.as_deref())
            .cmp(&(b.nodegroup_id.as_str(), b.tileid.as_deref()))
    });
    Ok(tiles)
}

/// One resource's tiles, recovered from the chunks, in stable order.
/// See [`resource_tiles_with_graph`] if the head carries several models and
/// you want the manifest fast path.
pub fn resource_tiles(head_dir: &Path, uuid: &str) -> Result<Vec<StaticTile>, ReadError> {
    resource_tiles_with_graph(head_dir, uuid, None)
}

/// How many tiles `fragment_dir` says this resource has — the cross-check for
/// [`resource_tiles`] (they must agree; a mismatch means a chunk went missing
/// or the resource id is duplicated across chunks).
pub fn expected_tile_count(head_dir: &Path, uuid: &str) -> Result<i64, ReadError> {
    let conn = open_head(head_dir)?;
    let rid = resolve_rid(&conn, head_dir, uuid, None)?;
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(tile_count), 0) FROM fragment_dir WHERE rid = ?1",
        [rid],
        |r| r.get(0),
    )?)
}

/// Hydrate one resource into a schema-aware JSON tree (alias-keyed, nested by
/// nodegroup, partial-safe).
///
/// `graph` must be the model these tiles belong to, with `build_indices()`
/// already called — the head does not carry the schema; the caller ships it
/// (a Tauri command bundles the graph JSON alongside the artifact).
pub fn hydrate_resource(
    head_dir: &Path,
    uuid: &str,
    graph: &StaticGraph,
) -> Result<serde_json::Value, ReadError> {
    let tiles = resource_tiles_with_graph(head_dir, uuid, Some(graph))?;
    hydrate_tiles(&tiles, uuid, graph)
}

/// Hydrate already-recovered tiles (the second half of [`hydrate_resource`]),
/// for callers that got their tiles some other way — a query result, say.
pub fn hydrate_tiles(
    tiles: &[StaticTile],
    uuid: &str,
    graph: &StaticGraph,
) -> Result<serde_json::Value, ReadError> {
    // The head stores no resource metadata beyond a display name, and the
    // hydrator only needs the identity fields.
    let metadata = StaticResourceMetadata {
        descriptors: Default::default(),
        graph_id: graph.graph_id().to_string(),
        name: String::new(),
        resourceinstanceid: uuid.to_string(),
        publication_id: None,
        principaluser_id: None,
        legacyid: None,
        graph_publication_id: None,
        createdtime: None,
        lastmodified: None,
    };
    resource_tiles_to_tree(tiles, &metadata, graph).map_err(ReadError::Hydration)
}
