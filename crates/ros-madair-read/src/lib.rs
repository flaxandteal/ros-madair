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
//! 4. read `chunks/<hash>.msgpack` (framed `Vec<ChunkTile>` via `decode_chunk`,
//!    which gates the P17 version header), keeping only the tiles whose
//!    `resourceinstance_id` is the target —
//!    chunks are content-addressed and pack up to 256 tiles from *many*
//!    resources;
//! 5. hydrate with the partial-safe `alizarin_core::resource_tiles_to_tree`.
//!
//! # Composition
//!
//! Several snapshots read as one — a shipped base plus on-device overlays — is
//! [`Layers`]. Layers do NOT share a dictionary, so nothing may be joined
//! across them but the resource UUID string; see that module for the full
//! consequences (P13 precedence, why counts cannot be summed, the tile merge
//! rule).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alizarin_core::graph::{StaticGraph, StaticResourceMetadata};
use alizarin_core::json_conversion::{resource_tiles_to_tree, resource_tiles_to_tree_with_context};
use alizarin_core::type_serialization::{ExternalResolver, SerializationContext, SerializationOptions};
use alizarin_core::StaticTile;
use ros_madair_format::{decode_chunk, ChunkDecodeError, Manifest, FORMAT_VERSION};
use ros_madair_handlers::default_registry;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

mod layers;
pub use layers::{Layer, Layers};

/// A content-addressed chunk cache (RM principle P15).
///
/// The read path re-reads `chunks/<hash>.msgpack` on every hydrate and every
/// `cited_by` scan. On local SQLite that is harmless (the OS page cache absorbs
/// it); over HTTP/CDN — the browser/Tauri consumer — each read is a network
/// fetch, and the A7 reverse-lookup flow (`cited_by` a target, then hydrate each
/// citer) re-reads the very chunks the scan just touched. This memoizes chunk
/// BYTES so a chunk is fetched once per session.
///
/// **It is keyed by content hash alone, and that is why it is correct.** A chunk
/// file's name IS the sha256 of its bytes, so a hash uniquely identifies immutable
/// content — the cache can never go stale, and a hash shared across layers (same
/// content) is legitimately one entry. This is the "conscious" block cache the
/// principle inventory calls for, not an accidental one.
///
/// Bytes, not parsed tiles: the fetch is the cost over a network; re-parsing
/// cached bytes is negligible CPU and keeps the cache free of the borrowed-`Cow`
/// lifetimes `ChunkTile` carries. Unbounded and session-lived (held on
/// [`Layers`]); a size cap / LRU is a later refinement if a session's working set
/// outgrows memory.
#[derive(Debug, Default)]
pub struct ChunkCache {
    bytes: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ChunkCache {
    /// The bytes of `chunks/<hash>.msgpack`, from cache if seen, else read from
    /// `dir` and memoized. `dir` is only consulted on a miss — a cache hit never
    /// touches the filesystem (or the network), which is the whole point.
    pub fn read_bytes(&self, dir: &Path, hash: &str) -> Result<Arc<Vec<u8>>, ReadError> {
        if let Some(b) = self.bytes.lock().unwrap().get(hash) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(b));
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let bytes = Arc::new(std::fs::read(
            dir.join("chunks").join(format!("{hash}.msgpack")),
        )?);
        self.bytes
            .lock()
            .unwrap()
            .insert(hash.to_string(), Arc::clone(&bytes));
        Ok(bytes)
    }

