// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Shared build-time logic used by both the binary (`ros-madair-build`) and
//! PyO3 (`IndexBuilder`) builders.
//!
//! These functions extract the core algorithmic loops that were previously
//! duplicated across the two builders: resource summary extraction, external
//! target scanning (for shadow pages), concept URI interning, and per-resource
//! page record construction with reverse-predicate emission.
//!
//! The [`build_to_memory`] function is the main entry point for consumers that
//! want in-memory index generation without filesystem I/O (e.g. Tauri plugins).

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::Path;

use alizarin_core::graph::{StaticGraph, StaticResource, StaticTile};
use alizarin_core::skos::SkosCollection;
use serde::Serialize;

use crate::concept_intervals::ConceptIntervalIndex;
use crate::datatype_class::{classify_datatype, DatatypeClass};
use crate::quantize::QuantizeType;
use crate::uri::{concept_prefix, node_uri, resource_prefix, resource_uri};
use crate::{
    assign_pages, assign_shadow_pages, build_concept_indexes,
    extract_centroid, graph_schema_to_triples, quantize_tile_value,
    quantize_type_for_datatype, resource_to_triples, serialize_summary,
    triples_to_ntriples, write_page_file, write_tile_content_file,
    Dictionary, PageConfig, PageIndex, PageRecord, PredicateBlock, ResourceMap,
    ResourceMeta, SummaryBuilder,
};
use crate::page_assignment::ResourceSummary;
use crate::value_extract;

/// Extract centroid and concept IDs from a resource's tiles.
///
/// Used during page assignment to build `ResourceSummary` entries.
pub fn extract_resource_summary(
    tiles: &[StaticTile],
    graph: &StaticGraph,
) -> (Option<(f64, f64)>, Vec<String>) {
    let mut centroid = None;
    let mut concept_ids = Vec::new();

    for tile in tiles {
        for (node_id, value) in &tile.data {
            if value.is_null() {
                continue;
            }
            let node = match graph.get_node_by_id(node_id) {
                Some(n) => n,
                None => continue,
            };
            match classify_datatype(&node.datatype) {
                Some(DatatypeClass::GeoJson) => {
                    if centroid.is_none() {
                        centroid = extract_centroid(&value.to_string());
                    }
                }
                Some(DatatypeClass::Concept | DatatypeClass::DomainValue) => {
                    concept_ids.extend(value_extract::extract_reference_ids(value));
                }
                _ => {}
            }
        }
    }

    (centroid, concept_ids)
}

/// Scan tiles for resource-instance references whose target is not in
/// `resource_to_page`, collecting them into `external_targets`.
///
/// Call once per resource during the shadow-page pre-scan.
pub fn collect_external_targets(
    tiles: &[StaticTile],
    graph: &StaticGraph,
    resource_to_page: &HashMap<String, u32>,
    external_targets: &mut HashSet<String>,
) {
    for tile in tiles {
        for (node_id, value) in &tile.data {
            if value.is_null() {
                continue;
            }
            let node = match graph.get_node_by_id(node_id) {
                Some(n) => n,
                None => continue,
            };
            if classify_datatype(&node.datatype) == Some(DatatypeClass::ResourceInstance) {
                for ref_id in value_extract::extract_reference_ids(value) {
                    if !resource_to_page.contains_key(&ref_id) {
                        external_targets.insert(ref_id);
                    }
                }
            }
        }
    }
}

/// Collect ALL resource-instance reference IDs from a resource's tiles.
///
/// Returns every resource-instance reference found, without filtering
/// against `resource_to_page`. Used during Pass 1 of the streaming build
/// to gather the full set of referenced IDs before page assignment.
pub fn collect_reference_ids(
    tiles: &[StaticTile],
    graph: &StaticGraph,
) -> Vec<String> {
    let mut refs = Vec::new();
    for tile in tiles {
        for (node_id, value) in &tile.data {
            if value.is_null() {
                continue;
            }
            let node = match graph.get_node_by_id(node_id) {
                Some(n) => n,
                None => continue,
            };
            if classify_datatype(&node.datatype) == Some(DatatypeClass::ResourceInstance) {
                refs.extend(value_extract::extract_reference_ids(value));
            }
        }
    }
    refs
}

/// Recursively intern all concept value_id URIs into the dictionary.
///
/// Must be called before building the `ConceptIntervalIndex` so that
/// all value_ids have dict_ids for the interval lookup.
pub fn intern_concept_uris(
    concepts: &HashMap<String, alizarin_core::skos::SkosConcept>,
    prefix: &str,
    dict: &mut Dictionary,
) {
    for concept in concepts.values() {
        for value in concept.pref_labels.values() {
            if !value.id.is_empty() {
                dict.intern(&format!("{prefix}{}", value.id));
            }
        }
        if let Some(children) = &concept.children {
            let child_map: HashMap<String, alizarin_core::skos::SkosConcept> =
                children.iter().map(|c| (c.id.clone(), c.clone())).collect();
            intern_concept_uris(&child_map, prefix, dict);
        }
    }
}

/// Build page records and summary quads for a single resource.
///
/// Processes all tiles, quantizes values, emits forward records on the
/// resource's own page, and reverse (`!pred`) records on the target's page
/// for resource-instance links.
///
/// Returns the `subject_id` (dict ID of the resource URI).
pub fn build_records_for_resource(
    resource_id: &str,
    tiles: &[StaticTile],
    graph: &StaticGraph,
    page_id: u32,
    dict: &mut Dictionary,
    base_uri: &str,
    resource_to_page: &HashMap<String, u32>,
    concept_intervals: Option<&ConceptIntervalIndex>,
    page_records: &mut HashMap<u32, Vec<(u32, PageRecord)>>,
    summary_builder: &mut SummaryBuilder,
) -> u32 {
    let subject_id = dict.intern(&crate::uri::resource_uri(base_uri, resource_id));

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
                Some(a) if !a.is_empty() => a.as_str(),
                _ => continue,
            };
            let qtype = match quantize_type_for_datatype(&node.datatype) {
                Some(qt) => qt,
                None => continue,
            };
            let pred_id = dict.intern(&node_uri(base_uri, alias));

            let object_vals = quantize_tile_value(
                value, qtype, &node.datatype, dict, base_uri,
                concept_intervals,
            );

            for object_val in object_vals {
                let record = PageRecord {
                    object_val,
                    subject_id,
                };
                page_records
                    .entry(page_id)
                    .or_default()
                    .push((pred_id, record));

                const NON_PAGE_SENTINEL: u32 = u32::MAX;
                let page_o = match qtype {
                    QuantizeType::ConceptDfs => {
                        ConceptIntervalIndex::encode_for_summary(object_val)
                    }
                    QuantizeType::DictionaryId => {
                        if let Some(term) = dict.resolve(object_val) {
                            if let Some(rid) = term.strip_prefix(&resource_prefix(base_uri)) {
                                resource_to_page
                                    .get(rid)
                                    .copied()
                                    .unwrap_or(NON_PAGE_SENTINEL)
                            } else {
                                NON_PAGE_SENTINEL
                            }
                        } else {
                            NON_PAGE_SENTINEL
                        }
                    }
                    _ => object_val,
                };
                summary_builder.add(page_id, pred_id, page_o, subject_id);

                // Emit reverse record on target's page for resource-instance links
                if page_o != NON_PAGE_SENTINEL && qtype == QuantizeType::DictionaryId {
                    let reverse_pred_id = dict.intern(&format!("!{}", node_uri(base_uri, alias)));
                    let reverse_record = PageRecord {
                        subject_id: object_val,
                        object_val: subject_id,
                    };
                    page_records
                        .entry(page_o)
                        .or_default()
                        .push((reverse_pred_id, reverse_record));
                    summary_builder.add(page_o, reverse_pred_id, page_id, object_val);
                }
            }
        }
    }

    subject_id
}

