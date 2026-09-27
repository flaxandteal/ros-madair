// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tour of the DuckDB+Parquet substrate, self-contained — no data
//! files, no network. Run it:
//!
//! ```bash
//! cargo run -p ros-madair-duck --example substrate_end_to_end
//! ```
//!
//! It walks the whole flow:
//!   1. write a tiny alizarin data_dir (a `Talk` model: a `founded` date node and
//!      a `location` geojson node, over four resources);
//!   2. `emit_parquet` it to the substrate (tiles/edges/catalog + manifest);
//!   3. `seal_and_sign` the snapshot (attest its `snapshot_id`);
//!   4. `verify_snapshot` — the reader-side gate (recompute the id, check the
//!      signature) → Verified;
//!   5. open it with `DuckReader` (which itself refuses a tampered layer);
//!   6. compile a couple of `Query` IR predicates → matching resource ids;
//!   7. hydrate one match into a schema-shaped JSON tree.
//!
//! Everything the CLAUDE.md / README "data flow" describes, in ~100 lines.

use std::collections::HashMap;
use std::path::Path;

use alizarin_core::graph::StaticGraph;
use alizarin_core::quantize::quantize_date;
use ros_madair_duck::{verify_snapshot, DuckReader};
use ros_madair_format::attest::HeadTrust;
use ros_madair_query::{Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "5efd0000-0000-4000-8000-0000000000aa";
const ROOT: &str = "5efd0000-0000-4000-8000-0000000000a0";
const FOUNDED_NG: &str = "5efd0000-0000-4000-8000-000000000001";
const GEO_NG: &str = "5efd0000-0000-4000-8000-000000000002";

const R_POINT: &str = "11110000-0000-4000-8000-000000000001";
const R_LSHAPE: &str = "22220000-0000-4000-8000-000000000001";
const R_FAR: &str = "33330000-0000-4000-8000-000000000001";
const R_DIAG: &str = "44440000-0000-4000-8000-000000000001";

/// The `Talk` model as one graph doc: root → `founded` (date) + `location` (geo).
fn graph_doc() -> serde_json::Value {
    let root = json!({
        "nodeid": ROOT, "name": "Talk", "alias": "talk", "datatype": "semantic",
        "graph_id": GRAPH, "istopnode": true,
    });
    json!({ "graph": [{
        "graphid": GRAPH, "name": "Talk", "root": root.clone(),
        "nodes": [
            root,
            { "nodeid": FOUNDED_NG, "nodegroup_id": FOUNDED_NG, "name": "Founded",
              "alias": "founded", "datatype": "date", "graph_id": GRAPH,
              "is_collector": true, "issearchable": true },
            { "nodeid": GEO_NG, "nodegroup_id": GEO_NG, "name": "Location",
              "alias": "location", "datatype": "geojson-feature-collection",
              "graph_id": GRAPH, "is_collector": true, "issearchable": true },
        ],
        "nodegroups": [
            { "nodegroupid": FOUNDED_NG, "cardinality": "1", "parentnodegroup_id": null },
            { "nodegroupid": GEO_NG, "cardinality": "1", "parentnodegroup_id": null },
        ],
        "edges": [
            { "edgeid": "5efd0000-0000-4000-8000-0000000000ee",
              "domainnode_id": ROOT, "rangenode_id": FOUNDED_NG, "graph_id": GRAPH },
            { "edgeid": "5efd0000-0000-4000-8000-0000000000ef",
              "domainnode_id": ROOT, "rangenode_id": GEO_NG, "graph_id": GRAPH },
        ],
    }]})
}

/// One resource: a `location` geometry and a `founded` date.
fn talk(id: &str, geometry: serde_json::Value, founded: &str) -> serde_json::Value {
    let fc = json!({
        "type": "FeatureCollection",
        "features": [{ "type": "Feature", "properties": {}, "geometry": geometry }],
    });
    json!({
        "resourceinstance": {
            "resourceinstanceid": id, "graph_id": GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } },
        },
        "tiles": [
            { "tileid": format!("{id}-geo"), "nodegroup_id": GEO_NG, "parenttile_id": null,
              "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
              "data": { GEO_NG: fc } },
            { "tileid": format!("{id}-date"), "nodegroup_id": FOUNDED_NG, "parenttile_id": null,
              "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
              "data": { FOUNDED_NG: founded } },
        ],
    })
}

