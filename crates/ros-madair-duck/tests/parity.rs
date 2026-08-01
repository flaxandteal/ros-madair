// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Slice 2 parity: the DuckDB read path answers the query battery correctly,
//! and its spatial is EXACT where the head's `resolve()` returned a superset.**
//!
//! Same synthetic Talk corpus as the read-side differential (four resources, each
//! with a geometry + a date). Emits both artifacts from one data_dir — the
//! SQLite head (for `Layers::resolve`) and the tile-row Parquet (for the DuckDB
//! path) — and runs the same `Query` IR through both. Asserts:
//!   - range/concept agree exactly between the two paths;
//!   - the DuckDB path's spatial DROPS the bbox false positive (the diagonal)
//!     that `resolve()` keeps — the exact fine step, finally done.
//! Demo fixture outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it, absent → skip.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use alizarin_core::quantize::quantize_date;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{Expr, Measure, Query};
use ros_madair_read::Layers;
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const GEO_NG: &str = "5efd0000-0000-4000-8000-000000000002";
const FOUNDED_NG: &str = "5efd0000-0000-4000-8000-000000000001";
// A CLM `reference` node — indexed via the clm handler as ConceptHierarchical,
// so it must promote to `concept_id` and answer a Concept predicate exactly like
// a `concept` node does. Two topics split the four resources.
const TOPIC_NG: &str = "5efd0000-0000-4000-8000-000000000003";
const TOPIC_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const TOPIC_B: &str = "bbbbbbbb-0000-4000-8000-000000000001";
// A resource-instance-list link node. POINT + LSHAPE link to FAR; FAR + DIAG
// have no link. Exact HasLink here (the head's chunk_link_summary is coarse).
const LINK_NG: &str = "5efd0000-0000-4000-8000-000000000004";

