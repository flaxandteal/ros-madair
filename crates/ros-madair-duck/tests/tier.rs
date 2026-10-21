// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Default-deny tier — the insurer-window subset head, proven safe by construction.**
//!
//! Track A of the Aonach Mór Responder: an emitted head can be SCOPED by dropping
//! rows at emit (egress-by-emit), so the withheld data is physically absent — an
//! over-reach returns empty because the bytes aren't there, not because a toggle
//! said no. This test emits a tiered `building` head (a `contacts` nodegroup
//! excluded, only the two "covered" buildings included) and asserts it CANNOT leak:
//!   1. row cut — the head names only the covered buildings;
//!   2. field cut — zero `contacts` rows in any store;
//!   3. declaration — `manifest.tier` lists what it dropped (matches reality);
//!   4. behavioral over-reach — a query for the contact field returns empty;
//!   5. tamper-evidence — the tier is inside the signed `snapshot_id`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::default_registry;
use ros_madair_query::{ConceptOp, Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "b0000000-0000-4000-8000-000000000001";
const ROOT: &str = "b0000000-0000-4000-8000-000000000002";
const NG_PROFILE: &str = "b0000000-0000-4000-8000-0000000000a1"; // geo — KEPT
const FOOTPRINT: &str = "b0000000-0000-4000-8000-0000000000a2";
const NG_CONTACT: &str = "b0000000-0000-4000-8000-0000000000c1"; // keyholder — EXCLUDED
const CONTACT_ROLE: &str = "b0000000-0000-4000-8000-0000000000c2";

const B1: &str = "11110000-0000-4000-8000-000000000001"; // covered
const B2: &str = "22220000-0000-4000-8000-000000000002"; // covered
const B3: &str = "33330000-0000-4000-8000-000000000003"; // NOT covered
const KEYHOLDER: &str = "dddd0000-0000-4000-8000-000000000001"; // a contact concept value

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-tier-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({"nodeid": ROOT, "name": "Building", "alias": "building", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    let footprint = json!({"nodeid": FOOTPRINT, "nodegroup_id": NG_PROFILE, "name": "footprint", "alias": "footprint",
        "datatype": "geojson-feature-collection", "graph_id": GRAPH, "istopnode": false,
        "is_collector": true, "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0});
    let contact = json!({"nodeid": CONTACT_ROLE, "nodegroup_id": NG_CONTACT, "name": "keyholder", "alias": "keyholder",
        "datatype": "concept", "graph_id": GRAPH, "istopnode": false,
        "is_collector": true, "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0});
    let doc = json!({"graph": [{
        "graphid": GRAPH, "name": "Building", "root": root.clone(),
        "nodes": [root, footprint, contact],
        "nodegroups": [
            {"nodegroupid": NG_PROFILE, "cardinality": "1", "parentnodegroup_id": null},
            {"nodegroupid": NG_CONTACT, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [
            {"edgeid": "e1", "domainnode_id": ROOT, "rangenode_id": NG_PROFILE, "graph_id": GRAPH},
            {"edgeid": "e2", "domainnode_id": NG_PROFILE, "rangenode_id": FOOTPRINT, "graph_id": GRAPH},
            {"edgeid": "e3", "domainnode_id": ROOT, "rangenode_id": NG_CONTACT, "graph_id": GRAPH},
            {"edgeid": "e4", "domainnode_id": NG_CONTACT, "rangenode_id": CONTACT_ROLE, "graph_id": GRAPH}],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn building(id: &str, name: &str, lng: f64, lat: f64) -> serde_json::Value {
    let fc = json!({"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [lng, lat]}}]});
    json!({
        "resourceinstance": {"resourceinstanceid": id, "graph_id": GRAPH, "name": name, "legacyid": format!("legacy-{name}"),
            "descriptors": {"en": {"name": name, "description": "", "map_popup": ""}}},
        "tiles": [
            {"tileid": format!("{}0000-0000-4000-8000-0000000000f1", &id[..4]),
             "nodegroup_id": NG_PROFILE, "parenttile_id": null, "resourceinstance_id": id,
             "sortorder": 0, "provisionaledits": null, "data": {FOOTPRINT: fc}},
            // The contact tile — this is what the insurer head must NOT carry.
            {"tileid": format!("{}0000-0000-4000-8000-0000000000f2", &id[..4]),
             "nodegroup_id": NG_CONTACT, "parenttile_id": null, "resourceinstance_id": id,
             "sortorder": 0, "provisionaledits": null, "data": {CONTACT_ROLE: KEYHOLDER}},
        ],
    })
}

/// Emit a head; `tier` present → the scoped insurer head. Returns the out dir.
/// `tag` MUST be unique per call — each emit gets its own in/out dirs so parallel
/// tests (and a full-vs-tiered pair) never share or overwrite a directory.
fn emit(tag: &str, tier: Option<ros_madair_emit::TierFilter>) -> PathBuf {
    let dir = scratch(&format!("{tag}-in"));
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("building");
    std::fs::create_dir_all(&rdir).unwrap();
    let resources = vec![
        building(B1, "Covered One", 0.0, 0.0),
        building(B2, "Covered Two", 0.001, 0.001),
        building(B3, "Uncovered Three", 0.002, 0.002),
    ];
    std::fs::write(
        rdir.join("buildings.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    let out = scratch(&format!("{tag}-out"));
    ros_madair_emit::emit_parquet_with_progress(
        dir.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
        &default_registry(),
        &std::collections::HashMap::new(),
        tier.as_ref(),
        &mut |_| std::ops::ControlFlow::Continue(()),
    )
    .expect("emit");
    out
}

fn graph() -> StaticGraph {
    let dir = scratch("g");
    let gp = dir.join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    serde_json::from_value(raw["graph"][0].clone()).unwrap()
}

fn count(parquet: &Path, sql_where: &str) -> i64 {
    // A missing store file is itself proof of the field cut (no rows at all), so
    // treat absent-file as zero rather than panicking on read_parquet.
    if !parquet.exists() {
        return 0;
    }
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.query_row(
        &format!(
            "SELECT count(*) FROM read_parquet('{}') WHERE {sql_where}",
            parquet.display()
        ),
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn insurer_tier_head_cannot_leak() {
    let tier = ros_madair_emit::TierFilter {
        name: "insurer-book".into(),
        exclude_nodegroups: HashSet::from([NG_CONTACT.to_string()]),
        exclude_models: HashSet::new(),
        include_resources: Some(HashSet::from([B1.to_string(), B2.to_string()])),
    };
    let out = emit("leak", Some(tier));
    let tiles = out.join(format!("tiles_{GRAPH}.parquet"));
    let concepts = out.join(format!("concepts_{GRAPH}.parquet"));
    let resources = out.join(format!("resources_{GRAPH}.parquet"));
    let graph = graph();
    let duck = DuckReader::open(tiles.to_str().unwrap()).expect("open");

    // 1. ROW CUT — the head names ONLY the covered buildings; B3 is absent entirely.
    let all = duck
        .resolve_ids(
            &Query {
                model: GRAPH.into(),
                r#where: None,
                measures: vec![Measure::SelectIds],
                limit: None,
            },
            &graph,
            &default_registry(),
        )
        .unwrap();
    let got: HashSet<&str> = all.iter().map(String::as_str).collect();
    assert_eq!(
        got,
        HashSet::from([B1, B2]),
        "row cut: only covered buildings, not {got:?}"
    );
    assert_eq!(
        count(&resources, &format!("resource_id = '{B3}'")),
        0,
        "the uncovered building must not be in the entry table"
    );

    // 2. FIELD CUT — zero `contacts` rows in ANY store (tiles + melted concepts).
    assert_eq!(
        count(&tiles, &format!("nodegroup_id = '{NG_CONTACT}'")),
        0,
        "no contact tile rows in the insurer head"
    );
    assert_eq!(
        count(&concepts, &format!("node_id = '{NG_CONTACT}'")),
        0,
        "no contact concept rows in the insurer head"
    );
    // And the keyholder value is nowhere in the concept store at all.
    assert_eq!(
        count(&concepts, &format!("concept_id = '{KEYHOLDER}'")),
        0,
        "the keyholder concept value must not appear"
    );

    // 3. DECLARATION — manifest.tier names what it dropped (declaration == reality).
    let manifest: ros_madair_format::Manifest =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    let t = manifest.tier.expect("tiered head must carry manifest.tier");
    assert_eq!(t.name, "insurer-book");
    assert!(
        t.exclude_nodegroups.contains(&NG_CONTACT.to_string()),
        "manifest must declare the excluded nodegroup"
    );

    // 4. BEHAVIORAL OVER-REACH — a query for the contact field must surface NOTHING:
    // either an empty result (the field's rows are absent) or a typed refusal (the
    // field's store was dropped entirely). Both are honest denials; only returning
    // data would be a leak.
    let over_reach = duck.resolve_ids(
        &Query {
            model: GRAPH.into(),
            r#where: Some(Expr::Concept {
                path: "keyholder".into(),
                op: ConceptOp::Is,
                value: KEYHOLDER.into(),
            }),
            measures: vec![Measure::SelectIds],
            limit: None,
        },
        &graph,
        &default_registry(),
    );
    assert!(
        over_reach.as_ref().map(|v| v.is_empty()).unwrap_or(true),
        "over-reach for a contact must return nothing or be refused, got {over_reach:?}"
    );
}

#[test]
fn tier_moves_the_snapshot_id() {
    // Tamper-evidence: the tier is inside the signed snapshot_id, so a full head and a
    // tiered head of the same data differ — you cannot add the contacts back without
    // breaking the owner's signature.
    let full = emit("full", None);
    let tiered = emit(
        "tiered",
        Some(ros_madair_emit::TierFilter {
            name: "insurer-book".into(),
            exclude_nodegroups: HashSet::from([NG_CONTACT.to_string()]),
            exclude_models: HashSet::new(),
            include_resources: Some(HashSet::from([B1.to_string(), B2.to_string()])),
        }),
    );
    let id = |d: &Path| -> String {
        let m: ros_madair_format::Manifest =
            serde_json::from_slice(&std::fs::read(d.join("manifest.json")).unwrap()).unwrap();
        m.snapshot_id
    };
    assert_ne!(
        id(&full),
        id(&tiered),
        "a tiered head must have a different snapshot_id than the full head"
    );
    // The full head carries no tier; the tiered head does.
    let full_m: ros_madair_format::Manifest =
        serde_json::from_slice(&std::fs::read(full.join("manifest.json")).unwrap()).unwrap();
    assert!(
        full_m.tier.is_none(),
        "an untiered emit must write tier: null"
    );
}
