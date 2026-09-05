// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Self-contained behavioral tests for the DuckDB read path.**
//!
//! Hand-writes minimal Parquet via DuckDB (no external fixtures), covering
//! query compilation across all `Expr` variants, tile round-trip, display
//! lookups, and reverse-link geo queries.
//!
//! Corpus: three resources × four nodegroups (date, reference, geojson, link).
//!   r1: founded 2005, category A, point(0,0), links→r3, descriptor "Alpha"
//!   r2: founded 2020, category A, point(50,50), links→r3, descriptor "Beta"
//!   r3: founded 1900, category B, point(100,100), no links, descriptor "Gamma"
//! Concept catalog: A (root, dfs 0..1) → B (leaf, dfs 1..1).

use std::path::PathBuf;

use alizarin_core::graph::StaticGraph;
use alizarin_core::quantize::quantize_date;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{ConceptOp, Expr, Measure, Query};
use serde_json::json;

const NG_DATE: &str = "dd000000-0000-4000-8000-000000000001";
const NG_REF: &str = "cc000000-0000-4000-8000-000000000002";
const NG_GEO: &str = "gg000000-0000-4000-8000-000000000003";
const NG_LINK: &str = "ll000000-0000-4000-8000-000000000004";

const R1: &str = "11110000-0000-4000-8000-000000000001";
const R2: &str = "22220000-0000-4000-8000-000000000002";
const R3: &str = "33330000-0000-4000-8000-000000000003";

const CAT_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const CAT_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "rm-behav-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn graph() -> StaticGraph {
    serde_json::from_value(json!({
        "graphid": "test-g",
        "name": "TestGraph",
        "root": {
            "nodeid": "root", "name": "Root", "datatype": "semantic",
            "graph_id": "test-g"
        },
        "nodes": [
            {
                "nodeid": "root", "name": "Root", "datatype": "semantic",
                "graph_id": "test-g"
            },
            {
                "nodeid": NG_DATE, "name": "Founded", "alias": "founded",
                "datatype": "date", "nodegroup_id": NG_DATE,
                "graph_id": "test-g", "is_collector": true
            },
            {
                "nodeid": NG_REF, "name": "Category", "alias": "category",
                "datatype": "reference", "nodegroup_id": NG_REF,
                "graph_id": "test-g", "is_collector": true
            },
            {
                "nodeid": NG_GEO, "name": "Location", "alias": "location",
                "datatype": "geojson-feature-collection", "nodegroup_id": NG_GEO,
                "graph_id": "test-g", "is_collector": true
            },
            {
                "nodeid": NG_LINK, "name": "Related", "alias": "related",
                "datatype": "resource-instance-list", "nodegroup_id": NG_LINK,
                "graph_id": "test-g", "is_collector": true
            },
        ],
        "nodegroups": [
            {"nodegroupid": NG_DATE, "cardinality": "1", "parentnodegroup_id": null},
            {"nodegroupid": NG_REF, "cardinality": "1", "parentnodegroup_id": null},
            {"nodegroupid": NG_GEO, "cardinality": "1", "parentnodegroup_id": null},
            {"nodegroupid": NG_LINK, "cardinality": "1", "parentnodegroup_id": null},
        ],
        "edges": [
            {"domainnode_id": "root", "rangenode_id": NG_DATE, "graph_id": "test-g"},
            {"domainnode_id": "root", "rangenode_id": NG_REF, "graph_id": "test-g"},
            {"domainnode_id": "root", "rangenode_id": NG_GEO, "graph_id": "test-g"},
            {"domainnode_id": "root", "rangenode_id": NG_LINK, "graph_id": "test-g"},
        ],
    }))
    .unwrap()
}

// --- data column builders (serde_json avoids manual JSON-in-SQL escaping) ---

fn tile_data(node_id: &str, value: serde_json::Value) -> String {
    let mut m = serde_json::Map::new();
    m.insert(node_id.to_string(), value);
    serde_json::to_string(&serde_json::Value::Object(m)).unwrap()
}

fn geo_fc(lng: f64, lat: f64) -> serde_json::Value {
    json!({
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature",
            "properties": {},
            "geometry": {"type": "Point", "coordinates": [lng, lat]}
        }]
    })
}

fn link_targets_json(node_id: &str, targets: &[&str]) -> String {
    let mut m = serde_json::Map::new();
    m.insert(node_id.to_string(), json!(targets));
    serde_json::to_string(&serde_json::Value::Object(m)).unwrap()
}

// --- fixture ---

