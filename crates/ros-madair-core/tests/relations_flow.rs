// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Integration test that mimics the starches `relations.ts` flow:
//!
//!   1. page_for_resource(uri) → page_id
//!   2. summary_from_page(page_id) → quads
//!   3. filter quads to resource-link predicates (page_o is a valid page)
//!   4. load_predicate_records(page_id, pred) → records
//!   5. filter records where subject_id == target dict_id
//!   6. check object resolves to a /resource/ URI
//!
//! This exercises the full chain that starches uses to discover related resources.

use std::collections::{HashMap, HashSet};
use std::fs;

use ros_madair_core::{
    assign_pages, parse_page_header, parse_records,
    serialize_summary, write_page_file, Dictionary, PageConfig, PageMeta,
    PageRecord, PredicateBlock, ResourceMap, SummaryBuilder,
    page_assignment::ResourceSummary,
};

/// Build a minimal index with resource-to-resource links, then run the
/// relations.ts flow to verify we find them.
#[test]
fn relations_flow_finds_linked_resources() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path();
    fs::create_dir_all(index_dir.join("pages")).unwrap();

    let base_uri = "https://example.org/";

    // Two resources that link to each other via a "related_resource" predicate
    let res_a = format!("{base_uri}resource/aaa-111");
    let res_b = format!("{base_uri}resource/bbb-222");
    let pred_related = format!("{base_uri}node/related_resource");
    let pred_type = format!("{base_uri}node/monument_type");
    let concept_church = format!("{base_uri}concept/church");

    // -- Build dictionary --
    let mut dict = Dictionary::new();
    let id_a = dict.intern(&res_a);
    let id_b = dict.intern(&res_b);
    let id_pred_related = dict.intern(&pred_related);
    let id_pred_type = dict.intern(&pred_type);
    let id_concept = dict.intern(&concept_church);

    // -- Page assignment: put both in the same page for simplicity --
    let summaries = vec![
        ResourceSummary {
            resource_id: "aaa-111".to_string(),
            graph_id: "heritage-place".to_string(),
            centroid: Some((-5.87, 54.61)),
            concept_ids: vec![],
        },
        ResourceSummary {
            resource_id: "bbb-222".to_string(),
            graph_id: "heritage-place".to_string(),
            centroid: Some((-5.86, 54.62)),
            concept_ids: vec![],
        },
    ];
    let page_index = assign_pages(&summaries, &PageConfig { target_page_size: 10 });

    let page_a = page_index.resource_to_page["aaa-111"];
    let page_b = page_index.resource_to_page["bbb-222"];

    // -- Build summary + records --
    let mut summary_builder = SummaryBuilder::new();
    let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();

    // A → related_resource → B (forward link)
    summary_builder.add(page_a, id_pred_related, id_b, id_a);
    page_records.entry(page_a).or_default().push((
        id_pred_related,
        PageRecord { subject_id: id_a, object_val: id_b },
    ));

    // B → related_resource → A (reverse link would be stored as !related_resource in relations.ts)
    summary_builder.add(page_b, id_pred_related, id_a, id_b);
    page_records.entry(page_b).or_default().push((
        id_pred_related,
        PageRecord { subject_id: id_b, object_val: id_a },
    ));

    // A → monument_type → church (concept link, NOT a resource link)
    summary_builder.add(page_a, id_pred_type, id_concept, id_a);
    page_records.entry(page_a).or_default().push((
        id_pred_type,
        PageRecord { subject_id: id_a, object_val: id_concept },
    ));

    // -- Write index files --
    let summary_bytes = serialize_summary(&summary_builder.build());
    fs::write(index_dir.join("summary.bin"), &summary_bytes).unwrap();
    fs::write(index_dir.join("dictionary.bin"), dict.to_bytes()).unwrap();

    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    fs::write(index_dir.join("resource_map.bin"), resource_map.to_bytes()).unwrap();

    let page_meta_json = serde_json::to_string(&page_index.page_meta).unwrap();
    fs::write(index_dir.join("page_meta.json"), &page_meta_json).unwrap();

    // Write page files
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
                    PredicateBlock { pred_id, records: recs }
                })
                .collect();
            let page_bytes = write_page_file(&mut blocks, &[]);
            fs::write(
                index_dir.join(format!("pages/page_{:04}.dat", pm.page_id)),
                &page_bytes,
            ).unwrap();
        }
    }

    // ===== Now run the relations.ts flow =====

    // Reload from disk (simulates client loading)
    let dict = Dictionary::from_bytes(&fs::read(index_dir.join("dictionary.bin")).unwrap()).unwrap();
    let summary = ros_madair_core::SummaryIndex::from_bytes(
        &fs::read(index_dir.join("summary.bin")).unwrap(),
    ).unwrap();
    let resource_map = ResourceMap::from_bytes(
        &fs::read(index_dir.join("resource_map.bin")).unwrap(),
    ).unwrap();
    let page_meta: Vec<PageMeta> =
        serde_json::from_str(&fs::read_to_string(index_dir.join("page_meta.json")).unwrap()).unwrap();
    let valid_pages: HashSet<u32> = page_meta.iter().map(|pm| pm.page_id).collect();

    // Step 1: page_for_resource
    let dict_id_a = dict.lookup(&res_a).expect("resource A should be in dict");
    let page_id = resource_map.page_for(dict_id_a).expect("resource A should have a page");

    // Step 2: summary_from_page
    let quads = summary.lookup_s(page_id);
    assert!(!quads.is_empty(), "should have summary quads from page {page_id}");

    // Step 3: filter to resource-link quads (page_o is a valid page ID)
    let resource_quads: Vec<_> = quads
        .iter()
        .filter(|q| valid_pages.contains(&q.page_o))
        .collect();

    assert!(
        !resource_quads.is_empty(),
        "should find at least one resource-link quad; all quads: {:?}",
        quads.iter().map(|q| {
            let pred = dict.resolve(q.predicate).unwrap_or("?");
            format!("({} -> page_o={}, pred={})", q.page_s, q.page_o, pred)
        }).collect::<Vec<_>>()
    );

    // Step 4+5: load records and filter by subject
    let mut found_relations: Vec<String> = Vec::new();
    for quad in &resource_quads {
        let pred_uri = dict.resolve(quad.predicate).unwrap();
        let page_path = index_dir.join(format!("pages/page_{:04}.dat", page_id));
        let data = fs::read(&page_path).unwrap();
        let header = parse_page_header(&data).unwrap();

        if let Some(entry) = header.entries.iter().find(|e| e.pred_id == quad.predicate) {
            let start = entry.offset as usize;
            let end = start + entry.record_count as usize * 8;
            let records = parse_records(&data[start..end]);

            for rec in &records {
                if rec.subject_id != dict_id_a {
                    continue;
                }
                // Step 6: check if object resolves to a /resource/ URI
                if let Some(obj_uri) = dict.resolve(rec.object_val) {
                    if obj_uri.contains("/resource/") {
                        found_relations.push(format!(
                            "{} --[{}]--> {}",
                            res_a, pred_uri, obj_uri
                        ));
                    }
                }
            }
        }
    }

    assert_eq!(
        found_relations.len(),
        1,
        "expected exactly 1 relation from A→B; got: {:?}",
        found_relations
    );
    assert!(
        found_relations[0].contains("bbb-222"),
        "relation should point to resource B: {}",
        found_relations[0]
    );
}