/// All output artifacts as in-memory byte buffers.
///
/// Keys are relative paths matching the on-disk layout, e.g.:
///   `"summary.bin"`, `"dictionary.bin"`, `"resource_map.bin"`,
///   `"page_meta.json"`, `"resource_names.json"`,
///   `"pages/page_0000.dat"`, `"tiles/tile_0000.dat"`,
///   `"all.nt"`, `"concept_hierarchy.json"`,
///   `"concept_intervals.bin"`, `"concept_tree.bin"`
///
/// Graph JSON files are NOT included — callers that need them
/// should write the raw graph JSON separately.
pub fn build_to_memory(
    base_uri: &str,
    graphs: &HashMap<String, StaticGraph>,
    resources: &[StaticResource],
    vocabulary_collections: &[SkosCollection],
    page_size: Option<usize>,
) -> Result<HashMap<String, Vec<u8>>, String> {
    let config = PageConfig {
        target_page_size: page_size.unwrap_or(2000),
    };
    let mut dict = Dictionary::new();
    let mut artifacts: HashMap<String, Vec<u8>> = HashMap::new();

    // 1. Build resource summaries for page assignment
    let summaries: Vec<ResourceSummary> = resources
        .iter()
        .map(|r| {
            let tiles = r.tiles.as_deref().unwrap_or_default();
            let (centroid, concept_ids) = graphs
                .get(&r.resourceinstance.graph_id)
                .map(|g| extract_resource_summary(tiles, g))
                .unwrap_or_default();
            ResourceSummary {
                resource_id: r.resourceinstance.resourceinstanceid.clone(),
                graph_id: r.resourceinstance.graph_id.clone(),
                centroid,
                concept_ids,
            }
        })
        .collect();

    // 2. Assign pages
    let mut page_index = assign_pages(&summaries, &config);

    // 3. Shadow pages for external link targets
    let mut external_targets = HashSet::new();
    for resource in resources {
        let graph = match graphs.get(&resource.resourceinstance.graph_id) {
            Some(g) => g,
            None => continue,
        };
        let tiles = resource.tiles.as_deref().unwrap_or_default();
        collect_external_targets(tiles, graph, &page_index.resource_to_page, &mut external_targets);
    }
    if !external_targets.is_empty() {
        let external_ids: Vec<String> = external_targets.into_iter().collect();
        let shadow_index = assign_shadow_pages(
            &external_ids,
            page_index.page_meta.len() as u32,
            &config,
        );
        page_index.resource_to_page.extend(shadow_index.resource_to_page);
        page_index.page_meta.extend(shadow_index.page_meta);
    }

    // 4. Concept indexes
    let (concept_interval_index, concept_tree) = if !vocabulary_collections.is_empty() {
        let prefix = concept_prefix(base_uri);
        for coll in vocabulary_collections {
            intern_concept_uris(&coll.concepts, &prefix, &mut dict);
        }
        let (ci, ct) = build_concept_indexes(vocabulary_collections, &dict, base_uri);
        (Some(ci), Some(ct))
    } else {
        (None, None)
    };

    // 5. Build page records, summary quads, tile content, and resource names
    let mut summary_builder = SummaryBuilder::new();
    let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();
    let mut tile_content: HashMap<u32, Vec<(u32, Vec<u8>)>> = HashMap::new();
    let mut resource_names: HashMap<String, String> = HashMap::new();

    for resource in resources {
        let graph = match graphs.get(&resource.resourceinstance.graph_id) {
            Some(g) => g,
            None => continue,
        };
        let rid = &resource.resourceinstance.resourceinstanceid;
        let tiles = resource.tiles.as_deref().unwrap_or_default();
        let page_id = match page_index.resource_to_page.get(rid.as_str()) {
            Some(&p) => p,
            None => continue,
        };

        if !resource.resourceinstance.name.is_empty() {
            resource_names.insert(rid.clone(), resource.resourceinstance.name.clone());
        }

        let subject_id = build_records_for_resource(
            rid, tiles, graph, page_id, &mut dict, base_uri,
            &page_index.resource_to_page, concept_interval_index.as_ref(),
            &mut page_records, &mut summary_builder,
        );

        // v2 tile blob with cache + scopes
        #[derive(Serialize)]
        struct ResourceBlob<'a> {
            tiles: &'a [StaticTile],
            #[serde(skip_serializing_if = "Option::is_none", rename = "__cache")]
            cache: Option<&'a serde_json::Value>,
            #[serde(skip_serializing_if = "Option::is_none", rename = "__scopes")]
            scopes: Option<&'a serde_json::Value>,
        }
        let blob = ResourceBlob {
            tiles,
            cache: resource.cache.as_ref(),
            scopes: resource.scopes.as_ref(),
        };
        if let Ok(tile_bytes) = rmp_serde::to_vec_named(&blob) {
            if !tile_bytes.is_empty() {
                tile_content.entry(page_id).or_default().push((subject_id, tile_bytes));
            }
        }
    }

    // 6. Serialize all artifacts

    // Summary
    let summary_quads = summary_builder.build();
    artifacts.insert("summary.bin".into(), serialize_summary(&summary_quads));

    // Dictionary
    artifacts.insert("dictionary.bin".into(), dict.to_bytes());

    // Resource map
    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    artifacts.insert("resource_map.bin".into(), resource_map.to_bytes());

    // Page meta — only include pages that have records (live pages)
    let live_page_meta: Vec<_> = page_index.page_meta.iter()
        .filter(|pm| page_records.contains_key(&pm.page_id))
        .collect();
    let page_meta_json = serde_json::to_string_pretty(&live_page_meta)
        .map_err(|e| format!("Failed to serialize page meta: {e}"))?;
    artifacts.insert("page_meta.json".into(), page_meta_json.into_bytes());

    // Resource names
    let names_json = serde_json::to_string(&resource_names)
        .map_err(|e| format!("Failed to serialize resource names: {e}"))?;
    artifacts.insert("resource_names.json".into(), names_json.into_bytes());

    // Per-page resource metadata (from StaticResource descriptors)
    let mut page_resource_meta: HashMap<u32, Vec<ResourceMeta>> = HashMap::new();
    for resource in resources {
        let rid = &resource.resourceinstance.resourceinstanceid;
        let page_id = match page_index.resource_to_page.get(rid.as_str()) {
            Some(&p) => p,
            None => continue,
        };
        let dict_id = match dict.lookup(&resource_uri(base_uri, rid)) {
            Some(id) => id,
            None => continue,
        };
        let name = resource.resourceinstance.descriptors.name.as_deref()
            .unwrap_or(&resource.resourceinstance.name)
            .to_string();
        let slug = resource.resourceinstance.descriptors.slug.clone().unwrap_or_default();
        let model = graphs.get(&resource.resourceinstance.graph_id)
            .map(|g| g.name.get("en"))
            .unwrap_or_default();
        page_resource_meta.entry(page_id).or_default().push(ResourceMeta {
            dict_id, name, slug, model,
        });
    }
    let has_meta = !page_resource_meta.is_empty();

    // Page files
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
            let meta = if has_meta {
                page_resource_meta.get(&pm.page_id).map(|v| v.as_slice()).unwrap_or(&[])
            } else {
                &[]
            };
            let page_bytes = write_page_file(&mut blocks, meta);
            artifacts.insert(format!("pages/page_{:04}.dat", pm.page_id), page_bytes);
        }
    }

    // Tile content files
    for pm in &page_index.page_meta {
        if let Some(mut entries) = tile_content.remove(&pm.page_id) {
            entries.sort_by_key(|(sid, _)| *sid);
            let tile_bytes = write_tile_content_file(&entries);
            artifacts.insert(format!("tiles/tile_{:04}.dat", pm.page_id), tile_bytes);
        }
    }

    // RDF export (N-Triples)
    let mut all_triples = Vec::new();
    for graph in graphs.values() {
        if let Ok(schema_triples) = graph_schema_to_triples(graph, base_uri) {
            all_triples.extend(schema_triples);
        }
    }
    for resource in resources {
        if let Some(graph) = graphs.get(&resource.resourceinstance.graph_id) {
            let tiles = resource.tiles.as_deref().unwrap_or_default();
            if let Ok((triples, _)) = resource_to_triples(
                graph, &resource.resourceinstance.resourceinstanceid, tiles, base_uri,
            ) {
                all_triples.extend(triples);
            }
        }
    }
    let ntriples = triples_to_ntriples(&all_triples);
    artifacts.insert("all.nt".into(), ntriples.into_bytes());

    // Concept hierarchy JSON
    if !vocabulary_collections.is_empty() {
        let hierarchy_json = serde_json::to_string_pretty(vocabulary_collections)
            .map_err(|e| format!("Failed to serialize concept hierarchy: {e}"))?;
        artifacts.insert("concept_hierarchy.json".into(), hierarchy_json.into_bytes());
    }

    // Concept interval index
    if let Some(ci) = &concept_interval_index {
        artifacts.insert("concept_intervals.bin".into(), ci.to_bytes());
    }

    // Concept tree
    if let Some(ct) = &concept_tree {
        artifacts.insert("concept_tree.bin".into(), ct.to_bytes());
    }

    Ok(artifacts)
}

