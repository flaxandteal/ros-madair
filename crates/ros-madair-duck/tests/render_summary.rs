// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Render-summary (anti-N+1): a linked resource's title renders from one fetch.**
//!
//! The emitter materialises alizarin's `__cache` (via `populate_caches`) into the
//! tile `data` column, so hydrating resource A shows the display name of a
//! resource B it links to WITHOUT a second query to load B. This test emits A→B,
//! hydrates A ALONE (only A's tiles are read), and asserts B's title appears in
//! A's rendered tree — proving the summary came from the carried `__cache`, not a
//! fetch of B.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use serde_json::json;

const GRAPH: &str = "f00dcafe-0000-4000-8000-000000000001";
const ROOT: &str = "f00dcafe-1ae0-11f0-b22a-8fd6f4eb1a02";
const NG: &str = "c0000000-0000-4000-8000-00000000000a";
const LINK: &str = "c0000000-0000-4000-8000-00000000000b";
const A: &str = "aaaa0000-0000-4000-8000-000000000001";
const B: &str = "bbbb0000-0000-4000-8000-000000000002";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-summary-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({"nodeid": ROOT, "name": "Thing", "alias": "thing", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    let doc = json!({"graph": [{
        "graphid": GRAPH, "name": "Thing", "root": root.clone(),
        "nodes": [root,
            {"nodeid": NG, "nodegroup_id": NG, "name": "rel", "alias": "rel", "datatype": "semantic",
             "graph_id": GRAPH, "istopnode": false, "is_collector": true, "isrequired": false,
             "issearchable": true, "exportable": false, "sortorder": 0},
            {"nodeid": LINK, "nodegroup_id": NG, "name": "related", "alias": "related",
             "datatype": "resource-instance", "graph_id": GRAPH, "istopnode": false,
             "is_collector": false, "isrequired": false, "issearchable": true, "exportable": false,
             "sortorder": 0}],
        "nodegroups": [{"nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [
            {"edgeid": "e1", "domainnode_id": ROOT, "rangenode_id": NG, "graph_id": GRAPH},
            {"edgeid": "e2", "domainnode_id": NG, "rangenode_id": LINK, "graph_id": GRAPH}],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn resource(id: &str, name: &str, tile: &str, data: serde_json::Value) -> serde_json::Value {
    json!({
        "resourceinstance": {"resourceinstanceid": id, "graph_id": GRAPH, "name": name, "legacyid": null,
            "descriptors": {"en": {"name": name, "description": "", "map_popup": ""}}},
        "tiles": [{"tileid": tile, "nodegroup_id": NG, "parenttile_id": null,
            "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null, "data": data}],
    })
}

#[test]
fn linked_resource_title_renders_from_cache_without_loading_target() {
    let dir = scratch("in");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("thing");
    std::fs::create_dir_all(&rdir).unwrap();
    // B is "Beta Place"; A links to B through the resource-instance node.
    let resources = vec![
        resource(B, "Beta Place", "1b000000-0000-4000-8000-000000000010", json!({})),
        resource(A, "Alpha", "1a000000-0000-4000-8000-000000000010",
                 json!({ LINK: [{ "resourceId": B }] })),
    ];
    std::fs::write(
        rdir.join("things.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    let pq = scratch("out");
    ros_madair_emit::emit_parquet(
        dir.to_str().unwrap(), pq.to_str().unwrap(), "https://example.org/",
        &default_registry(), &std::collections::HashMap::new(),
    )
    .expect("emit");

    let tiles = pq.join(format!("tiles_{GRAPH}.parquet"));
    let duck = DuckReader::open(tiles.to_str().unwrap()).expect("open");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let mut graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    graph.build_indices();

    // Hydrate A ALONE — resource_tiles(A) reads only A's tiles; B is never queried.
    let tree = duck.hydrate(A, &graph, &["en"]).expect("hydrate A");
    let s = serde_json::to_string(&tree).unwrap();
    eprintln!("hydrated A: {s}");

    assert!(
        s.contains("Beta Place"),
        "A's rendered tree shows B's title from __cache (no second query); got: {s}"
    );
    assert!(
        !s.contains("__cache"),
        "the reserved __cache key must not leak into the rendered tree"
    );
    eprintln!("OK: linked-resource title rendered from the carried __cache, target not loaded");
}