/// Build a 3-resource, 4-nodegroup tile Parquet + concept catalog.
/// Returns (dir, graph) — dir holds `tiles_test.parquet` and
/// `concept_catalog.parquet`.
fn fixture() -> (PathBuf, StaticGraph) {
    let dir = scratch("fix");
    let tiles = dir.join("tiles_test.parquet");
    let catalog = dir.join("concept_catalog.parquet");

    let q_2005 = quantize_date("2005-06-01").unwrap();
    let q_2020 = quantize_date("2020-03-15").unwrap();
    let q_1900 = quantize_date("1900-01-01").unwrap();

    let r1_date = tile_data(NG_DATE, json!("2005-06-01"));
    let r1_ref = tile_data(NG_REF, json!([CAT_A]));
    let r1_geo = tile_data(NG_GEO, geo_fc(0.0, 0.0));
    let r1_link = tile_data(NG_LINK, json!([{"resourceId": R3}]));
    let r1_lt = link_targets_json(NG_LINK, &[R3]);

    let r2_date = tile_data(NG_DATE, json!("2020-03-15"));
    let r2_ref = tile_data(NG_REF, json!([CAT_A]));
    let r2_geo = tile_data(NG_GEO, geo_fc(50.0, 50.0));
    let r2_link = tile_data(NG_LINK, json!([{"resourceId": R3}]));
    let r2_lt = link_targets_json(NG_LINK, &[R3]);

    let r3_date = tile_data(NG_DATE, json!("1900-01-01"));
    let r3_ref = tile_data(NG_REF, json!([CAT_B]));
    let r3_geo = tile_data(NG_GEO, geo_fc(100.0, 100.0));

    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE t (\
            resource_id VARCHAR, nodegroup_id VARCHAR, tileid VARCHAR,\
            parenttile_id VARCHAR, sortorder INTEGER, data VARCHAR,\
            descriptor_name VARCHAR, concept_id VARCHAR, q_ordered BIGINT,\
            geo_min_lng DOUBLE, geo_max_lng DOUBLE,\
            geo_min_lat DOUBLE, geo_max_lat DOUBLE,\
            link_targets VARCHAR\
        )",
    )
    .unwrap();

    let ins = |sql: &str| con.execute_batch(sql).unwrap();

    // r1 tiles (descriptor "Alpha" on every row, matching emitter behaviour)
    ins(&format!("INSERT INTO t VALUES('{R1}','{NG_DATE}','t01',NULL,0,'{r1_date}','Alpha',NULL,{q_2005},NULL,NULL,NULL,NULL,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R1}','{NG_REF}','t02',NULL,0,'{r1_ref}','Alpha','{CAT_A}',NULL,NULL,NULL,NULL,NULL,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R1}','{NG_GEO}','t03',NULL,0,'{r1_geo}','Alpha',NULL,NULL,0.0,0.0,0.0,0.0,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R1}','{NG_LINK}','t04',NULL,0,'{r1_link}','Alpha',NULL,NULL,NULL,NULL,NULL,NULL,'{r1_lt}')"));

    // r2 tiles
    ins(&format!("INSERT INTO t VALUES('{R2}','{NG_DATE}','t05',NULL,0,'{r2_date}','Beta',NULL,{q_2020},NULL,NULL,NULL,NULL,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R2}','{NG_REF}','t06',NULL,0,'{r2_ref}','Beta','{CAT_A}',NULL,NULL,NULL,NULL,NULL,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R2}','{NG_GEO}','t07',NULL,0,'{r2_geo}','Beta',NULL,NULL,50.0,50.0,50.0,50.0,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R2}','{NG_LINK}','t08',NULL,0,'{r2_link}','Beta',NULL,NULL,NULL,NULL,NULL,NULL,'{r2_lt}')"));

    // r3 tiles (no link tile)
    ins(&format!("INSERT INTO t VALUES('{R3}','{NG_DATE}','t09',NULL,0,'{r3_date}','Gamma',NULL,{q_1900},NULL,NULL,NULL,NULL,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R3}','{NG_REF}','t10',NULL,0,'{r3_ref}','Gamma','{CAT_B}',NULL,NULL,NULL,NULL,NULL,NULL)"));
    ins(&format!("INSERT INTO t VALUES('{R3}','{NG_GEO}','t11',NULL,0,'{r3_geo}','Gamma',NULL,NULL,100.0,100.0,100.0,100.0,NULL)"));

    con.execute_batch(&format!(
        "COPY (SELECT resource_id, nodegroup_id, tileid, parenttile_id, sortorder, data, \
         descriptor_name, \
         CASE WHEN concept_id IS NULL THEN NULL ELSE '[\"' || concept_id || '\"]' END AS concept_ids, \
         q_ordered, geo_min_lng, geo_max_lng, geo_min_lat, geo_max_lat, link_targets FROM t) \
         TO '{}' (FORMAT PARQUET)",
        tiles.display()
    ))
    .unwrap();

    // Concept catalog: A is root (dfs 0..1), B is child leaf (dfs 1..1).
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('{CAT_A}',0,1,'Category A'),\
            ('{CAT_B}',1,1,'Category B')\
        ) c(concept_id, dfs_enter, dfs_leave, label)) TO '{}' (FORMAT PARQUET)",
        catalog.display()
    ))
    .unwrap();

    // Edge table: r1 and r2 link to r3 via the `related` node (mirrors the
    // link_targets on t04/t08). Lets the OnLink path predicate semijoin.
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('{R1}','{NG_LINK}','{NG_LINK}','t04','{R3}'),\
            ('{R2}','{NG_LINK}','{NG_LINK}','t08','{R3}')\
        ) e(src_resource, src_node, src_nodegroup, src_tile, target_resource)) \
         TO '{}' (FORMAT PARQUET)",
        dir.join("edges_test.parquet").display()
    ))
    .unwrap();

    (dir, graph())
}

