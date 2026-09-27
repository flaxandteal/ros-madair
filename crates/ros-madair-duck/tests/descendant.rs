// SPDX-License-Identifier: AGPL-3.0-or-later
//! **`DescendantOrSelfOf` over the DFS-ordered concept catalog** — a subtree is a
//! single `dfs_enter BETWEEN pre_X AND submax_X` range read. Self-contained: it
//! hand-writes a tiny tiles + concept-catalog Parquet via DuckDB (no SKOS-emit
//! path), so it exercises the query compilation and the range-join directly.
//!
//! Concept tree (DFS pre/leave):
//!   root [0,3] → { a [1,2] → { a1 [2,2] }, b [3,3] }
//! Tiles: r1→a1, r2→b, r3→root, r4→a  (all in nodegroup NG).

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{ConceptOp, Expr, Measure, Query};

const NG: &str = "5efd0000-0000-4000-8000-000000000099";

fn scratch() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("rm-desc-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A minimal graph: root → one `reference` node aliased `topic` in nodegroup NG.
fn graph() -> StaticGraph {
    serde_json::from_value(serde_json::json!({
        "graphid": "g", "name": "G",
        "root": {"nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "g"},
        "nodes": [
            {"nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "g"},
            {"nodeid": NG, "name": "Topic", "alias": "topic", "datatype": "reference",
             "nodegroup_id": NG, "graph_id": "g", "is_collector": true},
        ],
        "nodegroups": [{"nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [{"domainnode_id": "root", "rangenode_id": NG, "graph_id": "g"}],
    }))
    .unwrap()
}

fn descendant_of(v: &str) -> Query {
    Query {
        model: "g".into(),
        r#where: Some(Expr::Concept { path: "topic".into(), op: ConceptOp::DescendantOrSelfOf, value: v.into() }),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

#[test]
fn descendant_or_self_is_a_subtree_range() {
    let dir = scratch();
    // `tiles_` prefix so open_with resolves the sibling concepts_ store by glob-replace.
    let tiles = dir.join("tiles_g.parquet");
    let cat = dir.join("concept_catalog.parquet");

    // Hand-write the tiles + melted concept store + DFS catalog. The concept values
    // (r1→a1, r2→b, r3→root, r4→a) now live in the melted concept_index, node-scoped on
    // the `topic` node (nodeid == NG here); the DescendantOrSelfOf range-joins the catalog.
    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('r1','{NG}'),('r2','{NG}'),('r3','{NG}'),('r4','{NG}') \
         ) t(resource_id, nodegroup_id)) TO '{tiles}' (FORMAT PARQUET); \
         COPY (SELECT * FROM (VALUES \
            ('r1','r1','{NG}','{NG}','a1'),('r2','r2','{NG}','{NG}','b'),\
            ('r3','r3','{NG}','{NG}','root'),('r4','r4','{NG}','{NG}','a') \
         ) ci(resource_id, tile_id, nodegroup_id, node_id, concept_id)) TO '{concepts}' (FORMAT PARQUET); \
         COPY (SELECT * FROM (VALUES \
            ('root',0,3,'Root'),('a',1,2,'A'),('a1',2,2,'A1'),('b',3,3,'B') \
         ) c(concept_id, dfs_enter, dfs_leave, label)) TO '{cat}' (FORMAT PARQUET);",
        tiles = tiles.display(),
        concepts = dir.join("concepts_g.parquet").display(),
        cat = cat.display(),
    ))
    .unwrap();

    let reader = DuckReader::open_with_catalog(tiles.to_str().unwrap(), cat.to_str().unwrap()).unwrap();
    let registry = default_registry();
    let g = graph();
    let ids = |v: &str| {
        let mut r = reader.resolve_ids(&descendant_of(v), &g, &registry).unwrap();
        r.sort();
        r
    };
    let set = |xs: &[&str]| {
        let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };

    assert_eq!(ids("root"), set(&["r1", "r2", "r3", "r4"]), "root's subtree = all");
    assert_eq!(ids("a"), set(&["r1", "r4"]), "a's subtree = {{a, a1}} → r4, r1");
    assert_eq!(ids("a1"), set(&["r1"]), "a1 is a leaf");
    assert_eq!(ids("b"), set(&["r2"]), "b is a leaf");

    // The catalog also answers label lookups (the v2_closure vocab home).
    assert_eq!(reader.concept_label("a1").unwrap().as_deref(), Some("A1"));

    eprintln!("OK: DescendantOrSelfOf is a DFS-interval subtree range; labels resolve");
}
