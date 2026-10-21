// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Grid-cell spatial prune + the per-resource entry table.**
//!
//! Proves the two emit-side additions end to end: `grid_<graphid>.parquet` (the
//! cell-sorted spatial edge) and `resources_<graphid>.parquet` (the entry table).
//! A `GridBox` around the origin returns the near resources and EXCLUDES the far
//! one (the compiler's `cells_in_box` contract agrees with emit's `grid_cell`, or
//! the `cell IN (…)` would miss). `All[GridBox, Concept]` intersects spatial with a
//! role. `resource_entries`/`render_summaries` read the entry/grid stores by id.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "f00dcafe-0000-4000-8000-0000000000a1";
const ROOT: &str = "f00dcafe-0000-4000-8000-0000000000a2";
const NG: &str = "c0000000-0000-4000-8000-0000000000b1";
const GEO: &str = "c0000000-0000-4000-8000-0000000000b2";
const ROLE: &str = "c0000000-0000-4000-8000-0000000000b3";
const NEAR_H: &str = "aaaa0000-0000-4000-8000-000000000001"; // (0,0) Hospital
const NEAR_S: &str = "aaaa0000-0000-4000-8000-000000000002"; // (0.001,0.001) Shelter
const FAR_H: &str = "aaaa0000-0000-4000-8000-000000000003"; // (100,100) Hospital
                                                            // A Hospital whose footprint STRADDLES the box edge: one corner inside the box
                                                            // at (0,0), one far outside at (0.05,0.05). Its CENTROID (0.025,0.025) is OUTSIDE
                                                            // the box — so a centroid-only grid would MISS it. bbox-to-cells must return it
                                                            // (over-inclusion is the safe failure; a responder must never miss it).