fn q(w: Expr) -> Query {
    Query {
        model: "test-g".into(),
        r#where: Some(w),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

fn set(xs: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = xs.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn query_resolution_covers_all_expr_variants() {
    let (dir, graph) = fixture();
    let registry = default_registry();
    let duck = DuckReader::open_with_catalog(
        dir.join("tiles_test.parquet").to_str().unwrap(),
        dir.join("concept_catalog.parquet").to_str().unwrap(),
    )
    .unwrap();
    let ids = |w: Expr| {
        let mut v = duck.resolve_ids(&q(w), &graph, &registry).unwrap();
        v.sort();
        v
    };

    // Range (date / Ordered)
    assert_eq!(
        ids(Expr::Range {
            path: "founded".into(),
            lo: quantize_date("2000-01-01").unwrap(),
            hi: quantize_date("2010-12-31").unwrap(),
        }),
        set(&[R1]),
        "range 2000-2010 = r1"
    );
    assert_eq!(
        ids(Expr::Range {
            path: "founded".into(),
            lo: quantize_date("1800-01-01").unwrap(),
            hi: quantize_date("2100-01-01").unwrap(),
        }),
        set(&[R1, R2, R3]),
        "range 1800-2100 = all"
    );

    // Concept Is (reference / ConceptHierarchical)
    assert_eq!(
        ids(Expr::Concept {
            path: "category".into(),
            op: ConceptOp::Is,
            value: CAT_A.into(),
        }),
        set(&[R1, R2]),
        "concept-is A"
    );
    assert_eq!(
        ids(Expr::Concept {
            path: "category".into(),
            op: ConceptOp::Is,
            value: CAT_B.into(),
        }),
        set(&[R3]),
        "concept-is B"
    );

    // DescendantOrSelfOf — A is root (dfs 0..1), B is child (dfs 1..1);
    // subtree(A) covers both, subtree(B) covers only B.
    assert_eq!(
        ids(Expr::Concept {
            path: "category".into(),
            op: ConceptOp::DescendantOrSelfOf,
            value: CAT_A.into(),
        }),
        set(&[R1, R2, R3]),
        "descendant-or-self(A) = all"
    );
    assert_eq!(
        ids(Expr::Concept {
            path: "category".into(),
            op: ConceptOp::DescendantOrSelfOf,
            value: CAT_B.into(),
        }),
        set(&[R3]),
        "descendant-or-self(B) = r3"
    );

    // Bbox — coarse at minimum; fine if spatial is loaded. Point(0,0) is
    // inside (-1,-1)-(1,1); Point(50,50) and Point(100,100) are not.
    assert_eq!(
        ids(Expr::Bbox {
            path: "location".into(),
            min_lng: -1.0,
            min_lat: -1.0,
            max_lng: 1.0,
            max_lat: 1.0,
        }),
        set(&[R1]),
        "bbox around origin = r1"
    );

    // HasLink
    assert_eq!(
        ids(Expr::HasLink { path: "related".into(), target: Some(R3.into()) }),
        set(&[R1, R2]),
        "haslink(r3)"
    );
    assert_eq!(
        ids(Expr::HasLink { path: "related".into(), target: None }),
        set(&[R1, R2]),
        "haslink(any)"
    );
    assert_eq!(
        ids(Expr::HasLink { path: "related".into(), target: Some(R1.into()) }),
        Vec::<String>::new(),
        "haslink(r1) = nobody"
    );

    // Compound: All, Any, Not
    assert_eq!(
        ids(Expr::All(vec![
            Expr::Concept { path: "category".into(), op: ConceptOp::Is, value: CAT_A.into() },
            Expr::Range {
                path: "founded".into(),
                lo: quantize_date("2015-01-01").unwrap(),
                hi: quantize_date("2025-01-01").unwrap(),
            },
        ])),
        set(&[R2]),
        "all(cat-A AND 2015-2025) = r2"
    );
    assert_eq!(
        ids(Expr::Any(vec![
            Expr::Concept { path: "category".into(), op: ConceptOp::Is, value: CAT_A.into() },
            Expr::Concept { path: "category".into(), op: ConceptOp::Is, value: CAT_B.into() },
        ])),
        set(&[R1, R2, R3]),
        "any(cat-A OR cat-B) = all"
    );
    assert_eq!(
        ids(Expr::Not(Box::new(Expr::Concept {
            path: "category".into(),
            op: ConceptOp::Is,
            value: CAT_A.into(),
        }))),
        set(&[R3]),
        "not(cat-A) = r3"
    );

    // Vacuous: empty All = all, empty Any = none
    assert_eq!(ids(Expr::All(vec![])), set(&[R1, R2, R3]), "empty All = all");
    assert_eq!(ids(Expr::Any(vec![])), Vec::<String>::new(), "empty Any = none");

    // count_records agrees with resolve_ids length
    let count = duck
        .count_records(
            &q(Expr::Range {
                path: "founded".into(),
                lo: quantize_date("1800-01-01").unwrap(),
                hi: quantize_date("2100-01-01").unwrap(),
            }),
            &graph,
            &registry,
        )
        .unwrap();
    assert_eq!(count, 3, "count_records = len(resolve_ids)");
}

#[test]
fn resource_tiles_round_trip() {
    let (dir, _) = fixture();
    let duck =
        DuckReader::open(dir.join("tiles_test.parquet").to_str().unwrap()).unwrap();

    let tiles = duck.resource_tiles(R1).unwrap();
    assert_eq!(tiles.len(), 4, "r1 has 4 tiles");

    let mut ngs: Vec<&str> = tiles.iter().map(|t| t.nodegroup_id.as_str()).collect();
    ngs.sort();
    assert_eq!(ngs, {
        let mut v = vec![NG_DATE, NG_GEO, NG_LINK, NG_REF];
        v.sort();
        v
    });

    let date_tile = tiles.iter().find(|t| t.nodegroup_id == NG_DATE).unwrap();
    assert_eq!(date_tile.data[NG_DATE], json!("2005-06-01"), "data round-trips");
    assert_eq!(date_tile.tileid.as_deref(), Some("t01"), "tileid preserved");
    assert_eq!(date_tile.resourceinstance_id, R1, "resource id set");

    assert!(
        duck.resource_tiles("nonexistent").unwrap().is_empty(),
        "missing resource -> empty"
    );
}

#[test]
fn descriptors_and_concept_labels() {
    let (dir, _) = fixture();
    let duck = DuckReader::open_with_catalog(
        dir.join("tiles_test.parquet").to_str().unwrap(),
        dir.join("concept_catalog.parquet").to_str().unwrap(),
    )
    .unwrap();

    let uris = vec![R1.into(), R2.into(), R3.into()];
    let descs = duck.descriptors(&uris).unwrap();
    assert_eq!(descs.get(R1).map(String::as_str), Some("Alpha"));
    assert_eq!(descs.get(R2).map(String::as_str), Some("Beta"));
    assert_eq!(descs.get(R3).map(String::as_str), Some("Gamma"));
    assert!(duck.descriptors(&[]).unwrap().is_empty(), "empty in -> empty out");

    let labels = duck.concept_labels().unwrap();
    assert_eq!(labels.get(CAT_A).map(String::as_str), Some("Category A"));
    assert_eq!(labels.get(CAT_B).map(String::as_str), Some("Category B"));

    assert_eq!(duck.concept_label(CAT_A).unwrap().as_deref(), Some("Category A"));
    assert_eq!(duck.concept_label("nonexistent").unwrap(), None);
}

#[test]
fn geo_points_reverse_link_lookup() {
    let (dir, _) = fixture();
    let duck =
        DuckReader::open(dir.join("tiles_test.parquet").to_str().unwrap()).unwrap();

    let mut pts = duck.geo_points(NG_LINK, R3).unwrap();
    pts.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(pts.len(), 2, "r1 and r2 link to r3");
    assert_eq!(pts[0].0, R1);
    assert_eq!(pts[0].1, "Alpha", "descriptor from link tile");
    assert_eq!((pts[0].2, pts[0].3), (0.0, 0.0), "r1 geo");
    assert_eq!(pts[1].0, R2);
    assert_eq!(pts[1].1, "Beta");
    assert_eq!((pts[1].2, pts[1].3), (50.0, 50.0), "r2 geo");

    assert!(
        duck.geo_points(NG_LINK, R1).unwrap().is_empty(),
        "nobody links to r1"
    );
}

#[test]
fn hydrate_layers_single_layer_smoke() {
    let (dir, graph) = fixture();
    // hydrate_layers now takes a LayeredGraph. `new` rejects an empty overlay set
    // ("use StaticGraph directly"), so the degenerate single-layer case is the
    // base plus a clone as the sole overlay — the same idiom alizarin-core's own
    // tests use for a one-graph LayeredGraph.
    let composed = alizarin_core::LayeredGraph::new(
        std::sync::Arc::new(graph.clone()),
        vec![std::sync::Arc::new(graph)],
    );
    let fn_registry = alizarin_core::default_functions_registry();
    let result = ros_madair_duck::hydrate_layers(
        &[dir.as_path()],
        R1,
        &composed,
        &["en"],
        None,
        &fn_registry,
    )
    .unwrap();
    assert!(result.is_object(), "hydrate returns a JSON tree");

    let err = ros_madair_duck::hydrate_layers(
        &[dir.as_path()],
        "nonexistent",
        &composed,
        &["en"],
        None,
        &fn_registry,
    );
    assert!(err.is_err(), "missing resource -> error");
}

#[test]
fn on_link_path_predicate_semijoins_the_edge_table() {
    let (dir, graph) = fixture();
    let registry = default_registry();
    let duck = DuckReader::open_with_catalog(
        dir.join("tiles_test.parquet").to_str().unwrap(),
        dir.join("concept_catalog.parquet").to_str().unwrap(),
    )
    .unwrap();

    let on_link_category = |value: &str| {
        Expr::OnLink {
            path: "related".into(),
            model: "test-g".into(),
            r#where: Box::new(Expr::Concept {
                path: "category".into(),
                op: ConceptOp::Is,
                value: value.into(),
            }),
        }
    };

    // Resources whose `related` target is Category B (r3): r1 and r2 both link to r3.
    let mut ids = duck.resolve_ids(&q(on_link_category(CAT_B)), &graph, &registry).unwrap();
    ids.sort();
    assert_eq!(ids, set(&[R1, R2]), "r1,r2 link to r3, which is Category B");

    // Nobody links to a Category-A resource (r1,r2 ARE category A, but nothing
    // links to them), so the hop yields the empty set.
    let none = duck.resolve_ids(&q(on_link_category(CAT_A)), &graph, &registry).unwrap();
    assert!(none.is_empty(), "nobody links to a Category-A resource");

    // Composes with a local predicate: link-to-B AND own category is A → r1,r2.
    let mut both = duck
        .resolve_ids(
            &q(Expr::All(vec![
                on_link_category(CAT_B),
                Expr::Concept { path: "category".into(), op: ConceptOp::Is, value: CAT_A.into() },
            ])),
            &graph,
            &registry,
        )
        .unwrap();
    both.sort();
    assert_eq!(both, set(&[R1, R2]), "OnLink INTERSECT local concept predicate");
}

#[test]
fn open_layers_composes_tiles_with_overlay_precedence() {
    let base = scratch("lbase");
    let overlay = scratch("lover");

    // One tile for R1 on the category nodegroup, with a given concept id.
    let write_tile = |dir: &std::path::Path, concept: &str| {
        let con = duckdb::Connection::open_in_memory().unwrap();
        con.execute_batch(
            "CREATE TABLE t (\
                resource_id VARCHAR, nodegroup_id VARCHAR, tileid VARCHAR,\
                parenttile_id VARCHAR, sortorder INTEGER, data VARCHAR,\
                descriptor_name VARCHAR, concept_id VARCHAR, q_ordered BIGINT,\
                geo_min_lng DOUBLE, geo_max_lng DOUBLE,\
                geo_min_lat DOUBLE, geo_max_lat DOUBLE, link_targets VARCHAR)",
        )
        .unwrap();
        con.execute_batch(&format!(
            "INSERT INTO t VALUES('{R1}','{NG_REF}','tx',NULL,0,'{{}}','Alpha',\
             '{concept}',NULL,NULL,NULL,NULL,NULL,NULL)"
        ))
        .unwrap();
        con.execute_batch(&format!(
            "COPY (SELECT resource_id, nodegroup_id, tileid, parenttile_id, sortorder, data, \
             descriptor_name, \
             CASE WHEN concept_id IS NULL THEN NULL ELSE '[\"' || concept_id || '\"]' END AS concept_ids, \
             q_ordered, geo_min_lng, geo_max_lng, geo_min_lat, geo_max_lat, link_targets FROM t) \
             TO '{}/tiles_test.parquet' (FORMAT PARQUET)",
            dir.display()
        ))
        .unwrap();
    };
    write_tile(&base, CAT_A);
    write_tile(&overlay, CAT_B);

    let duck = DuckReader::open_layers(&[base.as_path(), overlay.as_path()]).unwrap();
    let registry = default_registry();
    let hit = |value: &str| {
        let mut v = duck
            .resolve_ids(
                &q(Expr::Concept {
                    path: "category".into(),
                    op: ConceptOp::Is,
                    value: value.into(),
                }),
                &graph(),
                &registry,
            )
            .unwrap();
        v.sort();
        v
    };
    // The overlay's Category-B tile overrides the base's Category-A tile for the
    // same (resource, nodegroup): the composed search sees B, not A.
    assert_eq!(hit(CAT_B), set(&[R1]), "overlay's tile wins");
    assert!(hit(CAT_A).is_empty(), "base's tile is overridden, not unioned");
}

#[test]
fn on_link_crosses_layers() {
    // The building→org shape: the TARGET tile lives in one layer, the linking
    // EDGE in another. base carries r3 (Category B); overlay carries only the edge
    // r1→r3. A single OnLink hop must cross the layer boundary.
    let base = scratch("xlbase");
    let overlay = scratch("xlover");

    // base: r3's category tile (Category B), no edges.
    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE t (\
            resource_id VARCHAR, nodegroup_id VARCHAR, tileid VARCHAR,\
            parenttile_id VARCHAR, sortorder INTEGER, data VARCHAR,\
            descriptor_name VARCHAR, concept_id VARCHAR, q_ordered BIGINT,\
            geo_min_lng DOUBLE, geo_max_lng DOUBLE,\
            geo_min_lat DOUBLE, geo_max_lat DOUBLE, link_targets VARCHAR)",
    )
    .unwrap();
    con.execute_batch(&format!(
        "INSERT INTO t VALUES('{R3}','{NG_REF}','tx',NULL,0,'{{}}','Gamma',\
         '{CAT_B}',NULL,NULL,NULL,NULL,NULL,NULL)"
    ))
    .unwrap();
    con.execute_batch(&format!(
        "COPY t TO '{}/tiles_test.parquet' (FORMAT PARQUET)",
        base.display()
    ))
    .unwrap();

    // overlay: ONLY the edge r1 → r3 (the link lives in a different layer than the
    // target it points at).
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('{R1}','{NG_LINK}','{NG_LINK}','t04','{R3}')\
        ) e(src_resource, src_node, src_nodegroup, src_tile, target_resource)) \
         TO '{}/edges_test.parquet' (FORMAT PARQUET)",
        overlay.display()
    ))
    .unwrap();

    let duck = DuckReader::open_layers(&[base.as_path(), overlay.as_path()]).unwrap();
    let registry = default_registry();
    let ids = duck
        .resolve_ids(
            &q(Expr::OnLink {
                path: "related".into(),
                model: "test-g".into(),
                r#where: Box::new(Expr::Concept {
                    path: "category".into(),
                    op: ConceptOp::Is,
                    value: CAT_B.into(),
                }),
            }),
            &graph(),
            &registry,
        )
        .unwrap();
    // Inner (Category B) matches r3 in the BASE layer; the edge r1→r3 is in the
    // OVERLAY layer; the composed views join them → r1. No shadow record needed.
    assert_eq!(ids, set(&[R1]), "hop crosses layers: edge in overlay, target in base");
}