/// Summary statistics returned by [`build_to_disk`].
#[derive(Debug, Clone)]
pub struct BuildStats {
    pub summary_bytes: usize,
    pub dictionary_bytes: usize,
    pub resource_map_bytes: usize,
    pub page_meta_bytes: usize,
    pub page_count: usize,
    pub page_total_bytes: usize,
    pub tile_count: usize,
    pub tile_total_bytes: usize,
    pub ntriples_bytes: usize,
}

/// Read current RSS from /proc/self/status (Linux only). Returns MB.
#[cfg(target_os = "linux")]
fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

#[cfg(not(target_os = "linux"))]
fn rss_mb() -> f64 { 0.0 }

/// Build the index and write all output files directly to `output_dir`.
///
/// This is the memory-efficient alternative to [`build_to_memory`]: instead of
/// accumulating ~600 MB of serialized artifacts in a HashMap, files are written
/// to disk as they are produced and their buffers dropped immediately.
///
/// N-Triples are streamed via a second pass over resources, avoiding the
/// ~150 MB `all_triples` Vec.
///
/// `build_to_memory` is preserved unchanged for PyO3/Tauri consumers.
pub fn build_to_disk(
    output_dir: &Path,
    base_uri: &str,
    graphs: &HashMap<String, StaticGraph>,
    resources: &[StaticResource],
    vocabulary_collections: &[SkosCollection],
    page_size: Option<usize>,
) -> Result<BuildStats, String> {
    let config = PageConfig {
        target_page_size: page_size.unwrap_or(2000),
    };
    let mut dict = Dictionary::new();

    // Ensure output directories exist
    let pages_dir = output_dir.join("pages");
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&pages_dir)
        .map_err(|e| format!("Failed to create pages dir: {e}"))?;
    std::fs::create_dir_all(&tiles_dir)
        .map_err(|e| format!("Failed to create tiles dir: {e}"))?;

    eprintln!("[mem] build_to_disk start: {:.0} MB RSS", rss_mb());

    // Size estimation for resources
    {
        let mut tiles_count = 0usize;
        let mut cache_count = 0usize;
        let mut scopes_count = 0usize;
        let mut cache_bytes_est = 0usize;
        let mut scopes_bytes_est = 0usize;
        let mut tiles_bytes_est = 0usize;
        for r in resources {
            if let Some(tiles) = &r.tiles {
                tiles_count += tiles.len();
                for t in tiles {
                    tiles_bytes_est += t.data.len() * 200; // rough estimate per entry
                }
            }
            if let Some(c) = &r.cache {
                cache_count += 1;
                cache_bytes_est += c.to_string().len();
            }
            if let Some(s) = &r.scopes {
                scopes_count += 1;
                scopes_bytes_est += s.to_string().len();
            }
        }
        eprintln!("[mem] resources: {} total, {} tiles, cache: {} ({:.0} MB est), scopes: {} ({:.0} MB est), tiles_data: {:.0} MB est",
            resources.len(), tiles_count, cache_count, cache_bytes_est as f64 / 1048576.0,
            scopes_count, scopes_bytes_est as f64 / 1048576.0, tiles_bytes_est as f64 / 1048576.0);
    }

    // 1. Build resource summaries for page assignment
    let summaries: Vec<ResourceSummary> = resources
        .iter()
        .map(|r| {
            let tiles = r.tiles.as_deref().unwrap_or_default();
            let (centroid, concept_ids) = graphs
                .get(&r.resourceinstance.graph_id)
                .map(|g| extract_resource_summary(tiles, g))
                .unwrap_or_default();
            ResourceSummary {
                resource_id: r.resourceinstance.resourceinstanceid.clone(),
                graph_id: r.resourceinstance.graph_id.clone(),
                centroid,
                concept_ids,
            }
        })
        .collect();

    // 2. Assign pages
    let mut page_index = assign_pages(&summaries, &config);

    // 3. Shadow pages for external link targets
    let mut external_targets = HashSet::new();
    for resource in resources {
        let graph = match graphs.get(&resource.resourceinstance.graph_id) {
            Some(g) => g,
            None => continue,
        };
        let tiles = resource.tiles.as_deref().unwrap_or_default();
        collect_external_targets(tiles, graph, &page_index.resource_to_page, &mut external_targets);
    }
    if !external_targets.is_empty() {
        let external_ids: Vec<String> = external_targets.into_iter().collect();
        let shadow_index = assign_shadow_pages(
            &external_ids,
            page_index.page_meta.len() as u32,
            &config,
        );
        page_index.resource_to_page.extend(shadow_index.resource_to_page);
        page_index.page_meta.extend(shadow_index.page_meta);
    }

    // 4. Concept indexes — write immediately
    let (concept_interval_index, _concept_tree) = if !vocabulary_collections.is_empty() {
        let prefix = concept_prefix(base_uri);
        for coll in vocabulary_collections {
            intern_concept_uris(&coll.concepts, &prefix, &mut dict);
        }
        let (ci, ct) = build_concept_indexes(vocabulary_collections, &dict, base_uri);

        // Write concept files now (small, ~few KB each)
        let hierarchy_json = serde_json::to_string_pretty(vocabulary_collections)
            .map_err(|e| format!("Failed to serialize concept hierarchy: {e}"))?;
        std::fs::write(output_dir.join("concept_hierarchy.json"), hierarchy_json.as_bytes())
            .map_err(|e| format!("Failed to write concept_hierarchy.json: {e}"))?;
        std::fs::write(output_dir.join("concept_intervals.bin"), ci.to_bytes())
            .map_err(|e| format!("Failed to write concept_intervals.bin: {e}"))?;
        std::fs::write(output_dir.join("concept_tree.bin"), ct.to_bytes())
            .map_err(|e| format!("Failed to write concept_tree.bin: {e}"))?;

        (Some(ci), Some(ct))
    } else {
        (None, None)
    };

    // 5. Build page records, summary quads, tile content, resource names, and
    //    page resource metadata in a single pass over resources.
    //
    //    PEAK MEMORY HERE: all_resources(500) + page_records(200) +
    //    tile_content(300) + dict(50) + summary(100) + graphs(100) ≈ 1.3 GB
    let mut summary_builder = SummaryBuilder::new();
    let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();
    let mut tile_content: HashMap<u32, Vec<(u32, Vec<u8>)>> = HashMap::new();
    let mut resource_names: HashMap<String, String> = HashMap::new();
    let mut page_resource_meta: HashMap<u32, Vec<ResourceMeta>> = HashMap::new();

    for resource in resources {
        let graph = match graphs.get(&resource.resourceinstance.graph_id) {
            Some(g) => g,
            None => continue,
        };
        let rid = &resource.resourceinstance.resourceinstanceid;
        let tiles = resource.tiles.as_deref().unwrap_or_default();
        let page_id = match page_index.resource_to_page.get(rid.as_str()) {
            Some(&p) => p,
            None => continue,
        };

        if !resource.resourceinstance.name.is_empty() {
            resource_names.insert(rid.clone(), resource.resourceinstance.name.clone());
        }

        let subject_id = build_records_for_resource(
            rid, tiles, graph, page_id, &mut dict, base_uri,
            &page_index.resource_to_page, concept_interval_index.as_ref(),
            &mut page_records, &mut summary_builder,
        );

        // v2 tile blob with cache + scopes
        #[derive(Serialize)]
        struct ResourceBlob<'a> {
            tiles: &'a [StaticTile],
            #[serde(skip_serializing_if = "Option::is_none", rename = "__cache")]
            cache: Option<&'a serde_json::Value>,
            #[serde(skip_serializing_if = "Option::is_none", rename = "__scopes")]
            scopes: Option<&'a serde_json::Value>,
        }
        let blob = ResourceBlob {
            tiles,
            cache: resource.cache.as_ref(),
            scopes: resource.scopes.as_ref(),
        };
        if let Ok(tile_bytes) = rmp_serde::to_vec_named(&blob) {
            if !tile_bytes.is_empty() {
                tile_content.entry(page_id).or_default().push((subject_id, tile_bytes));
            }
        }

        // Per-page resource metadata (merged from separate pass in build_to_memory)
        let dict_id = dict.lookup(&resource_uri(base_uri, rid)).unwrap_or(subject_id);
        let name = resource.resourceinstance.descriptors.name.as_deref()
            .unwrap_or(&resource.resourceinstance.name)
            .to_string();
        let slug = resource.resourceinstance.descriptors.slug.clone().unwrap_or_default();
        let model = graphs.get(&resource.resourceinstance.graph_id)
            .map(|g| g.name.get("en"))
            .unwrap_or_default();
        page_resource_meta.entry(page_id).or_default().push(ResourceMeta {
            dict_id, name, slug, model,
        });
    }

    let has_meta = !page_resource_meta.is_empty();
    eprintln!("[mem] after resource loop: {:.0} MB RSS (page_records: {} pages, tile_content: {} pages)", rss_mb(), page_records.len(), tile_content.len());

    // 6. Write page files → drop page_records
    let live_page_ids: HashSet<u32> = page_records.keys().copied().collect();
    let mut page_count = 0usize;
    let mut page_total_bytes = 0usize;
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
            let meta = if has_meta {
                page_resource_meta.get(&pm.page_id).map(|v| v.as_slice()).unwrap_or(&[])
            } else {
                &[]
            };
            let page_bytes = write_page_file(&mut blocks, meta);
            let path = pages_dir.join(format!("page_{:04}.dat", pm.page_id));
            std::fs::write(&path, &page_bytes)
                .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
            page_count += 1;
            page_total_bytes += page_bytes.len();
        }
    }
    drop(page_records);
    drop(page_resource_meta);
    eprintln!("[mem] after page write+drop: {:.0} MB RSS", rss_mb());

    // 7. Write tile files → drop tile_content
    let mut tile_count = 0usize;
    let mut tile_total_bytes = 0usize;
    for pm in &page_index.page_meta {
        if let Some(mut entries) = tile_content.remove(&pm.page_id) {
            entries.sort_by_key(|(sid, _)| *sid);
            let tile_bytes = write_tile_content_file(&entries);
            let path = tiles_dir.join(format!("tile_{:04}.dat", pm.page_id));
            std::fs::write(&path, &tile_bytes)
                .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
            tile_count += 1;
            tile_total_bytes += tile_bytes.len();
        }
    }
    drop(tile_content);
    eprintln!("[mem] after tile write+drop: {:.0} MB RSS", rss_mb());

    // 8. Write summary, dictionary, resource_map, page_meta, resource_names
    let summary_quads = summary_builder.build();
    let summary_bytes_data = serialize_summary(&summary_quads);
    let summary_bytes = summary_bytes_data.len();
    std::fs::write(output_dir.join("summary.bin"), &summary_bytes_data)
        .map_err(|e| format!("Failed to write summary.bin: {e}"))?;
    drop(summary_bytes_data);

    let dict_bytes_data = dict.to_bytes();
    let dictionary_bytes = dict_bytes_data.len();
    std::fs::write(output_dir.join("dictionary.bin"), &dict_bytes_data)
        .map_err(|e| format!("Failed to write dictionary.bin: {e}"))?;
    drop(dict_bytes_data);

    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    let rmap_bytes_data = resource_map.to_bytes();
    let resource_map_bytes = rmap_bytes_data.len();
    std::fs::write(output_dir.join("resource_map.bin"), &rmap_bytes_data)
        .map_err(|e| format!("Failed to write resource_map.bin: {e}"))?;
    drop(rmap_bytes_data);

    // page_meta: only include pages that had records (page_records keys captured earlier)
    let live_page_meta: Vec<_> = page_index.page_meta.iter()
        .filter(|pm| live_page_ids.contains(&pm.page_id))
        .collect();
    let page_meta_json = serde_json::to_string_pretty(&live_page_meta)
        .map_err(|e| format!("Failed to serialize page meta: {e}"))?;
    let page_meta_bytes = page_meta_json.len();
    std::fs::write(output_dir.join("page_meta.json"), page_meta_json.as_bytes())
        .map_err(|e| format!("Failed to write page_meta.json: {e}"))?;

    let names_json = serde_json::to_string(&resource_names)
        .map_err(|e| format!("Failed to serialize resource names: {e}"))?;
    std::fs::write(output_dir.join("resource_names.json"), names_json.as_bytes())
        .map_err(|e| format!("Failed to write resource_names.json: {e}"))?;

    eprintln!("[mem] after metadata write: {:.0} MB RSS", rss_mb());

    // 9. Stream N-Triples to disk (second pass over resources, no accumulation)
    let nt_path = output_dir.join("all.nt");
    let nt_file = std::fs::File::create(&nt_path)
        .map_err(|e| format!("Failed to create all.nt: {e}"))?;
    let mut nt_writer = BufWriter::new(nt_file);
    let mut ntriples_bytes = 0usize;

    // Schema triples from graphs
    for graph in graphs.values() {
        if let Ok(schema_triples) = graph_schema_to_triples(graph, base_uri) {
            for triple in &schema_triples {
                let line = triple.to_ntriples();
                ntriples_bytes += line.len() + 1;
                writeln!(nt_writer, "{}", line)
                    .map_err(|e| format!("Failed to write N-Triple: {e}"))?;
            }
        }
    }

    // Resource triples (second pass — tiles are still in memory via resources slice)
    for resource in resources {
        if let Some(graph) = graphs.get(&resource.resourceinstance.graph_id) {
            let tiles = resource.tiles.as_deref().unwrap_or_default();
            if let Ok((triples, _)) = resource_to_triples(
                graph, &resource.resourceinstance.resourceinstanceid, tiles, base_uri,
            ) {
                for triple in &triples {
                    let line = triple.to_ntriples();
                    ntriples_bytes += line.len() + 1;
                    writeln!(nt_writer, "{}", line)
                        .map_err(|e| format!("Failed to write N-Triple: {e}"))?;
                }
            }
        }
    }

    nt_writer.flush()
        .map_err(|e| format!("Failed to flush all.nt: {e}"))?;
    eprintln!("[mem] after N-Triples stream: {:.0} MB RSS", rss_mb());

    Ok(BuildStats {
        summary_bytes,
        dictionary_bytes,
        resource_map_bytes,
        page_meta_bytes,
        page_count,
        page_total_bytes,
        tile_count,
        tile_total_bytes,
        ntriples_bytes,
    })
}

