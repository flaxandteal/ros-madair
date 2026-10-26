// SPDX-License-Identifier: AGPL-3.0-or-later
//! Run a Query IR (JSON on stdin) against an Aonach head and print matches as JSON.
//! Resolves concept VALUES given as labels -> interned ids via the head's catalog (when
//! present). Loads each model's StaticGraph from a source graphs dir by slug.
//!   cargo run -p ros-madair-duck --example aonach-query -- <head_dir> <graphs_root> < ir.json
use std::collections::HashMap;
use std::path::Path;

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::ExtensionTypeRegistry;
use ros_madair_query::{Expr, Query};
use serde_json::json;

fn norm(s: &str) -> String { s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect() }

/// index every graph in `graphs_dir` (data_dir/graphs/*.json) by normalised model name ->
/// (file path, graph id). The compiler matches OnLink targets by graph id.
fn graph_index(graphs_dir: &Path) -> HashMap<String, (std::path::PathBuf, String)> {
    let mut m = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(graphs_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") { continue; }
            if let Ok(raw) = std::fs::read_to_string(&p) {
                if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let g = doc.get("graph").and_then(|g| g.get(0)).unwrap_or(&doc);
                    if let (Some(name), Some(gid)) = (g.get("name").and_then(|n| n.as_str()), g.get("graphid").and_then(|n| n.as_str())) {
                        m.insert(norm(name), (p.clone(), gid.to_string()));
                    }
                }
            }
        }
    }
    m
}

/// rewrite OnLink target models from a human name to the graph id (compiler matches on id).
fn rewrite_hop_models(e: &mut Expr, name2gid: &HashMap<String, String>) {
    match e {
        Expr::All(v) | Expr::Any(v) | Expr::OnTile(v) => v.iter_mut().for_each(|c| rewrite_hop_models(c, name2gid)),
        Expr::Not(b) => rewrite_hop_models(b, name2gid),
        Expr::OnLink { model, r#where, .. } | Expr::OnBacklink { model, r#where, .. } => {
            if let Some(gid) = name2gid.get(&norm(model)) { *model = gid.clone(); }
            rewrite_hop_models(r#where, name2gid);
        }
        _ => {}
    }
}

fn load_graph_at(p: &Path) -> Result<StaticGraph, String> {
    let raw = std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", p.display()))?;
    let g = doc.get("graph").and_then(|g| g.get(0)).cloned().unwrap_or(doc);
    serde_json::from_value(g).map_err(|e| format!("graph schema {}: {e}", p.display()))
}

/// collect the on_link target model slugs (for linked graphs) walking the Expr.
fn collect_models(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::All(v) | Expr::Any(v) | Expr::OnTile(v) => v.iter().for_each(|c| collect_models(c, out)),
        Expr::Not(b) => collect_models(b, out),
        Expr::OnLink { model, r#where, .. } | Expr::OnBacklink { model, r#where, .. } => { out.push(norm(model)); collect_models(r#where, out); }
        _ => {}
    }
}

/// rewrite Concept values that are human labels -> interned concept ids (label->id map).
fn resolve_concepts(e: &mut Expr, l2id: &HashMap<String, String>) {
    match e {
        Expr::All(v) | Expr::Any(v) | Expr::OnTile(v) => v.iter_mut().for_each(|c| resolve_concepts(c, l2id)),
        Expr::Not(b) => resolve_concepts(b, l2id),
        Expr::OnLink { r#where, .. } => resolve_concepts(r#where, l2id),
        Expr::Concept { value, .. } => { if let Some(id) = l2id.get(value.as_str()) { *value = id.clone(); } }
        _ => {}
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 { eprintln!("usage: aonach-query <head_dir> <graphs_root>  (IR JSON on stdin)"); std::process::exit(2); }
    let head = Path::new(&args[1]);
    let idx = graph_index(Path::new(&args[2]));

    let mut query: Query = serde_json::from_reader(std::io::stdin()).expect("parse Query IR");
    let reader = DuckReader::open_layers(&[head]).expect("open head");

    // registry from the manifest's declared handlers
    let manifest: ros_madair_format::Manifest =
        serde_json::from_slice(&std::fs::read(head.join("manifest.json")).expect("manifest")).expect("manifest parse");
    let registry = ros_madair_handlers::registry_from_declarations(&manifest.handlers).expect("registry");

    // label -> CONCEPT id, read straight from the catalog (concept_id column — what the
    // melted store + the `concept_id = ...` filter match on; concept_labels() merges in
    // value-ids too, which would mis-resolve).
    let mut l2id: HashMap<String, String> = HashMap::new();
    let cat = head.join("concept_catalog.parquet");
    if cat.exists() {
        let conn = duckdb::Connection::open_in_memory().expect("duckdb");
        let mut stmt = conn
            .prepare(&format!("SELECT label, concept_id FROM read_parquet('{}') WHERE label IS NOT NULL", cat.display().to_string().replace('\'', "''")))
            .expect("catalog query");
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).expect("rows");
        for row in rows { if let Ok((label, id)) = row { l2id.insert(label, id); } }
    }
    // This Aonach data stores concept VALUES as labels in the melted store, so the
    // Concept `Is` filter matches on the label directly — pass composer labels through,
    // don't rewrite to catalog UUIDs. (resolve_concepts is kept for value-id datasets.)
    let _ = (&l2id, resolve_concepts as fn(&mut Expr, &HashMap<String, String>));

    // collect linked model keys (by name) BEFORE rewriting on_link.model -> graph id
    let root_key = norm(&query.model);
    let mut link_keys = Vec::new();
    if let Some(w) = &query.r#where { collect_models(w, &mut link_keys); }
    link_keys.sort(); link_keys.dedup(); link_keys.retain(|s| *s != root_key);
    // the compiler matches OnLink targets by graph id -> rewrite names to ids
    let name2gid: HashMap<String, String> = idx.iter().map(|(k, (_, g))| (k.clone(), g.clone())).collect();
    if let Some(w) = query.r#where.as_mut() { rewrite_hop_models(w, &name2gid); }
    let root_graph = load_graph_at(&idx.get(&root_key).unwrap_or_else(|| panic!("no graph for model {}", query.model)).0).expect("root graph");
    let linked_graphs: Vec<StaticGraph> = link_keys.iter().filter_map(|k| idx.get(k)).filter_map(|(p, _)| load_graph_at(p).ok()).collect();
    let linked_refs: Vec<&StaticGraph> = linked_graphs.iter().collect();

    let ids = reader.resolve_ids_linked(&query, &root_graph, &linked_refs, &registry).expect("resolve");
    let names = reader.descriptors(&ids.iter().take(12).cloned().collect::<Vec<_>>()).unwrap_or_default();
    let shown: Vec<serde_json::Value> = ids.iter().take(12).map(|i| json!({"id": i, "name": names.get(i)})).collect();
    println!("{}", json!({"count": ids.len(), "concepts_resolved": l2id.len(), "results": shown}));
}
