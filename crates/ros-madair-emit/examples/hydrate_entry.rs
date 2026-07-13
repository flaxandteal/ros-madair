// SPDX-License-Identifier: AGPL-3.0-or-later
//! Macbain pilot — the Tauri-NATIVE read path (no WASM).
//!
//! Given an emitted head directory + a resource UUID, this reproduces
//! exactly what a Gréasán Tauri command would do to hydrate one dictionary
//! entry for a single static layer:
//!
//!   1. open `head.sqlite` strictly read-only (rusqlite);
//!   2. map the resource UUID -> dict term_id -> spine `rid`
//!      (fragment_dir/spine `rid` is the sequential resource counter, NOT
//!      the dict id — spine carries the term_id -> rid mapping), then look
//!      up the chunk hashes for that rid via `fragment_dir JOIN chunks`;
//!   3. read `chunks/<hash>.msgpack` (rmp_serde of `Vec<ChunkTile>` — the
//!      emit's `to_vec_named` map-encoded struct), filtering to the tiles
//!      whose `resourceinstance_id` == the target (chunks are
//!      content-addressed and pack many resources' tiles up to 256/chunk);
//!   4. convert to `alizarin_core::StaticTile` and hydrate to a schema-aware
//!      JSON tree via the partial-safe `resource_tiles_to_tree`.
//!
//! ```text
//! cargo run -p ros-madair-emit --example hydrate_entry -- \
//!     <head_dir> <resource_uuid> <graph.json>
//! ```
//!
//! `<graph.json>` is the Arches resource-model export (bare graph object or
//! `{"graph":[...]}`). The head dir does not carry the schema; a Tauri
//! command would ship the graph alongside the head artifact.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::ExitCode;

use alizarin_core::graph::{StaticGraph, StaticResourceMetadata};
use alizarin_core::json_conversion::resource_tiles_to_tree;
use alizarin_core::StaticTile;
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

/// Deserialization mirror of `ros_madair_emit::chunks::ChunkTile` (which is
/// serialize-only and crate-private). Field names must match — the emit
/// writes with `rmp_serde::to_vec_named`, i.e. a msgpack map keyed by field
/// name, so missing optional fields (tileid/sortorder) default cleanly.
#[derive(Deserialize)]
struct ChunkTile {
    #[serde(default)]
    data: BTreeMap<String, serde_json::Value>,
    nodegroup_id: String,
    resourceinstance_id: String,
    #[serde(default)]
    tileid: Option<String>,
    #[serde(default)]
    parenttile_id: Option<String>,
    #[serde(default)]
    sortorder: Option<i32>,
}

impl From<ChunkTile> for StaticTile {
    fn from(c: ChunkTile) -> Self {
        StaticTile {
            data: c.data.into_iter().collect::<HashMap<_, _>>(),
            nodegroup_id: c.nodegroup_id,
            resourceinstance_id: c.resourceinstance_id,
            tileid: c.tileid,
            parenttile_id: c.parenttile_id,
            provisionaledits: None,
            sortorder: c.sortorder,
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: hydrate_entry <head_dir> <resource_uuid> <graph.json>");
        return ExitCode::from(2);
    }
    match run(&args[1], &args[2], &args[3]) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hydrate_entry failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    head_dir: &str,
    uuid: &str,
    graph_path: &str,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let head_dir = Path::new(head_dir);

    // --- Schema: load + index the graph (a Tauri command ships this) ---
    let graph_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(graph_path)?)?;
    // Accept either a bare Arches graph object or a {"graph":[...]} export.
    let graph_value = match graph_json.get("graph").and_then(|g| g.get(0)) {
        Some(v) => v.clone(),
        None => graph_json,
    };
    let mut graph: StaticGraph = serde_json::from_value(graph_value)?;
    graph.build_indices();

    // --- (a) head.sqlite, strictly read-only ---
    let conn = Connection::open_with_flags(
        head_dir.join("head.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    // Single-layer head: discover the one spine table.
    let spine_table: String = conn.query_row(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'spine_%' LIMIT 1",
        [],
        |r| r.get(0),
    )?;

    // --- (b) UUID -> dict term_id -> spine rid ---
    let rid: i64 = conn.query_row(
        &format!(
            "SELECT s.rid FROM {spine_table} s \
             JOIN dict d ON d.term_id = s.term_id WHERE d.term = ?1"
        ),
        [uuid],
        |r| r.get(0),
    )?;

    // rid -> chunk hashes (dedup: a resource's tiles for one nodegroup all
    // land in one chunk, but two nodegroups may share a chunk).
    let mut stmt = conn.prepare(
        "SELECT DISTINCT c.hash, f.tile_count FROM fragment_dir f \
         JOIN chunks c ON c.chunk = f.chunk WHERE f.rid = ?1",
    )?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([rid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<_, _>>()?;
    let expected: i64 = rows.iter().map(|(_, n)| n).sum();
    let hashes: std::collections::BTreeSet<String> =
        rows.into_iter().map(|(h, _)| h).collect();

    // --- (c) read chunks, filter to this resource's tiles ---
    let mut tiles: Vec<StaticTile> = Vec::new();
    for hash in &hashes {
        let bytes = std::fs::read(head_dir.join("chunks").join(format!("{hash}.msgpack")))?;
        let chunk: Vec<ChunkTile> = rmp_serde::from_slice(&bytes)?;
        for ct in chunk {
            if ct.resourceinstance_id == uuid {
                tiles.push(ct.into());
            }
        }
    }
    // Stable tile order for reproducible output.
    tiles.sort_by(|a, b| {
        (a.nodegroup_id.as_str(), a.tileid.as_deref())
            .cmp(&(b.nodegroup_id.as_str(), b.tileid.as_deref()))
    });

    // --- (d) hydrate to a schema-aware JSON tree (partial-safe) ---
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
    let tree = resource_tiles_to_tree(&tiles, &metadata, &graph)
        .map_err(|e| format!("hydration failed: {e}"))?;

    // --- (e) report: cross-check + the entry JSON ---
    eprintln!(
        "rid={rid} chunks={} tiles_recovered={} tiles_expected(fragment_dir)={} match={}",
        hashes.len(),
        tiles.len(),
        expected,
        tiles.len() as i64 == expected
    );
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(ExitCode::SUCCESS)
}
