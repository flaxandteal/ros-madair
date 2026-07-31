// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end: the real emitter writes a real snapshot, this crate reads it
//! back. No hand-built head — a test that re-declares the head schema is just
//! a fourth copy of it, and it would keep passing while the emitter moved.
//!
//! The demo fixture carries FOUR models (institution, person, session, talk),
//! so the multi-model case comes free: `spine_institution` is the first spine
//! table, and a resource from any other model is precisely what the old
//! `hydrate_entry.rs` (`… WHERE name LIKE 'spine_%' LIMIT 1`) could not find.
//!
//! The fixture lives outside the repo (Clódóir's demo data); point
//! `ROS_MADAIR_DEMO_DATA` elsewhere to move it. Absent → the test skips.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";

/// The institution model: "Company A" — i18n name + a GeoJSON location.
const INSTITUTION_GRAPH: &str = "0271fdde-e6c7-457c-b25a-e65a11aba499";
const COMPANY_A: &str = "fb8115bb-ba4e-48f7-993c-35071d29ae30";
/// The talk model — the LAST spine table, alphabetically and by creation.
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK: &str = "97cc9a1b-ee42-412e-9fa5-203d98bff815";

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()),
    );
    dir.join("graphs").is_dir().then_some(dir)
}

/// Emit the demo fixture once per test into its own temp dir.
fn emit_demo(tag: &str) -> Option<(PathBuf, PathBuf)> {
    let data = demo_data()?;
    let out = std::env::temp_dir().join(format!("rm-read-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    ros_madair_emit::emit(
        data.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
    )
    .expect("demo emit");
    Some((out, data))
}

fn graph(data: &Path, graph_id: &str) -> StaticGraph {
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(data.join("graphs").join(format!("{graph_id}.json"))).unwrap(),
    )
    .unwrap();
    let value = raw
        .get("graph")
        .and_then(|g| g.get(0))
        .cloned()
        .unwrap_or(raw);
    let mut graph: StaticGraph = serde_json::from_value(value).unwrap();
    graph.build_indices();
    graph
}

/// The reference hydration: emit → hydrate → the entry comes back whole
/// (i18n string, GeoJSON feature collection, identity fields).
#[test]
fn hydrates_company_a_from_a_freshly_emitted_snapshot() {
    let Some((head, data)) = emit_demo("company-a") else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let graph = graph(&data, INSTITUTION_GRAPH);

    // The fragment_dir cross-check: we recovered every tile the head says the
    // resource has, and none of the co-resident tiles from other resources
    // sharing the chunk.
    let tiles = ros_madair_read::resource_tiles_with_graph(&head, COMPANY_A, Some(&graph)).unwrap();
    let expected = ros_madair_read::expected_tile_count(&head, COMPANY_A).unwrap();
    assert_eq!(tiles.len() as i64, expected, "tile count vs fragment_dir");
    assert!(tiles.iter().all(|t| t.resourceinstance_id == COMPANY_A));

    let tree = ros_madair_read::hydrate_resource(&head, COMPANY_A, &graph, &["en"]).unwrap();
    assert_eq!(tree["resourceinstanceid"], COMPANY_A);
    assert_eq!(tree["graph_id"], INSTITUTION_GRAPH);
    // Display tree: the i18n string is flattened to its "en" value (no language
    // map, no direction) — consumers read the value directly.
    assert_eq!(tree["name"], "Company A");
    assert_eq!(tree["location"]["type"], "FeatureCollection");
    assert_eq!(tree["location"]["features"][0]["geometry"]["type"], "Point");
    assert!(tree["location"]["features"][0]["geometry"]["coordinates"]
        .as_array()
        .is_some_and(|c| c.len() == 2));
}

/// The multi-model regression: a resource in the LAST spine table. Under the
/// old single-spine lookup this resolved to "no rows" — the reader simply
/// could not see three of the four models.
#[test]
fn hydrates_a_resource_from_a_non_first_spine_table() {
    let Some((head, data)) = emit_demo("talk") else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    // The head really is multi-model, and institution really is first.
    let manifest = ros_madair_read::load_manifest(&head).unwrap().unwrap();
    assert_eq!(manifest.models.len(), 4);
    assert_eq!(manifest.models[0].spine_table, "spine_institution");
    assert_eq!(
        manifest.model_for_graph(TALK_GRAPH).unwrap().spine_table,
        "spine_talk"
    );

    let graph = graph(&data, TALK_GRAPH);
    let tree = ros_madair_read::hydrate_resource(&head, TALK, &graph, &["en"]).unwrap();
    assert_eq!(tree["resourceinstanceid"], TALK);
    assert_eq!(tree["graph_id"], TALK_GRAPH);
    // Display tree: i18n string flattened to its "en" value.
    assert_eq!(tree["title"], "Scripting for people who like papyrus");
    // Resource links survive the chunk round trip too.
    assert_eq!(tree["presenter"].as_array().unwrap().len(), 2);

    // …and with no graph hint at all (no manifest fast path, plain spine scan).
    let tiles = ros_madair_read::resource_tiles(&head, TALK).unwrap();
    assert_eq!(
        tiles.len() as i64,
        ros_madair_read::expected_tile_count(&head, TALK).unwrap()
    );
}

/// A UUID that is not a resource in this snapshot is a typed error, not a
/// stringly-typed sqlite "QueryReturnedNoRows".
#[test]
fn unknown_resource_is_typed() {
    let Some((head, _)) = emit_demo("unknown") else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let err = ros_madair_read::resource_tiles(&head, "not-a-resource").unwrap_err();
    assert!(
        matches!(err, ros_madair_read::ReadError::UnknownResource(ref u) if u == "not-a-resource"),
        "{err}"
    );
    assert!(err.to_string().contains("is not in this snapshot"));
}