const R_POINT: &str = "11110000-0000-4000-8000-000000000001";
const R_LSHAPE: &str = "22220000-0000-4000-8000-000000000001";
const R_FAR: &str = "33330000-0000-4000-8000-000000000001";
const R_DIAG: &str = "44440000-0000-4000-8000-000000000001";

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()));
    dir.join("graphs").is_dir().then_some(dir)
}
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() { copy_dir(&e.path(), &to); } else { std::fs::copy(e.path(), to).unwrap(); }
    }
}
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-duck-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn add_node(graph_path: &Path, nodeid: &str, alias: &str, datatype: &str, edge: &str) {
    let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": nodeid, "nodegroup_id": nodeid, "name": alias, "alias": alias,
        "datatype": datatype, "graph_id": TALK_GRAPH, "istopnode": false, "is_collector": true,
        "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({ "nodegroupid": nodeid, "cardinality": "1", "parentnodegroup_id": null }));
    g["edges"].as_array_mut().unwrap().push(json!({ "edgeid": edge, "domainnode_id": TALK_ROOT, "rangenode_id": nodeid, "graph_id": TALK_GRAPH }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}
fn talk(id: &str, tag: &str, geometry: serde_json::Value, founded: &str, topic: &str, link_to: Option<&str>) -> serde_json::Value {
    let fc = json!({ "type": "FeatureCollection", "features": [{ "type": "Feature", "properties": {}, "geometry": geometry }] });
    // A `reference` tile stores a bare array of the list-item UUID; a link tile
    // stores `[{"resourceId": target}]` (the shape link_keys parses).
    let mut tiles = vec![
        json!({ "tileid": format!("a{tag}0000-0000-4000-8000-000000000010"), "nodegroup_id": GEO_NG, "parenttile_id": null, "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null, "data": { GEO_NG: fc } }),
        json!({ "tileid": format!("b{tag}0000-0000-4000-8000-000000000010"), "nodegroup_id": FOUNDED_NG, "parenttile_id": null, "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null, "data": { FOUNDED_NG: founded } }),
        json!({ "tileid": format!("c{tag}0000-0000-4000-8000-000000000010"), "nodegroup_id": TOPIC_NG, "parenttile_id": null, "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null, "data": { TOPIC_NG: [topic] } }),
    ];
    if let Some(t) = link_to {
        tiles.push(json!({ "tileid": format!("d{tag}0000-0000-4000-8000-000000000010"), "nodegroup_id": LINK_NG, "parenttile_id": null, "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null, "data": { LINK_NG: [{ "resourceId": t }] } }));
    }
    json!({
        "resourceinstance": { "resourceinstanceid": id, "graph_id": TALK_GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } } },
        "tiles": tiles,
    })
}

/// Emit head + parquet from one corpus; return (head_dir, parquet_file, graph).
fn corpus() -> Option<(PathBuf, PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let dir = scratch("corpus");
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    let gp = dir.join("graphs").join(format!("{TALK_GRAPH}.json"));
    add_node(&gp, GEO_NG, "location", "geojson-feature-collection", "5efd0000-0000-4000-8000-0000000000ef");
    add_node(&gp, FOUNDED_NG, "founded", "date", "5efd0000-0000-4000-8000-0000000000ee");
    add_node(&gp, TOPIC_NG, "topic", "reference", "5efd0000-0000-4000-8000-0000000000ed");
    add_node(&gp, LINK_NG, "related", "resource-instance-list", "5efd0000-0000-4000-8000-0000000000ec");
    let talk_dir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&talk_dir).unwrap();
    let resources = vec![
        // POINT + LSHAPE share TOPIC_A and link to FAR; FAR + DIAG share TOPIC_B, no link.
        talk(R_POINT, "0", json!({ "type": "Point", "coordinates": [0.0, 0.0] }), "2005-06-01", TOPIC_A, Some(R_FAR)),
        talk(R_LSHAPE, "1", json!({ "type": "Polygon", "coordinates": [[[0.5,0.5],[10.0,0.5],[10.0,2.0],[2.0,2.0],[2.0,10.0],[0.5,10.0],[0.5,0.5]]] }), "2018-03-15", TOPIC_A, Some(R_FAR)),
        talk(R_FAR, "2", json!({ "type": "Point", "coordinates": [100.0, 100.0] }), "1850-01-01", TOPIC_B, None),
        talk(R_DIAG, "3", json!({ "type": "LineString", "coordinates": [[0.5,5.0],[5.0,0.5]] }), "2021-07-01", TOPIC_B, None),
    ];
    std::fs::write(talk_dir.join("talks.json"), serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap()).unwrap();

    let registry = default_registry();
    let head_out = scratch("head");
    ros_madair_emit::emit(dir.to_str().unwrap(), head_out.to_str().unwrap(), "https://example.org/").expect("emit head");
    let pq_out = scratch("pq");
    ros_madair_emit::emit_parquet(dir.to_str().unwrap(), pq_out.to_str().unwrap(), "https://example.org/", &registry, &std::collections::HashMap::new()).expect("emit parquet");

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    Some((head_out, pq_out.join("tiles_talk.parquet"), graph))
}

fn bbox(min_lng: f64, min_lat: f64, max_lng: f64, max_lat: f64) -> Expr {
    Expr::Bbox { path: "location".into(), min_lng, min_lat, max_lng, max_lat }
}
fn date_range(lo: &str, hi: &str) -> Expr {
    Expr::Range { path: "founded".into(), lo: quantize_date(lo).unwrap(), hi: quantize_date(hi).unwrap() }
}
fn q(w: Expr) -> Query {
    Query { model: TALK_GRAPH.into(), r#where: Some(w), measures: vec![Measure::SelectIds], limit: None }
}
fn topic_is(value: &str) -> Expr {
    Expr::Concept { path: "topic".into(), op: ros_madair_query::ConceptOp::Is, value: value.into() }
}
fn links_to(target: Option<&str>) -> Expr {
    Expr::HasLink { path: "related".into(), target: target.map(String::from) }
}

#[test]
fn duckdb_path_matches_the_battery_and_is_exact_on_spatial() {
    let Some((head, pq, graph)) = corpus() else { eprintln!("demo fixture absent — skipping"); return; };
    let registry = default_registry();
    let duck = DuckReader::open(pq.to_str().unwrap()).expect("open duck");
    let ids = |w: Expr| { let mut v = duck.resolve_ids(&q(w), &graph, &registry).unwrap(); v.sort(); v };
    let set = |xs: &[&str]| { let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect(); v.sort(); v };

    // Range — exact, and equals what the head path gives.
    assert_eq!(ids(date_range("2000-01-01", "2010-12-31")), set(&[R_POINT]));
    assert_eq!(ids(date_range("1000-01-01", "3000-01-01")), set(&[R_POINT, R_LSHAPE, R_FAR, R_DIAG]));

    // Spatial — EXACT: the diagonal's bbox overlaps the query box but its shape
    // does not, so the DuckDB fine step drops it.
    assert_eq!(
        ids(bbox(-1.0, -1.0, 1.0, 1.0)),
        set(&[R_POINT, R_LSHAPE]),
        "exact ST_Intersects excludes the diagonal false positive"
    );

    // Compound — the false positive does NOT contaminate an AND (it did in resolve()).
    assert_eq!(
        ids(Expr::All(vec![bbox(-1.0, -1.0, 1.0, 1.0), date_range("2010-01-01", "3000-01-01")])),
        set(&[R_LSHAPE]),
    );

    // CLM `reference` rides the SAME mechanism as `concept`: indexed as
    // ConceptHierarchical → promoted to `concept_id` → answered by a Concept
    // predicate. A reference query resolves exactly, no special-casing.
    assert_eq!(ids(topic_is(TOPIC_A)), set(&[R_POINT, R_LSHAPE]), "reference Concept-Is resolves like a concept");
    assert_eq!(ids(topic_is(TOPIC_B)), set(&[R_FAR, R_DIAG]));
    // And it composes: reference AND date.
    assert_eq!(
        ids(Expr::All(vec![topic_is(TOPIC_A), date_range("2010-01-01", "3000-01-01")])),
        set(&[R_LSHAPE]),
    );

    let layers = Layers::open(&[head.as_path()]).unwrap();
    // Pass the SAME registry the emit used — the head-SQL compiler needs the clm
    // handler to classify `reference` (a non-core datatype), exactly as the duck
    // path does. `None` here would reject reference as NotHeadIndexed — the
    // registry-parity discipline, made loud.
    let head_ids = |w: Expr| { let mut v = layers.resolve(&q(w), &graph, Some(&registry)).unwrap(); v.sort(); v };

    // The reference query agrees with the head path (both exact for concepts).
    assert_eq!(ids(topic_is(TOPIC_A)), head_ids(topic_is(TOPIC_A)), "reference: duck == head");

    // Links are EXACT here (the tile carries its real targets), unlike the head's
    // coarse chunk_link_summary. POINT + LSHAPE link to FAR; FAR + DIAG don't.
    assert_eq!(ids(links_to(Some(R_FAR))), set(&[R_POINT, R_LSHAPE]), "HasLink to FAR is exact");
    assert_eq!(ids(links_to(None)), set(&[R_POINT, R_LSHAPE]), "HasLink (any) = the linkers");
    assert_eq!(ids(links_to(Some(R_POINT))), Vec::<String>::new(), "nobody links to POINT");
    // Composes: links to FAR AND date >= 2010 → LSHAPE (2018), not POINT (2005).
    assert_eq!(
        ids(Expr::All(vec![links_to(Some(R_FAR)), date_range("2010-01-01", "3000-01-01")])),
        set(&[R_LSHAPE]),
    );

    // The headline, in one assertion: the head path returns the diagonal; the
    // DuckDB path does not. Same corpus, same query, exact vs superset.
    let head_spatial = head_ids(bbox(-1.0, -1.0, 1.0, 1.0));
    assert!(head_spatial.contains(&R_DIAG.to_string()), "head resolve() keeps the bbox superset (the diagonal)");
    assert!(!ids(bbox(-1.0, -1.0, 1.0, 1.0)).contains(&R_DIAG.to_string()), "duck path is exact — drops it");

    eprintln!("OK: battery matches; spatial + link exact; reference == concept mechanism");
}