    /// Reads served from cache (no fetch) this session.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Reads that missed and fetched from disk/network this session.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Distinct chunks resident in the cache.
    pub fn len(&self) -> usize {
        self.bytes.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Everything that can go wrong reading a snapshot.
#[derive(Debug)]
pub enum ReadError {
    /// The head DB could not be opened or queried.
    Sqlite(rusqlite::Error),
    /// A chunk file (or the manifest) could not be read.
    Io(std::io::Error),
    /// A chunk file did not decode — a bad/absent P17 framing header, a format
    /// version this reader does not implement, or a corrupt msgpack body.
    Chunk {
        hash: String,
        source: ChunkDecodeError,
    },
    /// An artifact's on-disk FORMAT version (P17) is not the one this reader
    /// implements. Names the artifact (head / manifest) so "re-emit" is
    /// actionable; the head defaults to `0` when never stamped (pre-P17).
    FormatSkew {
        artifact: String,
        found: u32,
        expected: u32,
    },
    /// `manifest.json` exists but does not parse as one.
    ///
    /// The likeliest cause by far is a STALE ARTIFACT — the manifest gains and
    /// loses fields as the format moves, so an older snapshot fails here with
    /// something like `missing field 'handlers'`, which names neither the cause
    /// nor the fix. `load_manifest` is eager and sits in the read path, so this
    /// error takes the whole snapshot down; it therefore says what to do about
    /// it. (No version gate: nothing is published, nothing else in the system is
    /// versioned — head schema, chunk encoding and closure all move freely — and
    /// re-emit takes seconds. The hint is the whole fix.)
    ///
    /// It carries the `path`, which is also what composition needs: reading a
    /// stack means reading several manifests, and "which one" is the first thing
    /// you ask.
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// A layer directory has no `manifest.json`. Optional for a single-snapshot
    /// read; REQUIRED for composition, which has nothing to validate without it.
    MissingManifest(PathBuf),
    /// Two layers cannot be composed: they disagree about something the
    /// compiled SQL assumes is shared (see `Layers::open`).
    Incompatible {
        base: PathBuf,
        layer: PathBuf,
        what: String,
        base_value: String,
        layer_value: String,
    },
    /// `Layers::open` was handed no directories.
    NoLayers,
    /// No layer in the stack carries this model, so there is no spine table to
    /// query and no composed view to speak of.
    ModelInNoLayer(String),
    /// The query did not compile against the graph.
    Query(ros_madair_query::QueryError),
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
    /// Composing the layers' tiles for one resource failed in
    /// `alizarin_core`'s merge — a data conflict the merge would not silently
    /// resolve, not a bug in the layer stack.
    Merge(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Sqlite(e) => write!(f, "head.sqlite: {e}"),
            ReadError::Io(e) => write!(f, "artifact read failed: {e}"),
            ReadError::Chunk { hash, source } => {
                write!(
                    f,
                    "chunk {hash}.msgpack is not decodable as tiles: {source}"
                )
            }
            ReadError::FormatSkew {
                artifact,
                found,
                expected,
            } => write!(
                f,
                "{artifact} is format version {found}, but this reader implements \
                 {expected} — the artifact and the reader are skewed; re-emit with \
                 a matching ros-madair-emit (there is no in-place migration)"
            ),
            ReadError::Manifest { path, source } => write!(
                f,
                "failed to parse manifest at {}: {source} — if the artifact \
                 predates a format change, re-emit it with ros-madair-emit \
                 (there is no compatibility path: old snapshots are not \
                 readable, only re-creatable)",
                path.display()
            ),
            ReadError::MissingManifest(dir) => write!(
                f,
                "layer {} has no manifest.json — composition requires one per \
                 layer (there is nothing to check compatibility against without it)",
                dir.display()
            ),
            ReadError::Incompatible {
                base,
                layer,
                what,
                base_value,
                layer_value,
            } => write!(
                f,
                "layers are not composable: {what} differs — {} says '{base_value}', \
                 {} says '{layer_value}'",
                base.display(),
                layer.display()
            ),
            ReadError::NoLayers => write!(f, "no layers given"),
            ReadError::ModelInNoLayer(graph_id) => {
                write!(f, "no layer in this stack carries model '{graph_id}'")
            }
            ReadError::Query(e) => write!(f, "query does not compile: {e}"),
            ReadError::UnknownResource(uuid) => {
                write!(f, "resource '{uuid}' is not in this snapshot")
            }
            ReadError::NoSpine => write!(
                f,
                "head.sqlite has no spine_* table — not a Rós Madair head"
            ),
            ReadError::Hydration(e) => write!(f, "hydration failed: {e}"),
            ReadError::Merge(e) => write!(f, "layer composition failed: {e}"),
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

/// Open a snapshot's head DB strictly read-only, gating on its P17 format
/// version. A head stamps [`FORMAT_VERSION`] into `PRAGMA user_version`
/// (defaulting to `0` if never stamped — a pre-P17 head), and a mismatch is
/// refused here rather than surfacing later as a missing table or a misread
/// column. This is the head arm of the coherent version gate (chunks and the
/// manifest are gated at their own read points).
pub fn open_head(head_dir: &Path) -> Result<Connection, ReadError> {
    let conn = Connection::open_with_flags(
        head_dir.join("head.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != FORMAT_VERSION {
        return Err(ReadError::FormatSkew {
            artifact: format!("head.sqlite ({})", head_dir.display()),
            found: version,
            expected: FORMAT_VERSION,
        });
    }
    Ok(conn)
}

/// Gate a parsed manifest on its P17 [`format_version`](Manifest::format_version)
/// — the manifest arm of the coherent version gate. A pre-P17 manifest lacks the
/// field and deserializes to `0`, which no reader implements, so it is caught.
fn check_manifest_format(manifest: &Manifest, path: &Path) -> Result<(), ReadError> {
    if manifest.format_version != FORMAT_VERSION {
        return Err(ReadError::FormatSkew {
            artifact: format!("manifest.json ({})", path.display()),
            found: manifest.format_version,
            expected: FORMAT_VERSION,
        });
    }
    Ok(())
}

/// The snapshot's manifest, if it is present beside the head.
///
/// Absent is not an error: a head directory is usable without it (the spine
/// tables are discoverable from `sqlite_master`), the manifest only makes the
/// model → spine mapping explicit. (Composition is stricter — see
/// [`Layers::open`], which requires one.)
///
/// PRESENT-BUT-UNPARSEABLE **is** an error, and an eager one: `spine_candidates`
/// calls this on every hydrate, so a stale manifest takes the whole snapshot
/// down. That is why the failure names the artifact and the fix rather than
/// surfacing a bare `missing field 'handlers'` (see [`ReadError::Manifest`]).
pub fn load_manifest(head_dir: &Path) -> Result<Option<Manifest>, ReadError> {
    let path = head_dir.join("manifest.json");
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|source| ReadError::Manifest {
            path: path.clone(),
            source,
        })?;
    check_manifest_format(&manifest, &path)?;
    Ok(Some(manifest))
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
    // One-shot: no cache (a fresh cache per call would never hit). The cached
    // path is [`Layers`], which reuses one cache across many hydrates/queries.
    resource_tiles_cached(head_dir, uuid, graph, None)
}

/// [`resource_tiles_with_graph`], but chunk bytes are fetched through `cache`
/// (P15) when one is given — so a chunk touched by an earlier read in the same
/// session is not fetched again. `None` reads straight from disk.
pub(crate) fn resource_tiles_cached(
    head_dir: &Path,
    uuid: &str,
    graph: Option<&StaticGraph>,
    cache: Option<&ChunkCache>,
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
        let bytes = read_chunk_bytes(head_dir, hash, cache)?;
        let chunk = decode_chunk(bytes.as_slice()).map_err(|source| ReadError::Chunk {
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

/// Hydrate many resources with **one** head connection and — the point — each
/// needed chunk **decoded once**, its tiles bucketed to every requested resource
/// in it. A chunk packs ~256 tiles from many resources, so the per-resource path
/// re-decodes shared chunks N times; this collapses that to one decode per chunk.
/// Missing UUIDs are skipped. Returns `(uuid, tiles)` for each resource found,
/// in input order.
pub fn hydrate_many(
    head_dir: &Path,
    uuids: &[String],
) -> Result<Vec<(String, Vec<StaticTile>)>, ReadError> {
    use std::collections::{BTreeSet, HashMap};

    let conn = open_head(head_dir)?;

    // uuid -> tiles bucket (only for requested resources), and the union of the
    // chunk hashes that hold any of their tiles.
    let mut buckets: HashMap<&str, Vec<StaticTile>> =
        uuids.iter().map(|u| (u.as_str(), Vec::new())).collect();
    let mut hashes: BTreeSet<String> = BTreeSet::new();

    let mut stmt = conn.prepare(
        "SELECT DISTINCT c.hash FROM fragment_dir f \
         JOIN chunks c ON c.chunk = f.chunk WHERE f.rid = ?1",
    )?;
    for uuid in uuids {
        let rid = match resolve_rid(&conn, head_dir, uuid, None) {
            Ok(rid) => rid,
            Err(ReadError::UnknownResource(_)) => continue,
            Err(e) => return Err(e),
        };
        for hash in stmt.query_map([rid], |r| r.get::<_, String>(0))? {
            hashes.insert(hash?);
        }
    }

    // Decode each unique chunk once; hand each tile to its resource's bucket.
    for hash in &hashes {
        let bytes = read_chunk_bytes(head_dir, hash, None)?;
        let chunk = decode_chunk(bytes.as_slice()).map_err(|source| ReadError::Chunk {
            hash: hash.clone(),
            source,
        })?;
        for tile in chunk {
            if let Some(bucket) = buckets.get_mut(&*tile.resourceinstance_id) {
                bucket.push(tile.into());
            }
        }
    }

    let mut out = Vec::with_capacity(uuids.len());
    for uuid in uuids {
        if let Some(mut tiles) = buckets.remove(uuid.as_str()) {
            if tiles.is_empty() {
                continue;
            }
            tiles.sort_by(|a, b| {
                (a.nodegroup_id.as_str(), a.tileid.as_deref())
                    .cmp(&(b.nodegroup_id.as_str(), b.tileid.as_deref()))
            });
            out.push((uuid.clone(), tiles));
        }
    }
    Ok(out)
}

/// [`hydrate_many`] restricted to specific nodegroups — targeted extraction.
///
/// Reads only the chunks that `fragment_dir` says hold the requested nodegroups
/// for the requested resources (a wide resource's other nodegroups' chunks are
/// never touched or decoded), and returns only those nodegroups' tiles. This is
/// the v2 analogue of alizarin's `get_values_at_path`: to read one field you
/// pay for one nodegroup, not the whole resource. `nodegroups` are node-group
/// UUIDs; empty means "all" (delegates to [`hydrate_many`]).
pub fn hydrate_nodegroups(
    head_dir: &Path,
    uuids: &[String],
    nodegroups: &[String],
) -> Result<Vec<(String, Vec<StaticTile>)>, ReadError> {
    use std::collections::{BTreeSet, HashMap, HashSet};

    if nodegroups.is_empty() {
        return hydrate_many(head_dir, uuids);
    }
    let conn = open_head(head_dir)?;

    // nodegroup uuids -> dict term ids (fragment_dir.nodegroup is interned).
    let mut ng_tids: Vec<i64> = Vec::with_capacity(nodegroups.len());
    for ng in nodegroups {
        if let Ok(tid) =
            conn.query_row("SELECT term_id FROM dict WHERE term = ?1", [ng], |r| r.get::<_, i64>(0))
        {
            ng_tids.push(tid);
        }
    }
    if ng_tids.is_empty() {
        return Ok(Vec::new());
    }
    let ng_set: HashSet<&str> = nodegroups.iter().map(|s| s.as_str()).collect();

    let placeholders = (2..2 + ng_tids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT c.hash FROM fragment_dir f JOIN chunks c ON c.chunk = f.chunk \
         WHERE f.rid = ?1 AND f.nodegroup IN ({placeholders})",
    );
    let mut stmt = conn.prepare(&sql)?;

    let mut buckets: HashMap<&str, Vec<StaticTile>> =
        uuids.iter().map(|u| (u.as_str(), Vec::new())).collect();
    let mut hashes: BTreeSet<String> = BTreeSet::new();
    for uuid in uuids {
        let rid = match resolve_rid(&conn, head_dir, uuid, None) {
            Ok(rid) => rid,
            Err(ReadError::UnknownResource(_)) => continue,
            Err(e) => return Err(e),
        };
        let mut params: Vec<i64> = Vec::with_capacity(1 + ng_tids.len());
        params.push(rid);
        params.extend(&ng_tids);
        for hash in stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            r.get::<_, String>(0)
        })? {
            hashes.insert(hash?);
        }
    }

    // Decode each selected chunk once; keep only tiles for a requested resource
    // AND a requested nodegroup (a chunk may pack other nodegroups too).
    for hash in &hashes {
        let bytes = read_chunk_bytes(head_dir, hash, None)?;
        let chunk = decode_chunk(bytes.as_slice()).map_err(|source| ReadError::Chunk {
            hash: hash.clone(),
            source,
        })?;
        for tile in chunk {
            if ng_set.contains(&*tile.nodegroup_id) {
                if let Some(bucket) = buckets.get_mut(&*tile.resourceinstance_id) {
                    bucket.push(tile.into());
                }
            }
        }
    }

    let mut out = Vec::with_capacity(uuids.len());
    for uuid in uuids {
        if let Some(mut tiles) = buckets.remove(uuid.as_str()) {
            if tiles.is_empty() {
                continue;
            }
            tiles.sort_by(|a, b| {
                (a.nodegroup_id.as_str(), a.tileid.as_deref())
                    .cmp(&(b.nodegroup_id.as_str(), b.tileid.as_deref()))
            });
            out.push((uuid.clone(), tiles));
        }
    }
    Ok(out)
}


/// Chunk bytes via the cache if one is supplied (P15), else a direct read.
/// Returns an `Arc` either way so callers share one representation.
pub(crate) fn read_chunk_bytes(
    dir: &Path,
    hash: &str,
    cache: Option<&ChunkCache>,
) -> Result<Arc<Vec<u8>>, ReadError> {
    match cache {
        Some(c) => c.read_bytes(dir, hash),
        None => Ok(Arc::new(std::fs::read(
            dir.join("chunks").join(format!("{hash}.msgpack")),
        )?)),
    }
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

/// Hydrate one resource into a schema-aware **display** JSON tree (alias-keyed,
/// nested by nodegroup, partial-safe), rendered against a language preference
/// chain.
///
/// Display tree: values come back resolved and localised — i18n strings walk
/// `languages` in order (first present wins, then first-available), and
/// `concept`/`reference` ids render as labels (from this head's `vocab`+`dict`).
/// Consumers read values directly; they do not re-resolve UUIDs or pick a
/// language themselves. Pass a single-element slice (e.g. `&["en"]`) for one
/// language, or an ordered chain (e.g. `&["gd", "ga", "en"]`) for controlled
/// fallback; an empty slice defaults to `en`.
///
/// `graph` must be the model these tiles belong to, with `build_indices()`
/// already called — the head does not carry the schema; the caller ships it
/// (a Tauri command bundles the graph JSON alongside the artifact).
pub fn hydrate_resource(
    head_dir: &Path,
    uuid: &str,
    graph: &StaticGraph,
    languages: &[&str],
) -> Result<serde_json::Value, ReadError> {
    let tiles = resource_tiles_with_graph(head_dir, uuid, Some(graph))?;
    // Reference/concept labels live in this head's `vocab`+`dict`; fold them so
    // the tree hydrates with rendered labels instead of raw UUIDs.
    let conn = open_head(head_dir)?;
    let mut labels = HashMap::new();
    read_vocab_labels(&conn, &mut labels)?;
    hydrate_tiles_with_labels(&tiles, uuid, graph, &labels, languages)
}

/// Hydrate already-recovered tiles (the second half of [`hydrate_resource`]),
/// for callers that got their tiles some other way — a query result, say.
///
/// # Descriptors are RE-DERIVED here, from *these* tiles
///
/// The resource's descriptor (its `name`) is recomputed by rendering the graph's
/// descriptor template against the tiles handed in — not read from any stored
/// copy. This is what makes composition correct: [`Layers::hydrate_resource`]
/// passes the COMPOSED tiles, so the composed `name` is the descriptor of the
/// merged view, and a shared entry gets the layer's real headword rather than a
/// lower layer's `<Headword>` placeholder. (This is the rule the batch merge
/// applies with `recompute_descriptors=true`, and the opposite of what
/// `merge_resources` does alone — it copies the first resource's descriptor
/// wholesale, which for topmost-first layers would be the placeholder.)
///
/// **Cost:** `build_descriptors` now runs on `&StaticGraph` directly — no graph
/// clone per call (it used to build an `IndexedGraph`, cloning the whole graph
/// plus every node, which made a fan-out of hydrations — e.g. `cited_by` then
/// hydrate each citer — allocate megabytes per resource). Still a single-resource
/// operation: a list of names is a query over `spine.display_name` (A1), not a
/// hydrate per row.
pub fn hydrate_tiles(
    tiles: &[StaticTile],
    uuid: &str,
    graph: &StaticGraph,
) -> Result<serde_json::Value, ReadError> {
    // Re-derive the descriptor from the composed tiles (see the doc above),
    // directly on the borrowed graph — no clone.
    let metadata = build_metadata(uuid, graph, tiles);
    resource_tiles_to_tree(tiles, &metadata, graph).map_err(ReadError::Hydration)
}

/// Hydrate tiles into a tree with **reference/concept labels resolved**, instead
/// of the raw UUIDs [`hydrate_tiles`] leaves behind.
///
/// `labels` is a `uuid -> label` map read from the head's `vocab`+`dict` (build
/// it with [`read_vocab_labels`]; the composed path folds it across every layer,
/// base-first, so the topmost layer wins). Rendering runs through alizarin's
/// Display-mode tree builder: `concept`/`concept-list` resolve via the built-in
/// serializer, and `reference` through the CLM handler in [`default_registry`] —
/// both driven by the one [`VocabResolver`]. Resolution happens inside the tree
/// build; nothing re-interprets the tree afterward.
///
/// [`hydrate_resource`] calls this after reading the head. Tile-only callers (a
/// query result, say) that have a label map can call it directly; those without
/// one keep using [`hydrate_tiles`] and get raw UUIDs.
pub fn hydrate_tiles_with_labels(
    tiles: &[StaticTile],
    uuid: &str,
    graph: &StaticGraph,
    labels: &HashMap<String, String>,
    languages: &[&str],
) -> Result<serde_json::Value, ReadError> {
    let metadata = build_metadata(uuid, graph, tiles);
    let resolver = VocabResolver { labels };
    let registry = default_registry();
    // `options` carries mode + the language preference chain; `ctx` carries the
    // resolvers/registry. `resource_resolver` is None — resource-instance display
    // names are a separate concern from concept/reference labels. `vocab.label`
    // is a single flat label per concept, so the resolver ignores language; the
    // chain only selects among i18n string datatypes.
    let options = SerializationOptions::display_seq(languages.iter().copied());
    let ctx = SerializationContext {
        node_config: None,
        external_resolver: Some(&resolver),
        // concept_lookup threads emit-side concept identity; the read/hydration path
        // resolves concepts via `external_resolver`, so None (added when alizarin's
        // 5da3e02 made the field required; keeps the parquet substrate compiling
        // against the current m1-emitter core).
        concept_lookup: None,
        resource_resolver: None,
        extension_registry: Some(&registry),
    };
    resource_tiles_to_tree_with_context(tiles, &metadata, graph, &options, &ctx)
        .map_err(ReadError::Hydration)
}

/// Build the resource metadata (descriptor re-derived from *these* tiles) shared
/// by every hydrate entry point — see [`hydrate_tiles`] for why the descriptor is
/// recomputed here rather than read from a stored copy.
fn build_metadata(uuid: &str, graph: &StaticGraph, tiles: &[StaticTile]) -> StaticResourceMetadata {
    let descriptors = graph.build_descriptors(tiles);
    StaticResourceMetadata {
        graph_id: graph.graph_id().to_string(),
        name: descriptors.name.clone().unwrap_or_default(),
        descriptors,
        resourceinstanceid: uuid.to_string(),
        publication_id: None,
        principaluser_id: None,
        legacyid: None,
        graph_publication_id: None,
        createdtime: None,
        lastmodified: None,
    }
}

/// Resolves concept / reference list-item UUIDs to display labels straight from
/// the head's `vocab`+`dict`. Flat and language-agnostic: `vocab.label` is one
/// label per concept, so the collection and language arguments are ignored.
struct VocabResolver<'a> {
    labels: &'a HashMap<String, String>,
}

impl ExternalResolver for VocabResolver<'_> {
    fn resolve_concept(
        &self,
        _collection: &str,
        concept_id: &str,
        _language: &str,
    ) -> Option<String> {
        self.labels.get(concept_id).cloned()
    }
}

/// Fold a head's `vocab.label` into `out` as a `uuid -> label` map, joining
/// `vocab` to `dict` on the concept term_id. A head with no `vocab` table — an
/// overlay carrying no concepts — contributes nothing rather than erroring.
/// Later calls overwrite earlier keys, so callers fold **base-first** to let the
/// topmost layer win.
pub fn read_vocab_labels(
    conn: &Connection,
    out: &mut HashMap<String, String>,
) -> Result<(), ReadError> {
    let has_vocab = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='vocab'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_vocab {
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "SELECT d.term, v.label FROM vocab v \
         JOIN dict d ON d.term_id = v.concept \
         WHERE v.label IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (uuid, label) = row?;
        if !label.is_empty() {
            out.insert(uuid, label);
        }
    }
    Ok(())
}