// ---------------------------------------------------------------------------
// Streaming two-pass build API
// ---------------------------------------------------------------------------
//
// These types and functions support a two-pass build where resources are
// loaded per-file and dropped between files, avoiding the ~2.9 GB peak
// from holding all resources in memory simultaneously.
//
// Pass 1: extract ResourceSummary + reference IDs per file → prepare_routing
// Pass 2: process resources per file → process_resource_batch (accumulates)
// After:  finalize_build writes all accumulated data to disk

/// Pre-computed routing state from Pass 1.
///
/// Contains page assignments and concept indexes needed by Pass 2.
pub struct BuildRouting {
    pub page_index: PageIndex,
    pub concept_interval_index: Option<ConceptIntervalIndex>,
    /// Pre-serialized concept artifacts: (intervals.bin, tree.bin, hierarchy.json).
    concept_bytes: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
}

/// Mutable accumulator state updated during Pass 2.
///
/// Holds all data being built up across resource batches:
/// page records, tile content, resource names, metadata, and a
/// streaming N-Triples writer.
pub struct BuildAccumulator {
    pub dict: Dictionary,
    pub summary_builder: SummaryBuilder,
    pub page_records: HashMap<u32, Vec<(u32, PageRecord)>>,
    pub tile_content: HashMap<u32, Vec<(u32, Vec<u8>)>>,
    pub resource_names: HashMap<String, String>,
    pub page_resource_meta: HashMap<u32, Vec<ResourceMeta>>,
    nt_writer: BufWriter<std::fs::File>,
    pub ntriples_bytes: usize,
}