#[test]
fn on_link_crosses_models() {
    // Two models: Org has a `location` link → Place; Place has a `designation`
    // (reference) concept. An OnLink from Org filters on the linked Place's
    // designation — the inner predicate resolves against the Place model, which
    // the caller supplies via `linked`.
    const ORG_G: &str = "org-g";
    const PLACE_G: &str = "place-g";
    const NG_LOC: &str = "cc000000-0000-4000-8000-000000000005";
    const NG_DES: &str = "dd000000-0000-4000-8000-000000000006";
    let r_org = "eeee0000-0000-4000-8000-000000000001";
    let r_place = "ffff0000-0000-4000-8000-000000000001";

    let org_graph: StaticGraph = serde_json::from_value(json!({
        "graphid": ORG_G, "name": "Org",
        "root": {"nodeid": "org-root", "name": "Org", "datatype": "semantic", "graph_id": ORG_G},
        "nodes": [
            {"nodeid": "org-root", "name": "Org", "datatype": "semantic", "graph_id": ORG_G},
            {"nodeid": NG_LOC, "name": "Location", "alias": "location",
             "datatype": "resource-instance", "nodegroup_id": NG_LOC, "graph_id": ORG_G, "is_collector": true},
        ],
        "nodegroups": [{"nodegroupid": NG_LOC, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [{"domainnode_id": "org-root", "rangenode_id": NG_LOC, "graph_id": ORG_G}],
    }))
    .unwrap();
    let place_graph: StaticGraph = serde_json::from_value(json!({
        "graphid": PLACE_G, "name": "Place",
        "root": {"nodeid": "place-root", "name": "Place", "datatype": "semantic", "graph_id": PLACE_G},
        "nodes": [
            {"nodeid": "place-root", "name": "Place", "datatype": "semantic", "graph_id": PLACE_G},
            {"nodeid": NG_DES, "name": "Designation", "alias": "designation",
             "datatype": "reference", "nodegroup_id": NG_DES, "graph_id": PLACE_G, "is_collector": true},
        ],
        "nodegroups": [{"nodegroupid": NG_DES, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [{"domainnode_id": "place-root", "rangenode_id": NG_DES, "graph_id": PLACE_G}],
    }))
    .unwrap();

    // One reader over both models' tiles + edges (nodegroup ids are globally
    // unique, so a single tiles/edges view holding both models is unambiguous).
    let dir = scratch("xmodel");
    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE t (\
            resource_id VARCHAR, nodegroup_id VARCHAR, tileid VARCHAR,\
            parenttile_id VARCHAR, sortorder INTEGER, data VARCHAR,\
            descriptor_name VARCHAR, concept_id VARCHAR, q_ordered BIGINT,\
            geo_min_lng DOUBLE, geo_max_lng DOUBLE,\
            geo_min_lat DOUBLE, geo_max_lat DOUBLE, link_targets VARCHAR)",
    )
    .unwrap();
    con.execute_batch(&format!(
        "INSERT INTO t VALUES('{r_place}','{NG_DES}','tp',NULL,0,'{{}}','Place',\
         '{CAT_B}',NULL,NULL,NULL,NULL,NULL,NULL)"
    ))
    .unwrap();
    con.execute_batch(&format!(
        "COPY t TO '{}/tiles_test.parquet' (FORMAT PARQUET)",
        dir.display()
    ))
    .unwrap();
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('{r_org}','{NG_LOC}','{NG_LOC}','tl','{r_place}')\
        ) e(src_resource, src_node, src_nodegroup, src_tile, target_resource)) \
         TO '{}/edges_test.parquet' (FORMAT PARQUET)",
        dir.display()
    ))
    .unwrap();

    let duck = DuckReader::open(dir.join("tiles_test.parquet").to_str().unwrap()).unwrap();
    let registry = default_registry();

    let query = Query {
        model: ORG_G.into(),
        r#where: Some(Expr::OnLink {
            path: "location".into(),
            model: PLACE_G.into(),
            r#where: Box::new(Expr::Concept {
                path: "designation".into(),
                op: ConceptOp::Is,
                value: CAT_B.into(),
            }),
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    };

    // With the Place model supplied, the hop resolves across models → the org.
    let ids = duck
        .resolve_ids_linked(&query, &org_graph, &[&place_graph], &registry)
        .unwrap();
    assert_eq!(ids, set(&[r_org]), "Org → Place(designation=B): the linking org");

    // Without it, the target model 'place-g' is unknown to the compiler → error.
    let err = duck.resolve_ids(&query, &org_graph, &registry);
    assert!(err.is_err(), "cross-model OnLink without the linked model errors");
}