/// Write the data_dir (`graphs/*.json`, `resources/**/*.json`) the emitter reads.
fn write_data_dir(dir: &Path) {
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    std::fs::write(&gp, serde_json::to_vec_pretty(&graph_doc()).unwrap()).unwrap();

    let resources = json!({ "business_data": { "resources": [
        talk(R_POINT,  json!({ "type": "Point", "coordinates": [0.0, 0.0] }),   "2005-06-01"),
        talk(R_LSHAPE, json!({ "type": "Polygon", "coordinates": [[[0.5,0.5],[10.0,0.5],[10.0,2.0],[0.5,2.0],[0.5,0.5]]] }), "2018-03-15"),
        talk(R_FAR,    json!({ "type": "Point", "coordinates": [100.0, 100.0] }), "1850-01-01"),
        talk(R_DIAG,   json!({ "type": "LineString", "coordinates": [[0.5,5.0],[5.0,0.5]] }), "2021-07-01"),
    ] } });
    let rd = dir.join("resources").join("talk");
    std::fs::create_dir_all(&rd).unwrap();
    std::fs::write(
        rd.join("talks.json"),
        serde_json::to_vec_pretty(&resources).unwrap(),
    )
    .unwrap();
}

fn load_graph(dir: &Path) -> StaticGraph {
    let bytes = std::fs::read(dir.join("graphs").join(format!("{GRAPH}.json"))).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut g: StaticGraph = serde_json::from_value(doc["graph"][0].clone()).unwrap();
    g.build_indices();
    g
}

fn main() {
    let base = std::env::temp_dir().join(format!("rm-example-{}", std::process::id()));
    let data_dir = base.join("data");
    let out_dir = base.join("snapshot");
    let _ = std::fs::remove_dir_all(&base);

    // 1. + 2. write a data_dir and compile it to the Parquet substrate.
    write_data_dir(&data_dir);
    let registry = ros_madair_handlers::default_registry();
    let cfg: HashMap<String, ros_madair_emit::ClusterConfig> = HashMap::new();
    let summaries = ros_madair_emit::emit_parquet(
        data_dir.to_str().unwrap(),
        out_dir.to_str().unwrap(),
        "https://example.org/",
        &registry,
        &cfg,
    )
    .expect("emit_parquet");
    let s = &summaries[0];
    println!(
        "emitted '{}': {} resources, {} tiles, {} row groups → {}",
        s.slug,
        s.resources,
        s.tiles,
        s.row_groups,
        out_dir.display()
    );

    // 3. sign, 4. verify (the reader-side gate).
    let key = base.join("signing.key");
    let id = ros_madair_emit::seal_and_sign(&out_dir, &key, None).expect("sign");
    println!("signed snapshot_id = {id}");
    match verify_snapshot(&out_dir).expect("verify") {
        HeadTrust::Verified { authored, .. } => {
            println!("verified: {authored} attestation(s)")
        }
        other => panic!("expected Verified, got {other:?}"),
    }

    // 5. open (open_layers itself refuses a tampered layer; this one verifies).
    let reader = DuckReader::open_layers(&[out_dir.as_path()]).expect("open_layers");
    // The schema is not carried in the snapshot; a consumer ships the graph
    // alongside it. Here we load the same graph JSON the emitter read.
    let graph = load_graph(&data_dir);

    // 6. compile Query IR predicates → matching resource ids.
    let count = |w: &Expr| Query {
        model: "talk".into(),
        r#where: Some(w.clone()),
        measures: vec![Measure::SelectIds],
        limit: None,
    };

    // Range: talks founded 2000..2025 (dates are quantized to an ordered i64).
    let founded_recent = Expr::Range {
        path: "founded".into(),
        lo: quantize_date("2000-01-01").unwrap(),
        hi: quantize_date("2025-12-31").unwrap(),
    };
    let recent = reader
        .resolve_ids(&count(&founded_recent), &graph, &registry)
        .unwrap();
    println!(
        "\nfounded in 2000..2025 → {} match(es): {recent:?}",
        recent.len()
    );

    // Bbox: talks whose geometry meets the unit-ish box near the origin. Opened
    // without the spatial extension, so this prunes coarsely on the geo_* zone-map
    // (a superset a caller would verify exactly on the hydrated tile).
    let near_origin = Expr::Bbox {
        path: "location".into(),
        min_lng: -1.0,
        min_lat: -1.0,
        max_lng: 12.0,
        max_lat: 12.0,
    };
    let near = reader
        .resolve_ids(&count(&near_origin), &graph, &registry)
        .unwrap();
    println!(
        "near the origin (coarse bbox) → {} match(es): {near:?}",
        near.len()
    );

    // 7. hydrate one match into a schema-shaped JSON tree.
    if let Some(uuid) = recent.first() {
        let tree = reader.hydrate(uuid, &graph, &["en"]).unwrap();
        println!(
            "\nhydrated {uuid}:\n{}",
            serde_json::to_string_pretty(&tree).unwrap()
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}
