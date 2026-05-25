// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Integration test: build the synthetic example index and verify queries.
//!
//! This exercises the full build → query roundtrip using the same dataset as
//! `build_example`, ensuring that:
//!   - dictionary, summary, page files, and resource map are well-formed
//!   - queries return expected result counts
//!   - page header parsing works on small files (< 1024 bytes)
//!
//! If this test fails, the example HTML pages are likely broken too.

use std::collections::{HashMap, HashSet};
use std::fs;

use ros_madair_core::{
    assign_pages, extract_centroid, quantize_type_for_datatype, quantize_tile_value,
    parse_page_header, parse_records, binary_search_object,
    serialize_summary, write_page_file, write_tile_content_file,
    Dictionary, PageConfig, PageRecord, PredicateBlock, ResourceMap, SummaryBuilder, SummaryIndex,
    page_assignment::ResourceSummary,
};
use ros_madair_core::uri::{resource_uri, node_uri};

use alizarin_core::graph::{StaticGraph, StaticTile};

fn build_example_index(dir: &std::path::Path) -> Dictionary {
    let base_uri = "https://example.org/";

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

    let mut graph: StaticGraph = serde_json::from_str(graph_json).unwrap();
    graph.build_indices();

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
        let tiles = vec![
            StaticTile {
                data: [("n-name".into(), serde_json::json!({"en": name}))].into(),
                nodegroup_id: "ng-name".into(),
                resourceinstance_id: id.to_string(),
                tileid: None, parenttile_id: None, provisionaledits: None, sortorder: None,
            },
            StaticTile {
                data: [("n-type".into(), serde_json::json!(concept))].into(),
                nodegroup_id: "ng-type".into(),
                resourceinstance_id: id.to_string(),
                tileid: None, parenttile_id: None, provisionaledits: None, sortorder: None,
            },
            StaticTile {
                data: [("n-geo".into(), serde_json::json!({"type": "Point", "coordinates": [lng, lat]}))].into(),
                nodegroup_id: "ng-geo".into(),
                resourceinstance_id: id.to_string(),
                tileid: None, parenttile_id: None, provisionaledits: None, sortorder: None,
            },
            StaticTile {
                data: [("n-date".into(), serde_json::json!(date))].into(),
                nodegroup_id: "ng-date".into(),
                resourceinstance_id: id.to_string(),
                tileid: None, parenttile_id: None, provisionaledits: None, sortorder: None,
            },
        ];
        resources.push((id.to_string(), tiles));
    }

    let summaries: Vec<ResourceSummary> = resources.iter().map(|(id, tiles)| {
        let mut centroid = None;
        let mut concept_ids = Vec::new();
        for tile in tiles {
            for (node_id, value) in &tile.data {
                if node_id == "n-geo" { centroid = extract_centroid(&value.to_string()); }
                if node_id == "n-type" {
                    if let Some(s) = value.as_str() { concept_ids.push(s.to_string()); }
                }
            }
        }
        ResourceSummary { resource_id: id.clone(), graph_id: "heritage-place".into(), centroid, concept_ids }
    }).collect();

    let page_index = assign_pages(&summaries, &PageConfig { target_page_size: 3 });

    let mut dict = Dictionary::new();
    let mut summary_builder = SummaryBuilder::new();
    let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();

    for (resource_id, tiles) in &resources {
        let page_id = page_index.resource_to_page[resource_id.as_str()];
        let subject_id = dict.intern(&resource_uri(base_uri, resource_id));
        for tile in tiles {
            for (node_id, value) in &tile.data {
                if value.is_null() { continue; }
                let node = match graph.get_node_by_id(node_id) { Some(n) => n, None => continue };
                let alias = match &node.alias { Some(a) => a.as_str(), None => continue };
                let qtype = match quantize_type_for_datatype(&node.datatype) { Some(qt) => qt, None => continue };
                let pred_id = dict.intern(&node_uri(base_uri, alias));
                let object_vals = quantize_tile_value(value, qtype, &node.datatype, &mut dict, base_uri, None);
                for object_val in object_vals {
                    let record = PageRecord { subject_id, object_val };
                    page_records.entry(page_id).or_default().push((pred_id, record));
                    summary_builder.add(page_id, pred_id, object_val, subject_id);
                }
            }
        }
    }

    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::write(dir.join("summary.bin"), serialize_summary(&summary_builder.build())).unwrap();
    fs::write(dir.join("dictionary.bin"), dict.to_bytes()).unwrap();

    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    fs::write(dir.join("resource_map.bin"), resource_map.to_bytes()).unwrap();
    fs::write(dir.join("page_meta.json"), serde_json::to_string(&page_index.page_meta).unwrap()).unwrap();

    for pm in &page_index.page_meta {
        if let Some(records) = page_records.get(&pm.page_id) {
            let mut by_pred: HashMap<u32, Vec<PageRecord>> = HashMap::new();
            for &(pred_id, record) in records { by_pred.entry(pred_id).or_default().push(record); }
            let mut blocks: Vec<PredicateBlock> = by_pred.into_iter()
                .map(|(pred_id, mut recs)| { recs.sort(); PredicateBlock { pred_id, records: recs } })
                .collect();
            fs::write(dir.join(format!("pages/page_{:04}.dat", pm.page_id)), write_page_file(&mut blocks, &[])).unwrap();
        }
    }

    // Write tile content files
    fs::create_dir_all(dir.join("tiles")).unwrap();
    let mut page_tile_entries: HashMap<u32, Vec<(u32, Vec<u8>)>> = HashMap::new();
    for (resource_id, tiles) in &resources {
        let page_id = page_index.resource_to_page[resource_id.as_str()];
        let subject_id = dict.lookup(&resource_uri(base_uri, resource_id)).unwrap();
        let blob = rmp_serde::to_vec_named(tiles).unwrap();
        page_tile_entries.entry(page_id).or_default().push((subject_id, blob));
    }
    for (page_id, mut entries) in page_tile_entries {
        entries.sort_by_key(|(sid, _)| *sid);
        fs::write(dir.join(format!("tiles/tile_{:04}.dat", page_id)), write_tile_content_file(&entries)).unwrap();
    }

    dict
}

