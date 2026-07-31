// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Differential harness: RM coarse/fine `resolve()` vs an exact engine.**
//!
//! Question under test: is RM's coarse-prune-then-(supposed)-fine-verify path
//! *genuinely equivalent* to what a real query engine (DuckDB over Parquet, with
//! its spatial extension) returns for the same data and the same predicates?
//!
//! This is NOT a pass/fail assertion test. It builds ONE corpus (four resources,
//! each carrying a geometry AND a date), emits a real RM snapshot, runs
//! `Layers::resolve` for a battery of queries, and writes two files to
//! `$RM_DIFF_OUT`:
//!   - `rm_results.json`  — {query_name: [resource ids RM returned]}
//!   - `source_rows.csv`  — id,date_iso,geom_wkt (the ground-truth rows)
//! A companion Python script loads the SAME rows into DuckDB, runs the exact
//! equivalents (`ST_Intersects`, `BETWEEN`), and the two id-sets are diffed.
//!
//! Run: RM_DIFF_OUT=/path cargo test -p ros-madair-read --test duckdb_equiv -- --nocapture

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use alizarin_core::quantize::quantize_date;
use ros_madair_query::{Expr, Measure, Query};
use ros_madair_read::Layers;
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const GEO_NG: &str = "5efd0000-0000-4000-8000-000000000002";
const FOUNDED_NG: &str = "5efd0000-0000-4000-8000-000000000001";

// Four resources. Each has a stable short id so the diff is readable.
const R_POINT: &str = "11110000-0000-4000-8000-000000000001"; // Point(0,0), in box
const R_LSHAPE: &str = "22220000-0000-4000-8000-000000000001"; // polygon, truly intersects box, centroid outside
const R_FAR: &str = "33330000-0000-4000-8000-000000000001"; // Point(100,100), far
const R_DIAG: &str = "44440000-0000-4000-8000-000000000001"; // line, bbox overlaps box but shape misses it

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()),
    );
    dir.join("graphs").is_dir().then_some(dir)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-diff-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn add_geo_node(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": GEO_NG, "nodegroup_id": GEO_NG, "name": "Location", "alias": "location",
        "datatype": "geojson-feature-collection", "graph_id": TALK_GRAPH,
        "istopnode": false, "is_collector": true, "isrequired": false,
        "issearchable": true, "exportable": false, "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": GEO_NG, "cardinality": "1", "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": "5efd0000-0000-4000-8000-0000000000ef",
        "domainnode_id": TALK_ROOT, "rangenode_id": GEO_NG, "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn add_date_node(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": FOUNDED_NG, "nodegroup_id": FOUNDED_NG, "name": "Founded", "alias": "founded",
        "datatype": "date", "graph_id": TALK_GRAPH,
        "istopnode": false, "is_collector": true, "isrequired": false,
        "issearchable": true, "exportable": false, "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": FOUNDED_NG, "cardinality": "1", "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": "5efd0000-0000-4000-8000-0000000000ee",
        "domainnode_id": TALK_ROOT, "rangenode_id": FOUNDED_NG, "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

/// A Talk carrying a geometry tile AND a date tile.
fn talk(id: &str, geo_tile: &str, date_tile: &str, geometry: serde_json::Value, founded: &str) -> serde_json::Value {
    let fc = json!({
        "type": "FeatureCollection",
        "features": [{ "type": "Feature", "properties": {}, "geometry": geometry }],
    });
    json!({
        "resourceinstance": {
            "resourceinstanceid": id, "graph_id": TALK_GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } },
        },
        "tiles": [
            {
                "tileid": geo_tile, "nodegroup_id": GEO_NG, "parenttile_id": null,
                "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
                "data": { GEO_NG: fc },
            },
            {
                "tileid": date_tile, "nodegroup_id": FOUNDED_NG, "parenttile_id": null,
                "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
                "data": { FOUNDED_NG: founded },
            },
        ],
    })
}

