// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Multi-valued concept promotion: EVERY concept in a tile is queryable.**
//!
//! emit promotes a tile's concepts as a JSON array (`concept_ids`), not a single
//! scalar, so a `Concept` filter is membership (`json_contains`) and matches ANY of
//! them. The pre-fix writer kept only the FIRST value, silently under-matching a
//! concept-list (or multi-concept) tile. This builds a tile with TWO reference
//! values and asserts BOTH resolve — the second would have been dropped before.
//! Self-contained: a minimal Talk model with one `reference`-list node.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{ConceptOp, Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const FABRIC: &str = "fab00000-0000-4000-8000-000000000001";

const R_BOTH: &str = "11110000-0000-4000-8000-000000000001"; // fabric = [brick, good]
const R_STONE: &str = "22220000-0000-4000-8000-000000000001"; // fabric = [stone]
const BRICK: &str = "b0000000-0000-4000-8000-000000000001";
const GOOD: &str = "60000000-0000-4000-8000-000000000001";
const STONE: &str = "50000000-0000-4000-8000-000000000001";

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-multival-{tag}-{}-{n}", std::process::id()));
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
    // A `reference` (CLM) list node — indexed ConceptHierarchical, so a tile can
    // carry several values in one node, all promoted to `concept_ids`.
    let fabric = json!({
        "nodeid": FABRIC, "nodegroup_id": FABRIC, "name": "fabric", "alias": "fabric",
        "datatype": "reference", "graph_id": GRAPH, "istopnode": false, "is_collector": true,
        "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0,
    });
    let doc = json!({ "graph": [{
        "graphid": GRAPH, "name": "Talk", "root": root.clone(),
        "nodes": [root, fabric],
        "nodegroups": [{ "nodegroupid": FABRIC, "cardinality": "1", "parentnodegroup_id": null }],
        "edges": [{ "edgeid": "e0000000-0000-4000-8000-0000000000fa",
                    "domainnode_id": ROOT, "rangenode_id": FABRIC, "graph_id": GRAPH }],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn resource(r: &str, values: &[&str]) -> serde_json::Value {
    json!({
        "resourceinstance": { "resourceinstanceid": r, "graph_id": GRAPH, "name": r, "legacyid": null,
            "descriptors": { "en": { "name": r, "description": "", "map_popup": "" } } },
        "tiles": [{
            "tileid": format!("{}0000-0000-4000-8000-0000000000aa", &r[..4]),
            "nodegroup_id": FABRIC, "parenttile_id": null, "resourceinstance_id": r,
            "sortorder": 0, "provisionaledits": null, "data": { FABRIC: values },
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
        resource(R_BOTH, &[BRICK, GOOD]),
        resource(R_STONE, &[STONE]),
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

fn is_concept(v: &str) -> Query {
    Query {
        model: GRAPH.into(),
        r#where: Some(Expr::Concept {
            path: "fabric".into(),
            op: ConceptOp::Is,
            value: v.into(),
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

#[test]
fn every_concept_in_a_tile_is_queryable_not_just_the_first() {
    let (pq, graph) = corpus();
    let registry = default_registry();
    let duck = DuckReader::open(pq.to_str().unwrap()).expect("open duck");
    let ids = |v: &str| {
        let mut r = duck.resolve_ids(&is_concept(v), &graph, &registry).unwrap();
        r.sort();
        r
    };
    let set = |xs: &[&str]| {
        let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };

    // Both values of R_BOTH's tile resolve. The pre-fix scalar promotion kept only
    // the first, so exactly one of these two would have silently missed.
    assert_eq!(ids(BRICK), set(&[R_BOTH]), "first value is queryable");
    assert_eq!(
        ids(GOOD),
        set(&[R_BOTH]),
        "the SECOND value is queryable — the multiplicity fix (was dropped before)"
    );
    // And the single-value control is unaffected.
    assert_eq!(ids(STONE), set(&[R_STONE]));
    // A value in no tile matches nothing.
    assert!(ids("00000000-0000-4000-8000-000000000000").is_empty());

    eprintln!("OK: every concept value in a tile is queryable, not just the first");
}
