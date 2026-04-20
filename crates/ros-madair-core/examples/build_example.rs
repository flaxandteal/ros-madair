// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Build example index data for the RosMadair demo.
//!
//! Creates a small synthetic dataset with heritage places, people, and concepts,
//! then builds the page-based index.
//!
//! Run: `cargo run --example build_example`

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use ros_madair_core::{
    assign_pages, extract_centroid, graph_schema_to_triples, quantize_type_for_datatype,
    quantize_tile_value, resource_to_triples, serialize_summary, triples_to_ntriples,
    write_page_file, write_tile_content_file, Dictionary, PageConfig, PageRecord,
    PredicateBlock, ResourceMap, SummaryBuilder, page_assignment::ResourceSummary,
};
use ros_madair_core::uri::{resource_uri, node_uri};

use alizarin_core::graph::{StaticGraph, StaticTile};

fn main() {
    let output_dir = Path::new("example/static/ros-madair");
    fs::create_dir_all(output_dir.join("pages")).expect("Failed to create output dir");

    let base_uri = "https://example.org/";

    // Create a simple graph definition
    let graph_json = r#"{
        "graphid": "heritage-place",
        "name": {"en": "Heritage Place"},
        "nodes": [
            {"nodeid": "n-name", "alias": "name_value", "datatype": "string", "name": "Name", "graph_id": "heritage-place", "nodegroup_id": "ng-name", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": true, "issearchable": true, "istopnode": false},
            {"nodeid": "n-type", "alias": "monument_type", "datatype": "concept", "name": "Monument Type", "graph_id": "heritage-place", "nodegroup_id": "ng-type", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": true, "issearchable": true, "istopnode": false},
            {"nodeid": "n-geo", "alias": "geometry", "datatype": "geojson-feature-collection", "name": "Geometry", "graph_id": "heritage-place", "nodegroup_id": "ng-geo", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": true, "issearchable": true, "istopnode": false},
            {"nodeid": "n-date", "alias": "year_built", "datatype": "date", "name": "Year Built", "graph_id": "heritage-place", "nodegroup_id": "ng-date", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": true, "issearchable": true, "istopnode": false}
        ],
        "edges": [],
        "root": {"nodeid": "n-root", "name": "Heritage Place", "datatype": "semantic", "graph_id": "heritage-place", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": false, "issearchable": false, "istopnode": true},
        "nodegroups": [
            {"nodegroupid": "ng-name"},
            {"nodegroupid": "ng-type"},
            {"nodegroupid": "ng-geo"},
            {"nodegroupid": "ng-date"}
        ]
    }"#;

    let mut graph: StaticGraph = serde_json::from_str(graph_json).expect("Invalid graph JSON");
    graph.build_indices();

    // Synthetic resources
    let places = vec![
        ("hp-001", "Belfast Castle", "church", -5.87, 54.61, "1870-01-01"),
        ("hp-002", "Carrickfergus Castle", "castle", -5.81, 54.72, "1177-01-01"),
        ("hp-003", "St Anne's Cathedral", "church", -5.93, 54.60, "1899-01-01"),
        ("hp-004", "Dunluce Castle", "castle", -6.58, 55.21, "1500-01-01"),
        ("hp-005", "Grey Abbey", "church", -5.55, 54.53, "1193-01-01"),
        ("hp-006", "Hillsborough Fort", "fort", -6.08, 54.46, "1650-01-01"),
    ];

    let mut resources: Vec<(String, Vec<StaticTile>)> = Vec::new();
    for (id, name, concept, lng, lat, date) in &places {
        let mut name_data = HashMap::new();
        name_data.insert("n-name".to_string(), serde_json::json!({"en": name}));

        let mut type_data = HashMap::new();
        type_data.insert("n-type".to_string(), serde_json::json!(concept));

        let mut geo_data = HashMap::new();
        geo_data.insert(
            "n-geo".to_string(),
            serde_json::json!({"type": "Point", "coordinates": [lng, lat]}),
        );

        let mut date_data = HashMap::new();
        date_data.insert("n-date".to_string(), serde_json::json!(date));

        resources.push((
            id.to_string(),
            vec![
                StaticTile {
                    data: name_data,
                    nodegroup_id: "ng-name".to_string(),
                    resourceinstance_id: id.to_string(),
                    tileid: None,
                    parenttile_id: None,
                    provisionaledits: None,
                    sortorder: None,
                },
                StaticTile {
                    data: type_data,
                    nodegroup_id: "ng-type".to_string(),
                    resourceinstance_id: id.to_string(),
                    tileid: None,
                    parenttile_id: None,
                    provisionaledits: None,
                    sortorder: None,
                },
                StaticTile {
                    data: geo_data,
                    nodegroup_id: "ng-geo".to_string(),
                    resourceinstance_id: id.to_string(),
                    tileid: None,
                    parenttile_id: None,
                    provisionaledits: None,
                    sortorder: None,
                },
                StaticTile {
                    data: date_data,
                    nodegroup_id: "ng-date".to_string(),
                    resourceinstance_id: id.to_string(),
                    tileid: None,
                    parenttile_id: None,
                    provisionaledits: None,
                    sortorder: None,
                },
            ],
        ));
    }

    // Build summaries
    let summaries: Vec<ResourceSummary> = resources
        .iter()
        .map(|(id, tiles)| {
            let mut centroid = None;
            let mut concept_ids = Vec::new();
            for tile in tiles {
                for (node_id, value) in &tile.data {
                    if node_id == "n-geo" {
                        centroid = extract_centroid(&value.to_string());
                    }
                    if node_id == "n-type" {
                        if let Some(s) = value.as_str() {
                            concept_ids.push(s.to_string());
                        }
                    }
                }
            }
            ResourceSummary {
                resource_id: id.clone(),
                graph_id: "heritage-place".to_string(),
                centroid,
                concept_ids,
            }
        })
        .collect();

    let page_index = assign_pages(&summaries, &PageConfig { target_page_size: 3 });

    // Build page records + summary
    let mut dict = Dictionary::new();
    let mut summary_builder = SummaryBuilder::new();
    let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();

    for (resource_id, tiles) in &resources {
        let page_id = page_index.resource_to_page[resource_id.as_str()];
        let subject_id = dict.intern(&resource_uri(base_uri, resource_id));

        for tile in tiles {
            for (node_id, value) in &tile.data {
                if value.is_null() {
                    continue;
                }
                let node = match graph.get_node_by_id(node_id) {
                    Some(n) => n,
                    None => continue,
                };
                let alias = match &node.alias {
                    Some(a) => a.as_str(),
                    None => continue,
                };
                let qtype = match quantize_type_for_datatype(&node.datatype) {
                    Some(qt) => qt,
                    None => continue,
                };
                let pred_id = dict.intern(&node_uri(base_uri, alias));

                let object_vals = quantize_tile_value(
                    value, qtype, &node.datatype, &mut dict, base_uri,
                );

                for object_val in object_vals {
                    let record = PageRecord {
                        subject_id,
                        object_val,
                    };
                    page_records
                        .entry(page_id)
                        .or_default()
                        .push((pred_id, record));
                    summary_builder.add(page_id, pred_id, object_val, subject_id);
                }
            }
        }
    }

    // Write outputs
    let summary_bytes = serialize_summary(&summary_builder.build());
    fs::write(output_dir.join("summary.bin"), &summary_bytes).unwrap();

    let dict_bytes = dict.to_bytes();
    fs::write(output_dir.join("dictionary.bin"), &dict_bytes).unwrap();

    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    fs::write(output_dir.join("resource_map.bin"), &resource_map.to_bytes()).unwrap();

    let page_meta_json = serde_json::to_string_pretty(&page_index.page_meta).unwrap();
    fs::write(output_dir.join("page_meta.json"), &page_meta_json).unwrap();

    for pm in &page_index.page_meta {
        if let Some(records) = page_records.get(&pm.page_id) {
            let mut by_pred: HashMap<u32, Vec<PageRecord>> = HashMap::new();
            for &(pred_id, record) in records {
                by_pred.entry(pred_id).or_default().push(record);
            }
            let mut blocks: Vec<PredicateBlock> = by_pred
                .into_iter()
                .map(|(pred_id, mut recs)| {
                    recs.sort();
                    PredicateBlock {
                        pred_id,
                        records: recs,
                    }
                })
                .collect();
            let page_bytes = write_page_file(&mut blocks, &[]);
            fs::write(
                output_dir.join(format!("pages/page_{:04}.dat", pm.page_id)),
                &page_bytes,
            )
            .unwrap();
        }
    }

    // RDF export
    let mut all_triples = Vec::new();
    if let Ok(schema_triples) = graph_schema_to_triples(&graph, base_uri) {
        all_triples.extend(schema_triples);
    }
    for (resource_id, tiles) in &resources {
        if let Ok(triples) = resource_to_triples(&graph, resource_id, tiles, base_uri) {
            all_triples.extend(triples);
        }
    }
    fs::write(output_dir.join("all.nt"), triples_to_ntriples(&all_triples)).unwrap();

    // Tile content files
    fs::create_dir_all(output_dir.join("tiles")).expect("Failed to create tiles dir");

    // Group resources by page, serialize tiles as MessagePack
    let mut page_tile_entries: HashMap<u32, Vec<(u32, Vec<u8>)>> = HashMap::new();
    for (resource_id, tiles) in &resources {
        let page_id = page_index.resource_to_page[resource_id.as_str()];
        let subject_id = dict.lookup(&resource_uri(base_uri, resource_id)).unwrap();
        let blob = rmp_serde::to_vec_named(tiles).expect("Failed to serialize tiles");
        page_tile_entries.entry(page_id).or_default().push((subject_id, blob));
    }

    for (page_id, mut entries) in page_tile_entries {
        entries.sort_by_key(|(sid, _)| *sid);
        let tile_bytes = write_tile_content_file(&entries);
        fs::write(
            output_dir.join(format!("tiles/tile_{:04}.dat", page_id)),
            &tile_bytes,
        )
        .unwrap();
    }

    // Write graph definition for the HTML demo
    fs::write(output_dir.join("graph.json"), graph_json).unwrap();

    println!("Built index with {} resources, {} pages", resources.len(), page_index.page_meta.len());
    println!("Dictionary: {} terms", dict.len());
    println!("Output: {}", output_dir.display());
}