// (id, geometry, wkt, founded-date) — the single source of truth for BOTH engines.
fn rows() -> Vec<(&'static str, serde_json::Value, &'static str, &'static str)> {
    vec![
        (
            R_POINT,
            json!({ "type": "Point", "coordinates": [0.0, 0.0] }),
            "POINT (0 0)",
            "2005-06-01",
        ),
        (
            R_LSHAPE,
            json!({ "type": "Polygon", "coordinates": [[
                [0.5, 0.5], [10.0, 0.5], [10.0, 2.0],
                [2.0, 2.0], [2.0, 10.0], [0.5, 10.0], [0.5, 0.5]
            ]] }),
            "POLYGON ((0.5 0.5, 10 0.5, 10 2, 2 2, 2 10, 0.5 10, 0.5 0.5))",
            "2018-03-15",
        ),
        (
            R_FAR,
            json!({ "type": "Point", "coordinates": [100.0, 100.0] }),
            "POINT (100 100)",
            "1850-01-01",
        ),
        (
            R_DIAG,
            json!({ "type": "LineString", "coordinates": [[0.5, 5.0], [5.0, 0.5]] }),
            "LINESTRING (0.5 5, 5 0.5)",
            "2021-07-01",
        ),
    ]
}

fn corpus() -> Option<(PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let dir = scratch("corpus");
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    let gp = dir.join("graphs").join(format!("{TALK_GRAPH}.json"));
    add_geo_node(&gp);
    add_date_node(&gp);
    let talk_dir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&talk_dir).unwrap();

    let resources: Vec<serde_json::Value> = rows()
        .into_iter()
        .enumerate()
        .map(|(i, (id, geom, _wkt, date))| {
            let geo_tile = format!("a{i}aa0000-0000-4000-8000-000000000010");
            let date_tile = format!("b{i}bb0000-0000-4000-8000-000000000010");
            talk(id, &geo_tile, &date_tile, geom, date)
        })
        .collect();

    std::fs::write(
        talk_dir.join("talks.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    let out = scratch("head");
    ros_madair_emit::emit(
        dir.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
    )
    .expect("emit");

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    Some((out, graph))
}

fn bbox(min_lng: f64, min_lat: f64, max_lng: f64, max_lat: f64) -> Expr {
    Expr::Bbox { path: "location".to_string(), min_lng, min_lat, max_lng, max_lat }
}
fn date_range(lo: &str, hi: &str) -> Expr {
    Expr::Range { path: "founded".to_string(), lo: quantize_date(lo).unwrap(), hi: quantize_date(hi).unwrap() }
}
fn q(where_: Expr) -> Query {
    Query { model: TALK_GRAPH.to_string(), r#where: Some(where_), measures: vec![Measure::SelectIds], limit: None }
}

#[test]
fn dump_rm_results_for_duckdb_diff() {
    let Some((out, graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let out_dir = match std::env::var("RM_DIFF_OUT") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            eprintln!("RM_DIFF_OUT unset — skipping dump");
            return;
        }
    };
    std::fs::create_dir_all(&out_dir).unwrap();

    let layers = Layers::open(&[out.as_path()]).unwrap();
    let ids = |where_: Expr| {
        let mut v = layers.resolve(&q(where_), &graph, None).unwrap();
        v.sort();
        v
    };

    // The battery. Names are shared with the DuckDB script.
    let mut results = serde_json::Map::new();
    results.insert(
        "spatial_box_unit".to_string(),
        json!(ids(bbox(-1.0, -1.0, 1.0, 1.0))),
    );
    results.insert(
        "range_2000s".to_string(),
        json!(ids(date_range("2000-01-01", "2010-12-31"))),
    );
    results.insert(
        "range_all".to_string(),
        json!(ids(date_range("1000-01-01", "3000-01-01"))),
    );
    // Compound: does a coarse spatial false positive contaminate an AND?
    results.insert(
        "spatial_box_AND_date_ge_2010".to_string(),
        json!(ids(Expr::All(vec![
            bbox(-1.0, -1.0, 1.0, 1.0),
            date_range("2010-01-01", "3000-01-01"),
        ]))),
    );

    std::fs::write(
        out_dir.join("rm_results.json"),
        serde_json::to_vec_pretty(&serde_json::Value::Object(results)).unwrap(),
    )
    .unwrap();

    // Ground-truth rows for the other engine (single source of truth).
    let mut csv = String::from("id,date_iso,geom_wkt\n");
    for (id, _geom, wkt, date) in rows() {
        csv.push_str(&format!("{id},{date},\"{wkt}\"\n"));
    }
    std::fs::write(out_dir.join("source_rows.csv"), csv).unwrap();

    eprintln!("wrote rm_results.json and source_rows.csv to {}", out_dir.display());
}
