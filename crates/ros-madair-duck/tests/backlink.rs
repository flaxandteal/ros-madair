// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Untyped backlinks (`Expr::Backlink`) over the reverse edge index.**
//!
//! "What links to resource T, through ANY relationship?" — the complement of a
//! typed `OnLink` hop. r1 points at T through one predicate (L1), r2 through a
//! DIFFERENT predicate (L2); an untyped backlink must find BOTH without naming a
//! node. It lowers to a semijoin over the reverse index (`edges_rev`, sorted
//! target-first), so the target-anchored lookup prunes. Being EXACT, it also
//! negates soundly. Self-contained: a minimal two-predicate graph emitted via the
//! real engine (so the redges_ sibling is written and composed).

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "bac11111-0000-4000-8000-000000000001";
const ROOT: &str = "bac11111-1ae0-11f0-b22a-8fd6f4eb1a02";
const NG1: &str = "11110000-0000-4000-8000-00000000000a";
const L1: &str = "11110000-0000-4000-8000-00000000000b";
const NG2: &str = "22220000-0000-4000-8000-00000000000a";
const L2: &str = "22220000-0000-4000-8000-00000000000b";

const R1: &str = "aaaa0000-0000-4000-8000-000000000001";
const R2: &str = "bbbb0000-0000-4000-8000-000000000002";
const R3: &str = "cccc0000-0000-4000-8000-000000000003";
const T: &str = "ffff0000-0000-4000-8000-0000000000ff"; // the link target
const OTHER: &str = "eeee0000-0000-4000-8000-0000000000ee";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-backlink-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn node(id: &str, ng: &str, alias: &str, dt: &str, collector: bool) -> serde_json::Value {
    json!({"nodeid": id, "nodegroup_id": ng, "name": alias, "alias": alias, "datatype": dt,
           "graph_id": GRAPH, "istopnode": false, "is_collector": collector, "isrequired": false,
           "issearchable": true, "exportable": false, "sortorder": 0})
}
fn edge(id: &str, f: &str, t: &str) -> serde_json::Value {
    json!({"edgeid": id, "domainnode_id": f, "rangenode_id": t, "graph_id": GRAPH})
}
fn link_tile(tile: &str, r: &str, ng: &str, node_id: &str, tgt: &str) -> serde_json::Value {
    json!({"tileid": tile, "nodegroup_id": ng, "parenttile_id": null, "resourceinstance_id": r,
           "sortorder": 0, "provisionaledits": null, "data": {node_id: [{"resourceId": tgt}]}})
}
fn resource(r: &str, tiles: Vec<serde_json::Value>) -> serde_json::Value {
    json!({"resourceinstance": {"resourceinstanceid": r, "graph_id": GRAPH, "name": r, "legacyid": null,
            "descriptors": {"en": {"name": r, "description": "", "map_popup": ""}}}, "tiles": tiles})
}

fn corpus() -> (PathBuf, StaticGraph) {
    let dir = scratch("in");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({"nodeid": ROOT, "name": "Thing", "alias": "thing", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    std::fs::write(&gp, serde_json::to_vec(&json!({"graph": [{
        "graphid": GRAPH, "name": "Thing", "root": root.clone(),
        "nodes": [root,
            node(NG1, NG1, "cites", "semantic", true),
            node(L1, NG1, "cites_ref", "resource-instance-list", false),
            node(NG2, NG2, "mentions", "semantic", true),
            node(L2, NG2, "mentions_ref", "resource-instance-list", false)],
        "nodegroups": [{"nodegroupid": NG1, "cardinality": "n", "parentnodegroup_id": null},
                       {"nodegroupid": NG2, "cardinality": "n", "parentnodegroup_id": null}],
        "edges": [edge("e1", ROOT, NG1), edge("e2", NG1, L1), edge("e3", ROOT, NG2), edge("e4", NG2, L2)],
    }]})).unwrap()).unwrap();

    let rdir = dir.join("resources").join("thing");
    std::fs::create_dir_all(&rdir).unwrap();
    let resources = vec![
        // R1 → T via L1 (cites); R2 → T via L2 (mentions) — different predicates.
        resource(
            R1,
            vec![link_tile(
                "1a110000-0000-4000-8000-000000000010",
                R1,
                NG1,
                L1,
                T,
            )],
        ),
        resource(
            R2,
            vec![link_tile(
                "2b220000-0000-4000-8000-000000000010",
                R2,
                NG2,
                L2,
                T,
            )],
        ),
        // R3 links to something else — must NOT be a backlink of T.
        resource(
            R3,
            vec![link_tile(
                "3c330000-0000-4000-8000-000000000010",
                R3,
                NG1,
                L1,
                OTHER,
            )],
        ),
    ];
    std::fs::write(
        rdir.join("things.json"),
        serde_json::to_vec(&json!({"business_data": {"resources": resources}})).unwrap(),
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

fn q(w: Expr) -> Query {
    Query {
        model: GRAPH.into(),
        r#where: Some(w),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

#[test]
fn backlink_finds_all_referrers_across_predicates() {
    let (pq, graph) = corpus();
    let registry = default_registry();
    let duck = DuckReader::open(pq.to_str().unwrap()).expect("open");
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

    // Untyped: both R1 (via cites) and R2 (via mentions) link to T — found without
    // naming either predicate.
    assert_eq!(
        ids(Expr::Backlink { target: T.into() }),
        set(&[R1, R2]),
        "backlink finds referrers through ANY predicate"
    );

    // A target nobody links to → empty (not an error).
    assert_eq!(
        ids(Expr::Backlink {
            target: "no-such-target".into()
        }),
        Vec::<String>::new()
    );

    // EXACT → negatable: resources that do NOT link to T = just R3.
    assert_eq!(
        ids(Expr::Not(Box::new(Expr::Backlink { target: T.into() }))),
        set(&[R3]),
        "backlink negates soundly (it is exact, not a coarse superset)"
    );

    eprintln!("OK: untyped backlinks resolve across predicates and negate soundly");
}
