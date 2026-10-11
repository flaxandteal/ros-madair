// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Numeric `Range` end-to-end: emit → ordered store → `BETWEEN` zone-map.**
//!
//! `number` is a stock Arches datatype that `alizarin-core` does not classify
//! (its ordered-scalar built-ins stop at `date`/`edtf`), so Rós Madair owns it
//! via `ros-madair-handlers`: a `number` node is `IndexClass::Ordered`, its
//! value melted into `ordered_<graph>.parquet` as a fixed-point i64, and
//! range-queryable. This test proves the whole chain: emit a graph with one
//! `number` node, then `Range` over it and assert the exact boundary-inclusive
//! result set — plus that the query-side bound encoding (`quantize_number` at
//! the shared scale) matches what emit stored.
//!
//! Self-contained: a minimal single-nodegroup model built inline.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::{default_registry, number_scale, quantize_number};
use ros_madair_query::{Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "b7e11d00-0000-4000-8000-000000000001";
const ROOT: &str = "b7e11d00-1ae0-11f0-b22a-8fd6f4eb1a02";
// A cardinality-1 `measurement` nodegroup carrying one `number` node (height).
const NG: &str = "cccc1111-0000-4000-8000-000000000001";
const H: &str = "dddd1111-0000-4000-8000-000000000001";

const R_SHORT: &str = "11111111-0000-4000-8000-000000000001"; // 5.25  — below range
const R_MID: &str = "22221111-0000-4000-8000-000000000001"; // 10.5  — inside
const R_TALL: &str = "33331111-0000-4000-8000-000000000001"; // 20.0  — on the upper bound
const R_HUGE: &str = "44441111-0000-4000-8000-000000000001"; // 99.99 — above range

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-num-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn edge(id: &str, from: &str, to: &str) -> serde_json::Value {
    json!({ "edgeid": id, "domainnode_id": from, "rangenode_id": to, "graph_id": GRAPH })
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({
        "nodeid": ROOT, "name": "Thing", "alias": "thing", "datatype": "semantic",
        "graph_id": GRAPH, "istopnode": true,
    });
    let doc = json!({ "graph": [{
        "graphid": GRAPH, "name": "Thing", "root": root.clone(),
        "nodes": [
            root,
            json!({
                "nodeid": NG, "nodegroup_id": NG, "name": "measurement", "alias": "measurement",
                "datatype": "semantic", "graph_id": GRAPH, "istopnode": false,
                "is_collector": true, "isrequired": false, "issearchable": true,
                "exportable": false, "sortorder": 0,
            }),
            json!({
                "nodeid": H, "nodegroup_id": NG, "name": "height", "alias": "height",
                "datatype": "number", "graph_id": GRAPH, "istopnode": false,
                "is_collector": false, "isrequired": false, "issearchable": true,
                "exportable": false, "sortorder": 0,
            }),
        ],
        "nodegroups": [
            { "nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null },
        ],
        "edges": [
            edge("e1110000-0000-4000-8000-000000000001", ROOT, NG),
            edge("e1110000-0000-4000-8000-000000000002", NG, H),
        ],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn resource(r: &str, tile: &str, height: f64) -> serde_json::Value {
    json!({
        "resourceinstance": { "resourceinstanceid": r, "graph_id": GRAPH, "name": r, "legacyid": null,
            "descriptors": { "en": { "name": r, "description": "", "map_popup": "" } } },
        "tiles": [ json!({
            "tileid": tile, "nodegroup_id": NG, "parenttile_id": null,
            "resourceinstance_id": r, "sortorder": 0, "provisionaledits": null,
            "data": { H: height },
        }) ],
    })
}

fn corpus() -> (PathBuf, StaticGraph) {
    let dir = scratch("corpus");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("thing");
    std::fs::create_dir_all(&rdir).unwrap();
    let resources = vec![
        resource(R_SHORT, "a1110000-0000-4000-8000-000000000010", 5.25),
        resource(R_MID, "a2220000-0000-4000-8000-000000000010", 10.5),
        resource(R_TALL, "a3330000-0000-4000-8000-000000000010", 20.0),
        resource(R_HUGE, "a4440000-0000-4000-8000-000000000010", 99.99),
    ];
    std::fs::write(
        rdir.join("things.json"),
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
    (pq.join(format!("tiles_{GRAPH}.parquet")), graph)
}

fn q(w: Expr) -> Query {
    Query {
        model: GRAPH.into(),
        r#where: Some(w),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

/// Build a numeric `Range` with bounds encoded at the SAME scale emit used
/// (no per-node `decimals` config here → the default scale).
fn height_between(lo: f64, hi: f64) -> Expr {
    let scale = number_scale(None);
    Expr::Range {
        path: "height".into(),
        lo: quantize_number(lo, scale).unwrap(),
        hi: quantize_number(hi, scale).unwrap(),
    }
}

#[test]
fn numeric_range_is_boundary_inclusive_and_exact() {
    let (pq, graph) = corpus();
    let registry = default_registry();
    let duck = DuckReader::open(pq.to_str().unwrap()).expect("open duck");
    let ids = |w: Expr| {
        let mut v = duck.resolve_ids(&q(w), &graph, &registry).unwrap();
        v.sort();
        v
    };
    let set = |xs: &[&str]| {
        let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };

    // [10.0, 20.0]: 10.5 (mid) and 20.0 (tall, ON the upper bound) match;
    // 5.25 and 99.99 fall outside.
    assert_eq!(
        ids(height_between(10.0, 20.0)),
        set(&[R_MID, R_TALL]),
        "range is inclusive on both ends; the on-bound value survives"
    );

    // A fractional window that excludes the boundary value: (5.25, 20.0) via
    // [5.26, 19.99] keeps only the mid.
    assert_eq!(ids(height_between(5.26, 19.99)), set(&[R_MID]));

    // A window below everything is empty, not an error.
    assert_eq!(ids(height_between(0.0, 5.0)), Vec::<String>::new());

    // The big value is reachable — fixed-point didn't overflow it.
    assert_eq!(ids(height_between(50.0, 100.0)), set(&[R_HUGE]));

    eprintln!("OK: numeric Range melts, quantizes, and compares boundary-inclusive");
}