const STRADDLE_H: &str = "aaaa0000-0000-4000-8000-000000000004";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-grid-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({"nodeid": ROOT, "name": "Place", "alias": "place", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    let geo = json!({"nodeid": GEO, "nodegroup_id": NG, "name": "footprint", "alias": "footprint",
        "datatype": "geojson-feature-collection", "graph_id": GRAPH, "istopnode": false,
        "is_collector": true, "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0});
    let role = json!({"nodeid": ROLE, "nodegroup_id": NG, "name": "service_role", "alias": "service_role",
        "datatype": "concept", "graph_id": GRAPH, "istopnode": false,
        "is_collector": false, "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0});
    let doc = json!({"graph": [{
        "graphid": GRAPH, "name": "Place", "root": root.clone(),
        "nodes": [root, geo, role],
        "nodegroups": [{"nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [
            {"edgeid": "e1", "domainnode_id": ROOT, "rangenode_id": NG, "graph_id": GRAPH},
            {"edgeid": "e2", "domainnode_id": NG, "rangenode_id": GEO, "graph_id": GRAPH},
            {"edgeid": "e3", "domainnode_id": NG, "rangenode_id": ROLE, "graph_id": GRAPH}],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

// A two-point footprint (bbox spans both) — for the straddle case.
fn resource2(
    id: &str,
    name: &str,
    p0: (f64, f64),
    p1: (f64, f64),
    role: &str,
) -> serde_json::Value {
    let feat = |p: (f64, f64)| {
        json!({"type": "Feature", "properties": {},
        "geometry": {"type": "Point", "coordinates": [p.0, p.1]}})
    };
    let fc = json!({"type": "FeatureCollection", "features": [feat(p0), feat(p1)]});
    resource_fc(id, name, fc, role)
}

fn resource(id: &str, name: &str, lng: f64, lat: f64, role: &str) -> serde_json::Value {
    let fc = json!({"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [lng, lat]}}]});
    resource_fc(id, name, fc, role)
}

fn resource_fc(id: &str, name: &str, fc: serde_json::Value, role: &str) -> serde_json::Value {
    json!({
        "resourceinstance": {"resourceinstanceid": id, "graph_id": GRAPH, "name": name, "legacyid": format!("legacy-{name}"),
            "descriptors": {"en": {"name": name, "description": "", "map_popup": ""}}},
        "tiles": [{"tileid": format!("{}0000-0000-4000-8000-0000000000cc", &id[..4]),
            "nodegroup_id": NG, "parenttile_id": null, "resourceinstance_id": id,
            "sortorder": 0, "provisionaledits": null, "data": {GEO: fc, ROLE: role}}],
    })
}

fn corpus() -> (PathBuf, StaticGraph) {
    let dir = scratch("in");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("place");
    std::fs::create_dir_all(&rdir).unwrap();
    let resources = vec![
        resource(NEAR_H, "Near Hospital", 0.0, 0.0, "Hospital"),
        resource(NEAR_S, "Near Shelter", 0.001, 0.001, "Shelter"),
        resource(FAR_H, "Far Hospital", 100.0, 100.0, "Hospital"),
        resource2(
            STRADDLE_H,
            "Straddle Hospital",
            (0.0, 0.0),
            (0.05, 0.05),
            "Hospital",
        ),
    ];
    std::fs::write(
        rdir.join("places.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    let pq = scratch("out");
    ros_madair_emit::emit_parquet(
        dir.to_str().unwrap(),
        pq.to_str().unwrap(),
        "https://example.org/",
        &default_registry(),
        &std::collections::HashMap::new(),
    )
    .expect("emit");
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    (pq.join(format!("tiles_{GRAPH}.parquet")), graph)
}

fn ids(duck: &DuckReader, graph: &StaticGraph, expr: Expr) -> Vec<String> {
    let mut got = duck
        .resolve_ids(
            &Query {
                model: GRAPH.into(),
                r#where: Some(expr),
                measures: vec![Measure::SelectIds],
                limit: None,
            },
            graph,
            &default_registry(),
        )
        .unwrap();
    got.sort();
    got
}

fn grid_box() -> Expr {
    Expr::GridBox {
        path: "footprint".into(),
        min_lng: -0.01,
        min_lat: -0.01,
        max_lng: 0.01,
        max_lat: 0.01,
    }
}

#[test]
fn grid_box_prunes_to_near_resources_and_entry_table_reads() {
    let (pq, graph) = corpus();
    // The grid_*.parquet sits next to tiles — proves emit wrote it.
    let grid = pq.with_file_name(format!("grid_{GRAPH}.parquet"));
    assert!(grid.exists(), "emit must write {}", grid.display());
    let resources = pq.with_file_name(format!("resources_{GRAPH}.parquet"));
    assert!(
        resources.exists(),
        "emit must write {}",
        resources.display()
    );

    let duck = DuckReader::open(pq.to_str().unwrap()).expect("open");

    // GridBox around the origin: every resource whose FOOTPRINT reaches the box —
    // the two near ones AND the straddler (whose centroid is outside), but NOT the
    // far one. The straddler is the safety property: a centroid-only grid would drop
    // it; bbox-to-cells (a true superset) must keep it. If cells_in_box (compiler)
    // and grid_cell (emit) disagreed, the far one would leak or a near one vanish.
    assert_eq!(
        ids(&duck, &graph, grid_box()),
        {
            let mut v = vec![NEAR_H.to_string(), NEAR_S.to_string(), STRADDLE_H.to_string()];
            v.sort();
            v
        },
        "GridBox must return every footprint reaching the box (incl. the straddler), exclude the far one"
    );

    // All[GridBox, Concept(Hospital)]: spatial ∩ role → the near hospital AND the
    // straddling hospital (NEAR_S is a Shelter; FAR_H is out of box).
    let intersect = Expr::All(vec![
        grid_box(),
        Expr::Concept {
            path: "service_role".into(),
            op: ros_madair_query::ConceptOp::Is,
            value: "Hospital".into(),
        },
    ]);
    assert_eq!(
        ids(&duck, &graph, intersect),
        {
            let mut v = vec![NEAR_H.to_string(), STRADDLE_H.to_string()];
            v.sort();
            v
        },
        "spatial ∩ role must keep both in-box hospitals, incl. the straddler the centroid grid would miss"
    );

    // Entry table: the per-resource spine reads by id.
    let entries = duck
        .resource_entries(&[NEAR_H.to_string()])
        .expect("resource_entries");
    let (graph_id, legacy, descriptor) = entries.get(NEAR_H).expect("NEAR_H entry");
    assert_eq!(graph_id, GRAPH);
    assert_eq!(legacy.as_deref(), Some("legacy-Near Hospital"));
    assert_eq!(descriptor.as_deref(), Some("Near Hospital"));

    // Render summary: descriptor rides the grid edge (ward would too, if linked).
    let render = duck
        .render_summaries(&[NEAR_H.to_string()])
        .expect("render_summaries");
    let (descriptor, geo_descriptor, _summary) = render.get(NEAR_H).expect("NEAR_H render");
    assert_eq!(descriptor.as_deref(), Some("Near Hospital"));
    // geo_descriptor (v4) reaches the app THROUGH render_summaries — the accessor the
    // map uses to place a pin without hydrating the tile tree.
    let via_api = geo_descriptor
        .as_deref()
        .expect("render_summaries geo_descriptor");
    assert!(
        via_api.contains("\"type\":\"Point\"") && via_api.contains("coordinates"),
        "render_summaries must surface the geojson Point, got: {via_api}"
    );

    // And independently off the raw edge: these test resources carry no `point`
    // descriptor, so emit falls back to the footprint's bbox-centre — a valid geojson
    // Point.
    let probe = duckdb::Connection::open_in_memory().expect("duckdb");
    let gd: String = probe
        .query_row(
            &format!(
                "SELECT geo_descriptor FROM read_parquet('{}') WHERE resource_id = ? AND geo_descriptor IS NOT NULL LIMIT 1",
                grid.display()
            ),
            duckdb::params![NEAR_H],
            |r| r.get(0),
        )
        .expect("geo_descriptor for NEAR_H");
    assert!(
        gd.contains("\"type\":\"Point\"") && gd.contains("coordinates"),
        "grid edge must carry a geojson Point geo_descriptor, got: {gd}"
    );

    eprintln!(
        "OK: grid prunes spatially, intersects with concept, entry+render+geo_descriptor read"
    );
}