fn run_query(index_dir: &std::path::Path, dict: &Dictionary, patterns: &[(&str, &str)]) -> usize {
    let base = index_dir.to_str().unwrap();
    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();

    let resolved: Vec<(u32, u32)> = patterns.iter().filter_map(|(p, o)| {
        Some((dict.lookup(p)?, dict.lookup(o)?))
    }).collect();

    if resolved.len() != patterns.len() { return 0; }

    let mut page_preds: HashMap<u32, HashSet<u32>> = HashMap::new();
    for &(pred_id, obj_id) in &resolved {
        for q in summary.lookup_op(obj_id, pred_id) {
            page_preds.entry(q.page_s).or_default().insert(pred_id);
        }
    }

    let mut result_sets: Vec<HashSet<u32>> = Vec::new();
    for &(pred_id, obj_id) in &resolved {
        let mut matches = HashSet::new();
        for (&page_id, preds) in &page_preds {
            if !preds.contains(&pred_id) { continue; }
            let data = fs::read(format!("{base}/pages/page_{:04}.dat", page_id)).unwrap();
            let header = parse_page_header(&data).unwrap();
            if let Some((start, end)) = header.predicate_byte_range(pred_id) {
                let recs = parse_records(&data[start as usize..end as usize]);
                let (lo, hi) = binary_search_object(&recs, obj_id);
                for rec in &recs[lo..hi] { matches.insert(rec.subject_id); }
            }
        }
        result_sets.push(matches);
    }

    if result_sets.is_empty() { return 0; }
    let mut iter = result_sets.into_iter();
    let mut intersection = iter.next().unwrap();
    for set in iter { intersection = intersection.intersection(&set).copied().collect(); }
    intersection.len()
}

#[test]
fn example_build_and_query_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path();
    let dict = build_example_index(index_dir);

    let n = "https://example.org/node/";
    let c = "https://example.org/concept/";

    // Churches: hp-001, hp-003, hp-005
    let church_uri = format!("{c}church");
    let monument_pred = format!("{n}monument_type");
    let count = run_query(index_dir, &dict, &[(&monument_pred, &church_uri)]);
    assert_eq!(count, 3, "Expected 3 churches");

    // Castles: hp-002, hp-004
    let castle_uri = format!("{c}castle");
    let count = run_query(index_dir, &dict, &[(&monument_pred, &castle_uri)]);
    assert_eq!(count, 2, "Expected 2 castles");

    // Forts: hp-006
    let fort_uri = format!("{c}fort");
    let count = run_query(index_dir, &dict, &[(&monument_pred, &fort_uri)]);
    assert_eq!(count, 1, "Expected 1 fort");
}

#[test]
fn page_files_parseable_even_when_small() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path();
    build_example_index(index_dir);

    // With page_size=3 and 6 resources, we get 2 pages.
    // Each page file should be well under 1024 bytes.
    for entry in fs::read_dir(index_dir.join("pages")).unwrap() {
        let path = entry.unwrap().path();
        let data = fs::read(&path).unwrap();
        assert!(data.len() < 1024, "Page file {} is {} bytes — expected < 1024 for this test dataset",
            path.display(), data.len());
        let header = parse_page_header(&data).unwrap();
        assert!(!header.entries.is_empty(), "Page {} has no predicates", path.display());
    }
}

#[test]
fn dictionary_has_magic_header() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path();
    build_example_index(index_dir);

    let data = fs::read(index_dir.join("dictionary.bin")).unwrap();
    assert_eq!(&data[0..4], b"RMDC", "Dictionary should start with RMDC magic");
}

#[test]
fn resource_map_has_magic_header() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path();
    build_example_index(index_dir);

    let data = fs::read(index_dir.join("resource_map.bin")).unwrap();
    assert_eq!(&data[0..4], b"RMRM", "Resource map should start with RMRM magic");
}