impl BuildAccumulator {
    /// Create a new accumulator with a pre-seeded dictionary.
    ///
    /// Opens `output_dir/all.nt` for streaming N-Triples output.
    /// The output directory must already exist.
    pub fn new(dict: Dictionary, output_dir: &Path) -> Result<Self, String> {
        let nt_path = output_dir.join("all.nt");
        let nt_file = std::fs::File::create(&nt_path)
            .map_err(|e| format!("Failed to create all.nt: {e}"))?;
        Ok(Self {
            dict,
            summary_builder: SummaryBuilder::new(),
            page_records: HashMap::new(),
            tile_content: HashMap::new(),
            resource_names: HashMap::new(),
            page_resource_meta: HashMap::new(),
            nt_writer: BufWriter::new(nt_file),
            ntriples_bytes: 0,
        })
    }

    /// Write schema triples from graph definitions to the N-Triples stream.
    ///
    /// Call this once before processing resource batches.
    pub fn write_schema_triples(
        &mut self,
        graphs: &HashMap<String, StaticGraph>,
        base_uri: &str,
    ) -> Result<(), String> {
        for graph in graphs.values() {
            if let Ok(schema_triples) = graph_schema_to_triples(graph, base_uri) {
                for triple in &schema_triples {
                    let line = triple.to_ntriples();
                    self.ntriples_bytes += line.len() + 1;
                    writeln!(self.nt_writer, "{}", line)
                        .map_err(|e| format!("Failed to write N-Triple: {e}"))?;
                }
            }
        }
        Ok(())
    }
}

