// SPDX-License-Identifier: AGPL-3.0-or-later
//! **OnTile same-tile correlation: the distinction `All` cannot make.**
//!
//! The flagship query — "built by Lanyon between 1860–1900" — must mean ONE
//! construction event carried both facts, not "a date in range somewhere AND a
//! Lanyon link somewhere in the resource." This test builds the exact case that
//! separates them: a resource with two construction events —
//!
//!   - 1870, built by someone else
//!   - 1855, built by Lanyon
//!
//! Neither event is both in-range AND by Lanyon, so `OnTile` must EXCLUDE it,
//! while `All` (each condition met somewhere) wrongly INCLUDES it. A control
//! resource whose single event is 1880-by-Lanyon must match both.
//!
//! Same-tile correlation rides tile identity that already exists in the substrate:
//! `tiles.tileid` and `edges.src_tile`. No re-emit — a pure query-side predicate.
//! Self-contained: builds a minimal Talk model inline (a cardinality-n
//! `construction` nodegroup carrying a date node + a link node).

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use alizarin_core::quantize::quantize_date;
use ros_madair_duck::{DuckError, DuckReader};
use ros_madair_handlers::default_registry;
use ros_madair_query::{Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
// The cardinality-n `construction` nodegroup (collector C) carries BOTH a date
// node (D) and a resource-instance-list link node (L) — so one tile of it holds
// an event's date and its builder together.
const C: &str = "cccc0000-0000-4000-8000-000000000001";
const D: &str = "dddd0000-0000-4000-8000-000000000001";
const L: &str = "ffff0000-0000-4000-8000-000000000001";
// A SECOND, separate nodegroup (its own collector) — for the cross-nodegroup
// error case (a tile belongs to one nodegroup, so correlating across is refused).
const AM: &str = "aaaadddd-0000-4000-8000-000000000001";

const R_TWO: &str = "11110000-0000-4000-8000-000000000001";
const R_ONE: &str = "22220000-0000-4000-8000-000000000001";
const LANYON: &str = "aaaa0000-0000-4000-8000-000000000001";
const ELSE: &str = "eeee0000-0000-4000-8000-000000000001";

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-corr-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node(nodeid: &str, ng: &str, alias: &str, datatype: &str, collector: bool) -> serde_json::Value {
    json!({
        "nodeid": nodeid, "nodegroup_id": ng, "name": alias, "alias": alias,
        "datatype": datatype, "graph_id": GRAPH, "istopnode": false, "is_collector": collector,
        "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0,
    })
}
fn edge(id: &str, from: &str, to: &str) -> serde_json::Value {
    json!({ "edgeid": id, "domainnode_id": from, "rangenode_id": to, "graph_id": GRAPH })
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({
        "nodeid": ROOT, "name": "Talk", "alias": "talk", "datatype": "semantic",
        "graph_id": GRAPH, "istopnode": true,
    });
    let doc = json!({ "graph": [{
        "graphid": GRAPH, "name": "Talk", "root": root.clone(),
        "nodes": [
            root,
            node(C, C, "construction", "semantic", true),
            node(D, C, "founded", "date", false),
            node(L, C, "builder", "resource-instance-list", false),
            node(AM, AM, "amended", "date", true),
        ],
        "nodegroups": [
            { "nodegroupid": C, "cardinality": "n", "parentnodegroup_id": null },
            { "nodegroupid": AM, "cardinality": "1", "parentnodegroup_id": null },
        ],
        "edges": [
            edge("e0000000-0000-4000-8000-000000000001", ROOT, C),
            edge("e0000000-0000-4000-8000-000000000002", C, D),
            edge("e0000000-0000-4000-8000-000000000003", C, L),
            edge("e0000000-0000-4000-8000-000000000004", ROOT, AM),
        ],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

/// One construction event: a `C` tile carrying a date (D) and a builder link (L).
fn event(tile: &str, r: &str, date: &str, builder: &str) -> serde_json::Value {
    json!({
        "tileid": tile, "nodegroup_id": C, "parenttile_id": null,
        "resourceinstance_id": r, "sortorder": 0, "provisionaledits": null,
        "data": { D: date, L: [{ "resourceId": builder }] },
    })
}
fn resource(r: &str, tiles: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "resourceinstance": { "resourceinstanceid": r, "graph_id": GRAPH, "name": r, "legacyid": null,
            "descriptors": { "en": { "name": r, "description": "", "map_popup": "" } } },
        "tiles": tiles,
    })
}

/// Emit the tile-row + edge Parquet; return (tiles_parquet, graph).
fn corpus() -> (PathBuf, StaticGraph) {
    let dir = scratch("corpus");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&rdir).unwrap();
    let resources = vec![
        // R_TWO: two events — 1870 by someone else, 1855 by Lanyon. Neither event
        // is BOTH in [1860,1900] AND by Lanyon.
        resource(
            R_TWO,
            vec![
                event(
                    "1a110000-0000-4000-8000-000000000010",
                    R_TWO,
                    "1870-01-01",
                    ELSE,
                ),
                event(
                    "1b110000-0000-4000-8000-000000000010",
                    R_TWO,
                    "1855-01-01",
                    LANYON,
                ),
            ],
        ),
        // R_ONE: one event, 1880 by Lanyon — in-range AND by Lanyon on ONE tile.
        resource(
            R_ONE,
            vec![event(
                "2a220000-0000-4000-8000-000000000010",
                R_ONE,
                "1880-01-01",
                LANYON,
            )],
        ),
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

fn date_in_range() -> Expr {
    Expr::Range {
        path: "founded".into(),
        lo: quantize_date("1860-01-01").unwrap(),
        hi: quantize_date("1900-12-31").unwrap(),
    }
}
fn by_lanyon() -> Expr {
    Expr::HasLink {
        path: "builder".into(),
        target: Some(LANYON.into()),
    }
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
fn on_tile_correlates_within_one_event_where_all_does_not() {
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

    // `All`: each condition met SOMEWHERE in the resource. R_TWO has a 1870 event
    // AND (separately) a Lanyon event, so it is wrongly included — the over-count.
    assert_eq!(
        ids(Expr::All(vec![date_in_range(), by_lanyon()])),
        set(&[R_ONE, R_TWO]),
        "All is resource-scoped: R_TWO's two separate events both satisfy it"
    );

    // `OnTile`: both conditions on ONE event. No single R_TWO event is in-range AND
    // by Lanyon, so it is excluded; R_ONE's single 1880-by-Lanyon event qualifies.
    assert_eq!(
        ids(Expr::OnTile(vec![date_in_range(), by_lanyon()])),
        set(&[R_ONE]),
        "OnTile is tile-scoped: only the single-event R_ONE survives"
    );

    // A single-child OnTile is just that condition, tile-scoped.
    assert_eq!(ids(Expr::OnTile(vec![by_lanyon()])), set(&[R_ONE, R_TWO]));

    // Cross-nodegroup correlation is unsatisfiable (a tile is one nodegroup
    // instance) — refused with a typed, repairable error, not a silent empty.
    let cross = Expr::OnTile(vec![
        date_in_range(),
        Expr::Range {
            path: "amended".into(),
            lo: 0,
            hi: i64::MAX,
        },
    ]);
    match duck.resolve_ids(&q(cross), &graph, &registry) {
        Err(DuckError::Compile(m)) => {
            assert!(m.contains("nodegroup"), "names the conflict: {m}");
        }
        other => panic!("expected a cross-nodegroup compile error, got {other:?}"),
    }

    eprintln!("OK: OnTile correlates within one tile; All over-counts across events");
}
