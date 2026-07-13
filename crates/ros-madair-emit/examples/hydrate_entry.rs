// SPDX-License-Identifier: AGPL-3.0-or-later
//! Macbain pilot — the Tauri-NATIVE read path (no WASM), now a thin driver
//! over `ros-madair-read`.
//!
//! Everything this example used to spell out by hand (open the head read-only,
//! UUID → dict term_id → spine rid, `fragment_dir JOIN chunks`, decode
//! `chunks/<hash>.msgpack`, filter to the resource, hydrate) is library code:
//! `ros_madair_read::hydrate_resource`. A Gréasán Tauri command calls that same
//! function — it does not re-derive it from this file, which is what happened
//! when this was the only copy (down to a private re-declaration of the chunk
//! wire format, now `ros_madair_format::ChunkTile`).
//!
//! ```text
//! cargo run -p ros-madair-emit --example hydrate_entry -- \
//!     <head_dir> <resource_uuid> <graph.json>
//! ```
//!
//! `<graph.json>` is the Arches resource-model export (bare graph object or
//! `{"graph":[...]}`). The head dir does not carry the schema; a Tauri command
//! ships the graph alongside the head artifact.

use std::path::Path;
use std::process::ExitCode;

use alizarin_core::graph::StaticGraph;

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
    let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(graph_path)?)?;
    // Accept either a bare Arches graph object or a {"graph":[...]} export.
    let graph_value = raw
        .get("graph")
        .and_then(|g| g.get(0))
        .cloned()
        .unwrap_or(raw);
    let mut graph: StaticGraph = serde_json::from_value(graph_value)?;
    graph.build_indices();

    // --- The whole read path ---
    let tiles = ros_madair_read::resource_tiles_with_graph(head_dir, uuid, Some(&graph))?;
    let tree = ros_madair_read::hydrate_tiles(&tiles, uuid, &graph)?;

    // --- Report: cross-check against fragment_dir, then the entry JSON ---
    let expected = ros_madair_read::expected_tile_count(head_dir, uuid)?;
    eprintln!(
        "tiles_recovered={} tiles_expected(fragment_dir)={expected} match={}",
        tiles.len(),
        tiles.len() as i64 == expected
    );
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(ExitCode::SUCCESS)
}