/// Compute page assignments and concept indexes from lightweight summaries.
///
/// Call after Pass 1 has extracted summaries and reference IDs from all
/// resources. Returns routing state for Pass 2 and a dictionary with
/// concept URIs pre-interned (to be moved into [`BuildAccumulator`]).
///
/// `all_reference_ids` should contain every resource-instance reference ID
/// found across all resources. External targets (IDs not in the page index)
/// are assigned to shadow pages automatically.
pub fn prepare_routing(
    summaries: &[ResourceSummary],
    all_reference_ids: &HashSet<String>,
    vocabulary_collections: &[SkosCollection],
    base_uri: &str,
    page_size: Option<usize>,
) -> Result<(BuildRouting, Dictionary), String> {
    let config = PageConfig {
        target_page_size: page_size.unwrap_or(2000),
    };
    let mut dict = Dictionary::new();

    // 1. Assign pages from summaries
    let mut page_index = assign_pages(summaries, &config);

    // 2. Shadow pages for external link targets
    let external_targets: Vec<String> = all_reference_ids
        .iter()
        .filter(|id| !page_index.resource_to_page.contains_key(id.as_str()))
        .cloned()
        .collect();
    if !external_targets.is_empty() {
        let shadow_index = assign_shadow_pages(
            &external_targets,
            page_index.page_meta.len() as u32,
            &config,
        );
        page_index.resource_to_page.extend(shadow_index.resource_to_page);
        page_index.page_meta.extend(shadow_index.page_meta);
    }

    // 3. Concept indexes
    let (concept_interval_index, concept_bytes) = if !vocabulary_collections.is_empty() {
        let prefix = concept_prefix(base_uri);
        for coll in vocabulary_collections {
            intern_concept_uris(&coll.concepts, &prefix, &mut dict);
        }
        let (ci, ct) = build_concept_indexes(vocabulary_collections, &dict, base_uri);

        let hierarchy_json = serde_json::to_string_pretty(vocabulary_collections)
            .map_err(|e| format!("Failed to serialize concept hierarchy: {e}"))?;
        let bytes = (ci.to_bytes(), ct.to_bytes(), hierarchy_json.into_bytes());
        (Some(ci), Some(bytes))
    } else {
        (None, None)
    };

    Ok((
        BuildRouting {
            page_index,
            concept_interval_index,
            concept_bytes,
        },
        dict,
    ))
}