#[test]
fn on_link_chain_two_hops() {
    // a → b → c, with c Category B. A nested OnLink (hop, then hop, then the leaf)
    // folds to nested edge semijoins: category-B matches c; related-to-c gives b;
    // related-to-b gives a. So the two-hop chain resolves to {a}.
    let a = "aa000000-0000-4000-8000-000000000001";
    let b = "bb000000-0000-4000-8000-000000000001";
    let c = "cc000000-0000-4000-8000-000000000001";

    let dir = scratch("chain");
    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE t (\
            resource_id VARCHAR, nodegroup_id VARCHAR, tileid VARCHAR,\
            parenttile_id VARCHAR, sortorder INTEGER, data VARCHAR,\
            descriptor_name VARCHAR, concept_id VARCHAR, q_ordered BIGINT,\
            geo_min_lng DOUBLE, geo_max_lng DOUBLE,\
            geo_min_lat DOUBLE, geo_max_lat DOUBLE, link_targets VARCHAR)",
    )
    .unwrap();
    // Only c carries a category tile (Category B) — the leaf of the chain.
    con.execute_batch(&format!(
        "INSERT INTO t VALUES('{c}','{NG_REF}','tc',NULL,0,'{{}}','C',\
         '{CAT_B}',NULL,NULL,NULL,NULL,NULL,NULL)"
    ))
    .unwrap();
    con.execute_batch(&format!(
        "COPY t TO '{}/tiles_test.parquet' (FORMAT PARQUET)",
        dir.display()
    ))
    .unwrap();
    // Edges: a → b and b → c, both via `related`.
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('{a}','{NG_LINK}','{NG_LINK}','ta','{b}'),\
            ('{b}','{NG_LINK}','{NG_LINK}','tb','{c}')\
        ) e(src_resource, src_node, src_nodegroup, src_tile, target_resource)) \
         TO '{}/edges_test.parquet' (FORMAT PARQUET)",
        dir.display()
    ))
    .unwrap();

    let duck = DuckReader::open(dir.join("tiles_test.parquet").to_str().unwrap()).unwrap();
    let registry = default_registry();

    let on_link = |inner: Expr| Expr::OnLink {
        path: "related".into(),
        model: "test-g".into(),
        r#where: Box::new(inner),
    };
    let chain = on_link(on_link(Expr::Concept {
        path: "category".into(),
        op: ConceptOp::Is,
        value: CAT_B.into(),
    }));

    let ids = duck.resolve_ids(&q(chain), &graph(), &registry).unwrap();
    assert_eq!(ids, set(&[a]), "two-hop chain a→b→c(CategoryB) resolves to a");
}

