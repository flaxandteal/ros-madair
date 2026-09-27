// SPDX-License-Identifier: AGPL-3.0-or-later
//! **The exact spatial step must test EVERY feature, not just `features[0]`.**
//!
//! emit's `extract_bbox` unions every feature of a tile's FeatureCollection into
//! the promoted zone-map bbox, so the coarse prune admits a tile if *any* feature
//! overlaps. The exact fine step used to parse only `features[0]` — so a tile
//! whose FIRST feature is far away but whose SECOND feature intersects the query
//! box was a silent false negative under a sound coarse prune.
//!
//! This builds exactly that tile: a two-feature resource with `features[0]` at
//! (100,100) and `features[1]` at the origin. A query box at the origin must match
//! it (via the second feature). Self-contained.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const GEO: &str = "5efd0000-0000-4000-8000-000000000002";

const R_MULTI: &str = "11110000-0000-4000-8000-000000000001"; // features[0] far, features[1] at origin
const R_FIRST: &str = "22220000-0000-4000-8000-000000000001"; // single feature at origin
const R_FAR: &str = "33330000-0000-4000-8000-000000000001"; // single feature far away

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-multifeat-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({
        "nodeid": ROOT, "name": "Talk", "alias": "talk", "datatype": "semantic",
        "graph_id": GRAPH, "istopnode": true,
    });
    let loc = json!({
        "nodeid": GEO, "nodegroup_id": GEO, "name": "location", "alias": "location",
        "datatype": "geojson-feature-collection", "graph_id": GRAPH, "istopnode": false,
        "is_collector": true, "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0,
    });
    let doc = json!({ "graph": [{
        "graphid": GRAPH, "name": "Talk", "root": root.clone(),
        "nodes": [root, loc],
        "nodegroups": [{ "nodegroupid": GEO, "cardinality": "1", "parentnodegroup_id": null }],
        "edges": [{ "edgeid": "e0000000-0000-4000-8000-0000000000ef",
                    "domainnode_id": ROOT, "rangenode_id": GEO, "graph_id": GRAPH }],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn point(lng: f64, lat: f64) -> serde_json::Value {
    json!({ "type": "Feature", "properties": {}, "geometry": { "type": "Point", "coordinates": [lng, lat] } })
}
fn fc(features: Vec<serde_json::Value>) -> serde_json::Value {
    json!({ "type": "FeatureCollection", "features": features })
}
fn resource(r: &str, geometry: serde_json::Value) -> serde_json::Value {
    json!({
        "resourceinstance": { "resourceinstanceid": r, "graph_id": GRAPH, "name": r, "legacyid": null,
            "descriptors": { "en": { "name": r, "description": "", "map_popup": "" } } },
        "tiles": [{
            "tileid": format!("{}0000-0000-4000-8000-0000000000aa", &r[..4]),
            "nodegroup_id": GEO, "parenttile_id": null, "resourceinstance_id": r,
            "sortorder": 0, "provisionaledits": null, "data": { GEO: geometry },
        }],
    })
}

fn corpus() -> (PathBuf, StaticGraph) {
    let dir = scratch("corpus");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&rdir).unwrap();
    let resources = vec![
        // features[0] far, features[1] at the origin — the case that exposed the bug.
        resource(R_MULTI, fc(vec![point(100.0, 100.0), point(0.0, 0.0)])),
        resource(R_FIRST, fc(vec![point(0.0, 0.0)])),
        resource(R_FAR, fc(vec![point(100.0, 100.0)])),
    ];
    std::fs::write(
        rdir.join("talks.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    let registry = default_registry();
    let pq = scratch("pq");
    ros_madair_emit::emit_parquet(
        dir.to_str().unwrap(),
        pq.to_str().unwrap(),
        "https://example.org/",
        &registry,
        &std::collections::HashMap::new(),
    )
    .expect("emit parquet");

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    (pq.join("tiles_talk.parquet"), graph)
}

#[test]
fn exact_spatial_tests_every_feature_not_just_the_first() {
    let (pq, graph) = corpus();
    let registry = default_registry();
    let duck = DuckReader::open(pq.to_str().unwrap()).expect("open duck");

    let bbox = Expr::Bbox {
        path: "location".into(),
        min_lng: -1.0,
        min_lat: -1.0,
        max_lng: 1.0,
        max_lat: 1.0,
    };
    let mut got = duck
        .resolve_ids(
            &Query {
                model: GRAPH.into(),
                r#where: Some(bbox),
                measures: vec![Measure::SelectIds],
                limit: None,
            },
            &graph,
            &registry,
        )
        .unwrap();
    got.sort();
    let mut want = vec![R_FIRST.to_string(), R_MULTI.to_string()];
    want.sort();

    // R_MULTI matches via features[1] (the old features[0]-only step dropped it);
    // R_FAR is correctly excluded (exact, so this also asserts spatial is active).
    assert_eq!(
        got, want,
        "exact step must find the origin feature at any index; R_FAR excluded"
    );
    eprintln!("OK: exact spatial tests all features (R_MULTI matched via features[1])");
}