/// Process one batch of resources, updating the accumulator.
///
/// For each resource: builds page records and summary quads, serializes
/// tile blobs, collects resource names and metadata, and streams
/// N-Triples to disk. Resources with unknown graph IDs or missing page
/// assignments are skipped.
pub fn process_resource_batch(
    resources: &[StaticResource],
    graphs: &HashMap<String, StaticGraph>,
    routing: &BuildRouting,
    base_uri: &str,
    acc: &mut BuildAccumulator,
) -> Result<(), String> {
    for resource in resources {
        let graph = match graphs.get(&resource.resourceinstance.graph_id) {
            Some(g) => g,
            None => continue,
        };
        let rid = &resource.resourceinstance.resourceinstanceid;
        let tiles = resource.tiles.as_deref().unwrap_or_default();
        let page_id = match routing.page_index.resource_to_page.get(rid.as_str()) {
            Some(&p) => p,
            None => continue,
        };

        if !resource.resourceinstance.name.is_empty() {
            acc.resource_names.insert(rid.clone(), resource.resourceinstance.name.clone());
        }

        let subject_id = build_records_for_resource(
            rid, tiles, graph, page_id, &mut acc.dict, base_uri,
            &routing.page_index.resource_to_page,
            routing.concept_interval_index.as_ref(),
            &mut acc.page_records, &mut acc.summary_builder,
        );

        // v2 tile blob with cache + scopes
        #[derive(Serialize)]
        struct ResourceBlob<'a> {
            tiles: &'a [StaticTile],
            #[serde(skip_serializing_if = "Option::is_none", rename = "__cache")]
            cache: Option<&'a serde_json::Value>,
            #[serde(skip_serializing_if = "Option::is_none", rename = "__scopes")]
            scopes: Option<&'a serde_json::Value>,
        }
        let blob = ResourceBlob {
            tiles,
            cache: resource.cache.as_ref(),
            scopes: resource.scopes.as_ref(),
        };
        if let Ok(tile_bytes) = rmp_serde::to_vec_named(&blob) {
            if !tile_bytes.is_empty() {
                acc.tile_content.entry(page_id).or_default().push((subject_id, tile_bytes));
            }
        }

        // Per-page resource metadata
        let dict_id = acc.dict.lookup(&resource_uri(base_uri, rid)).unwrap_or(subject_id);
        let name = resource.resourceinstance.descriptors.name.as_deref()
            .unwrap_or(&resource.resourceinstance.name)
            .to_string();
        let slug = resource.resourceinstance.descriptors.slug.clone().unwrap_or_default();
        let model = graphs.get(&resource.resourceinstance.graph_id)
            .map(|g| g.name.get("en"))
            .unwrap_or_default();
        acc.page_resource_meta.entry(page_id).or_default().push(ResourceMeta {
            dict_id, name, slug, model,
        });

        // Stream N-Triples
        if let Ok((triples, _)) = resource_to_triples(graph, rid, tiles, base_uri) {
            for triple in &triples {
                let line = triple.to_ntriples();
                acc.ntriples_bytes += line.len() + 1;
                writeln!(acc.nt_writer, "{}", line)
                    .map_err(|e| format!("Failed to write N-Triple: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Write all accumulated data to disk.
///
/// Writes page files, tile files, summary, dictionary, resource map,
/// page metadata, resource names, and concept files. Does not need
/// access to the original resources.
pub fn finalize_build(
    output_dir: &Path,
    routing: &BuildRouting,
    acc: BuildAccumulator,
    base_uri: &str,
) -> Result<BuildStats, String> {
    // Destructure accumulator for ownership / partial drops
    let BuildAccumulator {
        dict,
        summary_builder,
        page_records,
        mut tile_content,
        resource_names,
        page_resource_meta,
        mut nt_writer,
        ntriples_bytes,
    } = acc;

    // Flush N-Triples stream
    nt_writer.flush()
        .map_err(|e| format!("Failed to flush all.nt: {e}"))?;
    drop(nt_writer);

    // Ensure output directories exist
    let pages_dir = output_dir.join("pages");
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&pages_dir)
        .map_err(|e| format!("Failed to create pages dir: {e}"))?;
    std::fs::create_dir_all(&tiles_dir)
        .map_err(|e| format!("Failed to create tiles dir: {e}"))?;

    let has_meta = !page_resource_meta.is_empty();

    // Write page files
    let live_page_ids: HashSet<u32> = page_records.keys().copied().collect();
    let mut page_count = 0usize;
    let mut page_total_bytes = 0usize;
    for pm in &routing.page_index.page_meta {
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
            let meta = if has_meta {
                page_resource_meta.get(&pm.page_id).map(|v| v.as_slice()).unwrap_or(&[])
            } else {
                &[]
            };
            let page_bytes = write_page_file(&mut blocks, meta);
            let path = pages_dir.join(format!("page_{:04}.dat", pm.page_id));
            std::fs::write(&path, &page_bytes)
                .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
            page_count += 1;
            page_total_bytes += page_bytes.len();
        }
    }
    drop(page_records);
    drop(page_resource_meta);

    // Write tile files
    let mut tile_count = 0usize;
    let mut tile_total_bytes = 0usize;
    for pm in &routing.page_index.page_meta {
        if let Some(mut entries) = tile_content.remove(&pm.page_id) {
            entries.sort_by_key(|(sid, _)| *sid);
            let tile_bytes = write_tile_content_file(&entries);
            let path = tiles_dir.join(format!("tile_{:04}.dat", pm.page_id));
            std::fs::write(&path, &tile_bytes)
                .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
            tile_count += 1;
            tile_total_bytes += tile_bytes.len();
        }
    }
    drop(tile_content);

    // Write summary
    let summary_quads = summary_builder.build();
    let summary_bytes_data = serialize_summary(&summary_quads);
    let summary_bytes = summary_bytes_data.len();
    std::fs::write(output_dir.join("summary.bin"), &summary_bytes_data)
        .map_err(|e| format!("Failed to write summary.bin: {e}"))?;
    drop(summary_bytes_data);

    // Write dictionary
    let dict_bytes_data = dict.to_bytes();
    let dictionary_bytes = dict_bytes_data.len();
    std::fs::write(output_dir.join("dictionary.bin"), &dict_bytes_data)
        .map_err(|e| format!("Failed to write dictionary.bin: {e}"))?;
    drop(dict_bytes_data);

    // Write resource map
    let resource_map = ResourceMap::build(&dict, &routing.page_index.resource_to_page, base_uri);
    let rmap_bytes_data = resource_map.to_bytes();
    let resource_map_bytes = rmap_bytes_data.len();
    std::fs::write(output_dir.join("resource_map.bin"), &rmap_bytes_data)
        .map_err(|e| format!("Failed to write resource_map.bin: {e}"))?;
    drop(rmap_bytes_data);

    // Write page_meta (only pages that had records)
    let live_page_meta: Vec<_> = routing.page_index.page_meta.iter()
        .filter(|pm| live_page_ids.contains(&pm.page_id))
        .collect();
    let page_meta_json = serde_json::to_string_pretty(&live_page_meta)
        .map_err(|e| format!("Failed to serialize page meta: {e}"))?;
    let page_meta_bytes = page_meta_json.len();
    std::fs::write(output_dir.join("page_meta.json"), page_meta_json.as_bytes())
        .map_err(|e| format!("Failed to write page_meta.json: {e}"))?;

    // Write resource names
    let names_json = serde_json::to_string(&resource_names)
        .map_err(|e| format!("Failed to serialize resource names: {e}"))?;
    std::fs::write(output_dir.join("resource_names.json"), names_json.as_bytes())
        .map_err(|e| format!("Failed to write resource_names.json: {e}"))?;

    // Write concept files (pre-serialized during prepare_routing)
    if let Some((intervals_bytes, tree_bytes, hierarchy_json)) = &routing.concept_bytes {
        std::fs::write(output_dir.join("concept_intervals.bin"), intervals_bytes)
            .map_err(|e| format!("Failed to write concept_intervals.bin: {e}"))?;
        std::fs::write(output_dir.join("concept_tree.bin"), tree_bytes)
            .map_err(|e| format!("Failed to write concept_tree.bin: {e}"))?;
        std::fs::write(output_dir.join("concept_hierarchy.json"), hierarchy_json)
            .map_err(|e| format!("Failed to write concept_hierarchy.json: {e}"))?;
    }

    Ok(BuildStats {
        summary_bytes,
        dictionary_bytes,
        resource_map_bytes,
        page_meta_bytes,
        page_count,
        page_total_bytes,
        tile_count,
        tile_total_bytes,
        ntriples_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_minimal_resource(id: &str, graph_id: &str) -> StaticResource {
        use alizarin_core::graph::{StaticResourceDescriptors, StaticResourceMetadata};
        StaticResource {
            resourceinstance: StaticResourceMetadata {
                descriptors: StaticResourceDescriptors::default(),
                graph_id: graph_id.to_string(),
                name: format!("Resource {id}"),
                resourceinstanceid: id.to_string(),
                publication_id: None,
                principaluser_id: None,
                legacyid: None,
                graph_publication_id: None,
                createdtime: None,
                lastmodified: None,
            },
            tiles: Some(Vec::new()),
            metadata: HashMap::new(),
            cache: None,
            scopes: None,
            tiles_loaded: None,
        }
    }

    #[test]
    fn test_build_to_memory_empty() {
        let artifacts = build_to_memory(
            "https://example.org/",
            &HashMap::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(artifacts.contains_key("summary.bin"));
        assert!(artifacts.contains_key("dictionary.bin"));
        assert!(artifacts.contains_key("resource_map.bin"));
        assert!(artifacts.contains_key("page_meta.json"));
        assert!(artifacts.contains_key("resource_names.json"));
        assert!(artifacts.contains_key("all.nt"));
        // No concept files when no vocabulary
        assert!(!artifacts.contains_key("concept_intervals.bin"));
        assert!(!artifacts.contains_key("concept_tree.bin"));
    }

    #[test]
    fn test_build_to_memory_with_resources() {
        // Minimal graph — just enough to be valid. Nodes/edges are empty so
        // no quantized records are produced, but page assignment, dictionary,
        // and resource map should still work.
        let graph_json = r#"{
            "graphid": "test-graph",
            "name": {"en": "Test Graph"},
            "nodes": [],
            "nodegroups": [],
            "edges": [],
            "root": {
                "nodeid": "root-node",
                "name": "",
                "datatype": "semantic",
                "nodegroup_id": "",
                "graph_id": "test-graph",
                "istopnode": true
            }
        }"#;
        let mut graph: StaticGraph = serde_json::from_str(graph_json).unwrap();
        graph.build_indices();

        let mut graphs = HashMap::new();
        graphs.insert("test-graph".to_string(), graph);

        let resources = vec![
            make_minimal_resource("aaa-111", "test-graph"),
            make_minimal_resource("bbb-222", "test-graph"),
        ];

        let artifacts = build_to_memory(
            "https://example.org/",
            &graphs,
            &resources,
            &[],
            None,
        )
        .unwrap();

        // Core artifacts present and non-empty
        assert!(!artifacts["summary.bin"].is_empty());
        assert!(!artifacts["dictionary.bin"].is_empty());
        assert!(!artifacts["resource_map.bin"].is_empty());
        assert!(artifacts.contains_key("page_meta.json"));

        // resource_names should have 2 entries
        let names: HashMap<String, String> =
            serde_json::from_slice(&artifacts["resource_names.json"]).unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names["aaa-111"], "Resource aaa-111");

        // page_meta is valid JSON (may be empty — resources have no tiles so no records)
        let page_meta: Vec<serde_json::Value> =
            serde_json::from_slice(&artifacts["page_meta.json"]).unwrap();
        // With empty tiles, no quantized records are produced, so all pages are filtered
        assert!(page_meta.is_empty());

        // N-Triples export should contain graph schema triples
        let nt = std::str::from_utf8(&artifacts["all.nt"]).unwrap();
        assert!(nt.contains("example.org"));
    }

    #[test]
    fn test_build_to_disk_empty() {
        let dir = tempfile::tempdir().unwrap();
        let stats = build_to_disk(
            dir.path(),
            "https://example.org/",
            &HashMap::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(dir.path().join("summary.bin").exists());
        assert!(dir.path().join("dictionary.bin").exists());
        assert!(dir.path().join("resource_map.bin").exists());
        assert!(dir.path().join("page_meta.json").exists());
        assert!(dir.path().join("resource_names.json").exists());
        assert!(dir.path().join("all.nt").exists());
        assert_eq!(stats.page_count, 0);
        assert_eq!(stats.tile_count, 0);
    }

    #[test]
    fn test_build_to_disk_matches_build_to_memory() {
        let graph_json = r#"{
            "graphid": "test-graph",
            "name": {"en": "Test Graph"},
            "nodes": [],
            "nodegroups": [],
            "edges": [],
            "root": {
                "nodeid": "root-node",
                "name": "",
                "datatype": "semantic",
                "nodegroup_id": "",
                "graph_id": "test-graph",
                "istopnode": true
            }
        }"#;
        let mut graph: StaticGraph = serde_json::from_str(graph_json).unwrap();
        graph.build_indices();

        let mut graphs = HashMap::new();
        graphs.insert("test-graph".to_string(), graph);

        let resources = vec![
            make_minimal_resource("aaa-111", "test-graph"),
            make_minimal_resource("bbb-222", "test-graph"),
        ];

        // Build both ways
        let artifacts = build_to_memory(
            "https://example.org/",
            &graphs,
            &resources,
            &[],
            None,
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let stats = build_to_disk(
            dir.path(),
            "https://example.org/",
            &graphs,
            &resources,
            &[],
            None,
        )
        .unwrap();

        // Core files should match byte-for-byte
        assert_eq!(
            std::fs::read(dir.path().join("summary.bin")).unwrap(),
            artifacts["summary.bin"],
        );
        assert_eq!(
            std::fs::read(dir.path().join("dictionary.bin")).unwrap(),
            artifacts["dictionary.bin"],
        );
        assert_eq!(
            std::fs::read(dir.path().join("resource_map.bin")).unwrap(),
            artifacts["resource_map.bin"],
        );

        // resource_names should match
        let disk_names: HashMap<String, String> = serde_json::from_slice(
            &std::fs::read(dir.path().join("resource_names.json")).unwrap(),
        )
        .unwrap();
        let mem_names: HashMap<String, String> =
            serde_json::from_slice(&artifacts["resource_names.json"]).unwrap();
        assert_eq!(disk_names, mem_names);

        // Stats should be consistent
        assert_eq!(stats.summary_bytes, artifacts["summary.bin"].len());
        assert_eq!(stats.dictionary_bytes, artifacts["dictionary.bin"].len());
    }

    #[test]
    fn test_streaming_matches_build_to_disk() {
        use crate::page_assignment::ResourceSummary;

        let graph_json = r#"{
            "graphid": "test-graph",
            "name": {"en": "Test Graph"},
            "nodes": [],
            "nodegroups": [],
            "edges": [],
            "root": {
                "nodeid": "root-node",
                "name": "",
                "datatype": "semantic",
                "nodegroup_id": "",
                "graph_id": "test-graph",
                "istopnode": true
            }
        }"#;
        let mut graph: StaticGraph = serde_json::from_str(graph_json).unwrap();
        graph.build_indices();

        let mut graphs = HashMap::new();
        graphs.insert("test-graph".to_string(), graph);

        let resources = vec![
            make_minimal_resource("aaa-111", "test-graph"),
            make_minimal_resource("bbb-222", "test-graph"),
        ];

        // Build via build_to_disk
        let dir1 = tempfile::tempdir().unwrap();
        let stats1 = build_to_disk(
            dir1.path(), "https://example.org/", &graphs, &resources, &[], None,
        ).unwrap();

        // Build via streaming path
        let dir2 = tempfile::tempdir().unwrap();

        // Pass 1: extract summaries + reference IDs
        let mut summaries = Vec::new();
        let mut all_refs = HashSet::new();
        for r in &resources {
            let tiles = r.tiles.as_deref().unwrap_or_default();
            let (centroid, concept_ids) = graphs
                .get(&r.resourceinstance.graph_id)
                .map(|g| extract_resource_summary(tiles, g))
                .unwrap_or_default();
            summaries.push(ResourceSummary {
                resource_id: r.resourceinstance.resourceinstanceid.clone(),
                graph_id: r.resourceinstance.graph_id.clone(),
                centroid,
                concept_ids,
            });
            if let Some(graph) = graphs.get(&r.resourceinstance.graph_id) {
                for ref_id in collect_reference_ids(tiles, graph) {
                    all_refs.insert(ref_id);
                }
            }
        }

        // Routing
        let (routing, dict) = prepare_routing(
            &summaries, &all_refs, &[], "https://example.org/", None,
        ).unwrap();

        // Pass 2
        let mut acc = BuildAccumulator::new(dict, dir2.path()).unwrap();
        acc.write_schema_triples(&graphs, "https://example.org/").unwrap();
        process_resource_batch(&resources, &graphs, &routing, "https://example.org/", &mut acc).unwrap();

        // Finalize
        let stats2 = finalize_build(dir2.path(), &routing, acc, "https://example.org/").unwrap();

        // Core binary files should match byte-for-byte
        assert_eq!(
            std::fs::read(dir1.path().join("summary.bin")).unwrap(),
            std::fs::read(dir2.path().join("summary.bin")).unwrap(),
        );
        assert_eq!(
            std::fs::read(dir1.path().join("dictionary.bin")).unwrap(),
            std::fs::read(dir2.path().join("dictionary.bin")).unwrap(),
        );
        assert_eq!(
            std::fs::read(dir1.path().join("resource_map.bin")).unwrap(),
            std::fs::read(dir2.path().join("resource_map.bin")).unwrap(),
        );

        // Resource names should match
        let names1: HashMap<String, String> = serde_json::from_slice(
            &std::fs::read(dir1.path().join("resource_names.json")).unwrap(),
        ).unwrap();
        let names2: HashMap<String, String> = serde_json::from_slice(
            &std::fs::read(dir2.path().join("resource_names.json")).unwrap(),
        ).unwrap();
        assert_eq!(names1, names2);

        // N-Triples should match
        let nt1 = std::fs::read_to_string(dir1.path().join("all.nt")).unwrap();
        let nt2 = std::fs::read_to_string(dir2.path().join("all.nt")).unwrap();
        assert_eq!(nt1, nt2);

        // Stats should be consistent
        assert_eq!(stats1.summary_bytes, stats2.summary_bytes);
        assert_eq!(stats1.dictionary_bytes, stats2.dictionary_bytes);
        assert_eq!(stats1.resource_map_bytes, stats2.resource_map_bytes);
        assert_eq!(stats1.page_count, stats2.page_count);
        assert_eq!(stats1.tile_count, stats2.tile_count);
        assert_eq!(stats1.ntriples_bytes, stats2.ntriples_bytes);
    }
}