#[test]
fn dot_qualified_path_walks_the_schema_tree() {
    // root → info (semantic) → category (reference). A dotted path resolves the
    // nested node, so a predicate can name `info.category`.
    const G: &str = "nest-g";
    const INFO: &str = "10000000-0000-4000-8000-000000000001";
    const CATNG: &str = "20000000-0000-4000-8000-000000000002";
    let r = "30000000-0000-4000-8000-000000000003";

    let graph: StaticGraph = serde_json::from_value(json!({
        "graphid": G, "name": "Nest",
        "root": {"nodeid": "nroot", "name": "Nest", "datatype": "semantic", "graph_id": G},
        "nodes": [
            {"nodeid": "nroot", "name": "Nest", "datatype": "semantic", "graph_id": G},
            {"nodeid": INFO, "name": "Info", "alias": "info", "datatype": "semantic",
             "nodegroup_id": INFO, "graph_id": G, "is_collector": true},
            {"nodeid": CATNG, "name": "Category", "alias": "category", "datatype": "reference",
             "nodegroup_id": CATNG, "graph_id": G, "is_collector": true},
        ],
        "nodegroups": [
            {"nodegroupid": INFO, "cardinality": "1", "parentnodegroup_id": null},
            {"nodegroupid": CATNG, "cardinality": "1", "parentnodegroup_id": INFO},
        ],
        "edges": [
            {"domainnode_id": "nroot", "rangenode_id": INFO, "graph_id": G},
            {"domainnode_id": INFO, "rangenode_id": CATNG, "graph_id": G},
        ],
    }))
    .unwrap();

    let dir = scratch("dotted");
    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(
        "CREATE TABLE t (\
            resource_id VARCHAR, nodegroup_id VARCHAR, tileid VARCHAR,\
            parenttile_id VARCHAR, sortorder INTEGER, data VARCHAR,\
            descriptor_name VARCHAR, concept_id VARCHAR, q_ordered BIGINT,\
            geo_min_lng DOUBLE, geo_max_lng DOUBLE,\
            geo_min_lat DOUBLE, geo_max_lat DOUBLE, link_targets VARCHAR)",
    )
    .unwrap();
    con.execute_batch(&format!(
        "INSERT INTO t VALUES('{r}','{CATNG}','td',NULL,0,'{{}}','R',\
         '{CAT_B}',NULL,NULL,NULL,NULL,NULL,NULL)"
    ))
    .unwrap();
    con.execute_batch(&format!(
        "COPY t TO '{}/tiles_test.parquet' (FORMAT PARQUET)",
        dir.display()
    ))
    .unwrap();

    let duck = DuckReader::open(dir.join("tiles_test.parquet").to_str().unwrap()).unwrap();
    let registry = default_registry();
    let query = |path: &str| Query {
        model: G.into(),
        r#where: Some(Expr::Concept {
            path: path.into(),
            op: ConceptOp::Is,
            value: CAT_B.into(),
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    };

    // The dotted path resolves the nested reference node and matches.
    let ids = duck.resolve_ids(&query("info.category"), &graph, &registry).unwrap();
    assert_eq!(ids, set(&[r]), "info.category resolves through the tree");

    // An unknown component is a typed error, not a panic.
    assert!(
        duck.resolve_ids(&query("info.nope"), &graph, &registry).is_err(),
        "unknown dotted component errors"
    );
}