/// Test that the concept-type predicate is NOT mistakenly returned as a
/// resource relation (its object_val resolves to /concept/, not /resource/).
#[test]
fn relations_flow_excludes_concept_links() {
    let tmp = tempfile::tempdir().unwrap();
    let index_dir = tmp.path();
    fs::create_dir_all(index_dir.join("pages")).unwrap();

    let base_uri = "https://example.org/";
    let res_a = format!("{base_uri}resource/aaa-111");
    let pred_type = format!("{base_uri}node/monument_type");
    let concept = format!("{base_uri}concept/church");

    let mut dict = Dictionary::new();
    let id_a = dict.intern(&res_a);
    let id_pred = dict.intern(&pred_type);
    let id_concept = dict.intern(&concept);

    let summaries = vec![ResourceSummary {
        resource_id: "aaa-111".to_string(),
        graph_id: "test".to_string(),
        centroid: Some((0.0, 0.0)),
        concept_ids: vec![],
    }];
    let page_index = assign_pages(&summaries, &PageConfig { target_page_size: 10 });
    let page_a = page_index.resource_to_page["aaa-111"];

    let mut summary_builder = SummaryBuilder::new();
    summary_builder.add(page_a, id_pred, id_concept, id_a);

    let summary_bytes = serialize_summary(&summary_builder.build());
    fs::write(index_dir.join("summary.bin"), &summary_bytes).unwrap();
    fs::write(index_dir.join("dictionary.bin"), dict.to_bytes()).unwrap();

    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    fs::write(index_dir.join("resource_map.bin"), resource_map.to_bytes()).unwrap();
    fs::write(
        index_dir.join("page_meta.json"),
        serde_json::to_string(&page_index.page_meta).unwrap(),
    ).unwrap();

    let mut blocks = vec![PredicateBlock {
        pred_id: id_pred,
        records: vec![PageRecord { subject_id: id_a, object_val: id_concept }],
    }];
    fs::write(
        index_dir.join(format!("pages/page_{:04}.dat", page_a)),
        write_page_file(&mut blocks, &[]),
    ).unwrap();

    // Reload
    let dict = Dictionary::from_bytes(&fs::read(index_dir.join("dictionary.bin")).unwrap()).unwrap();
    let summary = ros_madair_core::SummaryIndex::from_bytes(
        &fs::read(index_dir.join("summary.bin")).unwrap(),
    ).unwrap();
    let resource_map = ResourceMap::from_bytes(
        &fs::read(index_dir.join("resource_map.bin")).unwrap(),
    ).unwrap();
    let page_meta: Vec<PageMeta> =
        serde_json::from_str(&fs::read_to_string(index_dir.join("page_meta.json")).unwrap()).unwrap();
    let valid_pages: HashSet<u32> = page_meta.iter().map(|pm| pm.page_id).collect();

    let dict_id_a = dict.lookup(&res_a).unwrap();
    let page_id = resource_map.page_for(dict_id_a).unwrap();
    let quads = summary.lookup_s(page_id);

    // The concept's dict_id should NOT be a valid page — so it should be filtered out
    let resource_quads: Vec<_> = quads
        .iter()
        .filter(|q| valid_pages.contains(&q.page_o))
        .collect();

    // The concept dict_id might accidentally collide with a page_id if both are small numbers.
    // In that case, the /resource/ URI check is the final guard.
    let mut found_resource_links = 0;
    for quad in &resource_quads {
        let page_path = index_dir.join(format!("pages/page_{:04}.dat", page_id));
        let data = fs::read(&page_path).unwrap();
        let header = parse_page_header(&data).unwrap();
        if let Some(entry) = header.entries.iter().find(|e| e.pred_id == quad.predicate) {
            let start = entry.offset as usize;
            let end = start + entry.record_count as usize * 8;
            let records = parse_records(&data[start..end]);
            for rec in &records {
                if rec.subject_id != dict_id_a { continue; }
                if let Some(obj_uri) = dict.resolve(rec.object_val) {
                    if obj_uri.contains("/resource/") {
                        found_resource_links += 1;
                    }
                }
            }
        }
    }

    assert_eq!(
        found_resource_links, 0,
        "concept links should NOT appear as resource relations"
    );
}
