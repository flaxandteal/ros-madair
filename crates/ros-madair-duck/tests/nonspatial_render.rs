// SPDX-License-Identifier: AGPL-3.0-or-later
//! **render_fields work for a NON-spatial corpus (a dictionary).**
//!
//! Regression guard for the geo-gating bug (HANDOFF-rosmadair-nonspatial-render-row.md):
//! previously `render_summary`/`render_fields` were written ONLY inside the geo branch, so
//! `render_summaries()` returned an empty map for any resource without geometry — i.e. the whole
//! "a dictionary sets ['part_of_speech','dialect']" promise was dead. The fix carries the render
//! row on the per-resource ENTRY table (ungated). This test emits a dictionary-shaped corpus with
//! NO geometry, and asserts `render_summaries` returns the configured fields with a null
//! geo_descriptor — and that no grid file was written (the entry table is the source).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ros_madair_duck::DuckReader;
use ros_madair_emit::{default_registry, emit_parquet, ClusterConfig, ClusterDim};
use serde_json::json;

const GRAPH: &str = "d1c70000-0000-4000-8000-000000000001";
const ROOT: &str = "d1c70000-1ae0-11f0-b22a-8fd6f4eb1a02";
const NG: &str = "d1c70000-0000-4000-8000-00000000000a";
const POS: &str = "d1c70000-0000-4000-8000-00000000000b"; // part_of_speech node
const DIA: &str = "d1c70000-0000-4000-8000-00000000000c"; // dialect node
const CAT: &str = "ca700000-0000-4000-8000-000000000001"; // the headword "cat"

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-nonspatial-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    // A WORD model: a semantic root + a nodegroup with two string nodes. NO geometry node.
    let root = json!({"nodeid": ROOT, "name": "Word", "alias": "word", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    let node = |id: &str, alias: &str| json!({
        "nodeid": id, "nodegroup_id": NG, "name": alias, "alias": alias, "datatype": "string",
        "graph_id": GRAPH, "istopnode": false, "is_collector": id == POS, "isrequired": false,
        "issearchable": true, "exportable": false, "sortorder": 0});
    let doc = json!({"graph": [{
        "graphid": GRAPH, "name": "Word", "root": root.clone(),
        "nodes": [root, node(POS, "part_of_speech"), node(DIA, "dialect")],
        "nodegroups": [{"nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [
            {"edgeid": "e1", "domainnode_id": ROOT, "rangenode_id": POS, "graph_id": GRAPH},
            {"edgeid": "e2", "domainnode_id": POS, "rangenode_id": DIA, "graph_id": GRAPH}],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

#[test]
fn render_fields_resolve_for_a_non_geo_resource() {
    let dir = scratch("in");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("word");
    std::fs::create_dir_all(&rdir).unwrap();
    // "cat" — a noun in the Munster dialect. No geometry anywhere.
    let resources = vec![json!({
        "resourceinstance": {"resourceinstanceid": CAT, "graph_id": GRAPH, "name": "cat", "legacyid": null,
            "descriptors": {"en": {"name": "cat", "description": "a small domestic feline", "map_popup": ""}}},
        "tiles": [{"tileid": "ca700000-0000-4000-8000-000000000010", "nodegroup_id": NG,
            "parenttile_id": null, "resourceinstance_id": CAT, "sortorder": 0, "provisionaledits": null,
            "data": { POS: "noun", DIA: "Munster" }}],
    })];
    std::fs::write(
        rdir.join("words.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    // Domain config: a dictionary sets its own render fields — the whole point of the feature.
    let mut cfg = HashMap::new();
    cfg.insert(
        GRAPH.to_string(),
        ClusterConfig {
            dimensions: vec![ClusterDim::Descriptor], // NO Geo dimension
            row_group_size: 8192,
            partition_by_nodegroup: false,
            nodegroup_hierarchical_order: false,
            render_fields: vec!["part_of_speech".into(), "dialect".into()],
        },
    );

    let pq = scratch("out");
    emit_parquet(
        dir.to_str().unwrap(),
        pq.to_str().unwrap(),
        "https://example.org/",
        &default_registry(),
        &cfg,
    )
    .expect("emit");

    // The grid edge carries no rows for a non-geo resource (an empty file may still be written),
    // so render_summaries MUST be served by the per-resource entry table.
    assert!(
        pq.join(format!("resources_{GRAPH}.parquet")).exists(),
        "the entry table is written for every resource"
    );

    let tiles = pq.join(format!("tiles_{GRAPH}.parquet"));
    let duck = DuckReader::open(tiles.to_str().unwrap()).expect("open");

    let got = duck.render_summaries(&[CAT.to_string()]).expect("render_summaries");
    let (descriptor, geo, _summary, fields) = got
        .get(CAT)
        .expect("render_summaries returns the non-geo resource (was empty before the fix)");

    assert_eq!(descriptor.as_deref(), Some("cat"), "descriptor carried");
    assert!(geo.is_none(), "geo_descriptor is null for a non-geo resource, got {geo:?}");
    let fields = fields.as_deref().expect("render_fields present for a non-geo resource");
    assert!(
        fields.contains("part_of_speech") && fields.contains("noun"),
        "part_of_speech=noun carried in render_fields: {fields}"
    );
    assert!(
        fields.contains("dialect") && fields.contains("Munster"),
        "dialect=Munster carried in render_fields: {fields}"
    );
    eprintln!("OK: non-geo render_fields = {fields} (from the entry table, no grid)");
}
