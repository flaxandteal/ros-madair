// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proof that hydrate resolves a `reference` field to its label.
//!
//! The chain under test, exactly as the duck read path runs it:
//!
//! 1. a `uuid -> label` map (in production, `DuckReader::concept_labels` reads it
//!    from the concept catalog; here it is supplied directly) →
//! 2. `hydrate_tiles_with_labels` renders in Display mode, so the `reference`
//!    node routes through the CLM handler in the registry, which resolves the
//!    bare list-item UUID via the map →
//! 3. the tree carries the LABEL, not the UUID.
//!
//! Tile *recovery from Parquet* is skipped (irrelevant to resolution); tiles are
//! handed in directly, which is what `hydrate_tiles_with_labels` is for.
//!
//! Why a `reference` field and not `concept`: a `reference` tile stores the bare
//! concept-id, which IS a label-map key. A `concept`/`concept-list` tile stores a
//! pref-label *value-id* (see the emitter's `walk_concepts`), which is not a
//! label-map key — reference is the case the label map actually supports.

use std::collections::HashMap;

use alizarin_core::graph::StaticGraph;
use alizarin_core::StaticTile;
use ros_madair_duck::hydrate_tiles_with_labels;

const LICENCE_UUID: &str = "9239cbe9-9571-4e5a-9c7f-1b2c3d4e5f60";
const RID: &str = "11111111-1111-1111-1111-111111111111";

/// A minimal model: root → one cardinality-1 `reference` node aliased `licence`.
fn reference_graph() -> StaticGraph {
    let mut graph: StaticGraph = serde_json::from_value(serde_json::json!({
        "graphid": "g",
        "name": {"en": "G"},
        "root": {"nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "g"},
        "nodes": [
            {"nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "g"},
            {"nodeid": "n_lic", "name": "Licence", "alias": "licence", "datatype": "reference",
             "nodegroup_id": "ng1", "graph_id": "g", "config": {"controlledList": "coll-1"}}
        ],
        "nodegroups": [{"nodegroupid": "ng1", "cardinality": "1"}],
        "edges": [{"edgeid": "e1", "domainnode_id": "root", "rangenode_id": "n_lic",
                   "graph_id": "g"}]
    }))
    .expect("graph");
    graph.build_indices();
    graph
}

/// One tile for `ng1`, the `reference` node set to the bare list-item UUID —
/// the shape the substrate stores (a `reference` tile is a bare concept-id, or
/// array of them, not a resolved object).
fn licence_tile() -> StaticTile {
    serde_json::from_value(serde_json::json!({
        "nodegroup_id": "ng1",
        "resourceinstance_id": RID,
        "tileid": "22222222-2222-2222-2222-222222222222",
        "data": { "n_lic": [LICENCE_UUID] }
    }))
    .expect("tile")
}

#[test]
fn a_reference_field_hydrates_to_its_label() {
    // The label map the concept catalog would supply: list-item UUID → label.
    let labels: HashMap<String, String> = [(LICENCE_UUID.to_string(), "CC BY-SA 4.0".to_string())]
        .into_iter()
        .collect();

    let graph = reference_graph();
    let tree = hydrate_tiles_with_labels(&[licence_tile()], RID, &graph, &labels, &["en"])
        .expect("hydrate");

    assert_eq!(
        tree["licence"], "CC BY-SA 4.0",
        "the reference field must hydrate as its label, not the raw UUID; got tree={tree}"
    );
}

/// The control: with no label map, the SAME path leaves the reference as its raw
/// UUID — proving it is the label-driven resolution, not something else, that
/// produced the label above.
#[test]
fn without_the_label_map_the_reference_stays_a_uuid() {
    let graph = reference_graph();
    let empty = HashMap::new();
    let tree = hydrate_tiles_with_labels(&[licence_tile()], RID, &graph, &empty, &["en"])
        .expect("hydrate");
    assert_eq!(
        tree["licence"], LICENCE_UUID,
        "with no label for the UUID, the handler falls back to the id itself; got tree={tree}"
    );
}

/// The language preference chain threads all the way through hydrate for i18n
/// `string` fields: `gd` absent → fall to `ga` (the next preference), NOT `en`
/// and NOT an arbitrary map pick. Proves the sequence reaches `serialize_string`.
#[test]
fn the_language_chain_threads_through_hydrate() {
    let mut graph: StaticGraph = serde_json::from_value(serde_json::json!({
        "graphid": "g",
        "name": {"en": "G"},
        "root": {"nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "g"},
        "nodes": [
            {"nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "g"},
            {"nodeid": "n_t", "name": "Title", "alias": "title", "datatype": "string",
             "nodegroup_id": "ng1", "graph_id": "g"}
        ],
        "nodegroups": [{"nodegroupid": "ng1", "cardinality": "1"}],
        "edges": [{"edgeid": "e1", "domainnode_id": "root", "rangenode_id": "n_t",
                   "graph_id": "g"}]
    }))
    .expect("graph");
    graph.build_indices();

    let tile: StaticTile = serde_json::from_value(serde_json::json!({
        "nodegroup_id": "ng1",
        "resourceinstance_id": RID,
        "tileid": "33333333-3333-3333-3333-333333333333",
        "data": { "n_t": {"ga": "Dia dhuit", "en": "Hello"} }
    }))
    .expect("tile");

    let empty = HashMap::new();
    let tree = hydrate_tiles_with_labels(&[tile], RID, &graph, &empty, &["gd", "ga", "en"])
        .expect("hydrate");
    assert_eq!(
        tree["title"], "Dia dhuit",
        "the chain must prefer ga over en when gd is absent; got tree={tree}"
    );
}
