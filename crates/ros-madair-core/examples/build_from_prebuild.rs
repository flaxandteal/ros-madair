// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Build RosMadair index from a Arches prebuild export.
//!
//! Uses alizarin-core's PrebuildLoader to read graph definitions and
//! business_data JSON (with tiles), then builds the page-based static
//! index for browser-side SPARQL queries.
//!
//! Usage:
//!   cargo run --example build_from_prebuild -- /path/to/prebuild [output_dir] [page_size]
//!
//! The prebuild directory should have the standard starches-builder layout:
//!   prebuild/
//!     graphs/resource_models/*.json
//!     business_data/**/*.json

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

use alizarin_core::graph::{IndexedGraph, StaticResource};
use alizarin_core::loader::PrebuildLoader;

use ros_madair_core::{
    assign_pages, extract_centroid, graph_schema_to_triples, quantize_tile_value,
    quantize_type_for_datatype, serialize_summary, triples_to_ntriples, write_page_file,
    Dictionary, PageConfig, PageRecord, PredicateBlock, QuantizeType, ResourceMap, ResourceMeta,
    SummaryBuilder, page_assignment::ResourceSummary,
};
use ros_madair_core::datatype_class::{classify_datatype, DatatypeClass};
use ros_madair_core::uri::resource_prefix;
use ros_madair_core::value_extract;

fn main() {
    let all_args: Vec<String> = std::env::args().collect();
    let debug = all_args.iter().any(|a| a == "--debug");
    let args: Vec<&String> = all_args.iter().filter(|a| !a.starts_with("--")).collect();
    if args.len() < 2 {
        eprintln!("Usage: build_from_prebuild [--debug] <prebuild_dir> [output_dir] [page_size] [base_uri] [bd_file]");
        eprintln!("");
        eprintln!("  prebuild_dir  Path to Arches prebuild export");
        eprintln!("  output_dir    Output directory (default: example/static/index)");
        eprintln!("  page_size     Resources per page (default: 2000)");
        eprintln!("  base_uri      RDF base URI (default: https://example.org/)");
        eprintln!("  bd_file       Only process this business_data file (optional)");
        eprintln!("  --debug       Show detailed progress logging");
        std::process::exit(1);
    }

    let prebuild_dir = args[1].as_str();
    let output_dir = args.get(2).map(|s| s.as_str()).unwrap_or("example/static/index");
    let page_size: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);

    let output_path = Path::new(output_dir);
    fs::create_dir_all(output_path.join("pages")).expect("Failed to create output dir");

    let base_uri: &str = args.get(4).map(|s| s.as_str()).unwrap_or("https://example.org/");
    let base_uri = if base_uri.ends_with('/') {
        base_uri.to_string()
    } else {
        format!("{base_uri}/")
    };
    let base_uri = base_uri.as_str();

    // Load graphs and resources via alizarin PrebuildLoader
    let loader = PrebuildLoader::new(prebuild_dir).unwrap_or_else(|e| {
        eprintln!("Failed to open prebuild dir '{}': {}", prebuild_dir, e);
        std::process::exit(1);
    });

    let info = loader.get_info().unwrap();
    println!("Prebuild directory: {}", prebuild_dir);
    println!("  Graphs: {} files", info.graph_files.len());
    println!("  Has business_data: {}", info.has_business_data);

    // Load all graphs as IndexedGraphs (needed for node lookup and descriptor building)
    let graphs_by_id: HashMap<String, IndexedGraph> = loader
        .load_graphs_by_id()
        .unwrap_or_else(|e| {
            eprintln!("Failed to load graphs: {}", e);
            std::process::exit(1);
        });
    println!("Loaded {} graphs:", graphs_by_id.len());
    for (id, ig) in &graphs_by_id {
        let name = ig.graph.name.get("en");
        println!("  {} — {}", id, name);
    }

    // Load all full resources (with tiles) via alizarin, per graph per file
    let mut all_summaries: Vec<ResourceSummary> = Vec::new();
    let mut all_resources: Vec<StaticResource> = Vec::new();
    let mut all_triples = Vec::new();

    let bd_file_filter = args.get(5).map(|s| s.as_str());
    if debug {
        println!("[debug] prebuild_dir={}, output_dir={}, page_size={}, base_uri={}", prebuild_dir, output_dir, page_size, base_uri);
        if let Some(f) = bd_file_filter {
            println!("[debug] bd_file filter: {}", f);
        }
    }
    let bd_files = if let Some(filename) = bd_file_filter {
        let path = Path::new(filename);
        if path.exists() {
            vec![path.to_path_buf()]
        } else {
            // Try relative to prebuild_dir
            let relative = Path::new(prebuild_dir).join(filename);
            if relative.exists() {
                vec![relative]
            } else {
                eprintln!("Business data file not found: {}", filename);
                std::process::exit(1);
            }
        }
    } else {
        loader.find_business_data_files().unwrap_or_default()
    };
    println!("\nLoading resources from {} business_data file(s)...", bd_files.len());

    for (file_idx, file_path) in bd_files.iter().enumerate() {
        let t0 = Instant::now();
        if debug {
            println!("[debug] Loading file {}/{}: {}", file_idx + 1, bd_files.len(), file_path.display());
        }

        // Read and parse the file once (across all graphs)
        let resources = match loader.load_all_full_resources_from_file(file_path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Warning: Failed to parse {}: {}", file_path.display(), e);
                continue;
            }
        };

        if debug {
            println!("[debug]   Loaded {} resources in {:.1?}", resources.len(), t0.elapsed());
        }

        let t1 = Instant::now();
        let mut file_resource_count = 0usize;
        let total = resources.len();
        for (res_idx, resource) in resources.into_iter().enumerate() {
            let graph_id = &resource.resourceinstance.graph_id;
            let graph = match graphs_by_id.get(graph_id.as_str()) {
                Some(g) => g,
                None => {
                    if debug && res_idx == 0 {
                        println!("[debug]   Skipping resources for unknown graph {}", graph_id);
                    }
                    continue;
                }
            };

            let resource_id = &resource.resourceinstance.resourceinstanceid;
            let tiles = resource.tiles.as_deref().unwrap_or_default();

            // Build resource summary for page assignment
            let mut centroid = None;
            let mut concept_ids = Vec::new();

            for tile in tiles {
                for (node_id, value) in &tile.data {
                    if value.is_null() {
                        continue;
                    }
                    let node = match graph.graph.get_node_by_id(node_id) {
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

            all_summaries.push(ResourceSummary {
                resource_id: resource_id.clone(),
                graph_id: graph_id.clone(),
                centroid,
                concept_ids,
            });

            file_resource_count += 1;
            all_resources.push(resource);

            if debug && (res_idx + 1) % 1000 == 0 {
                println!("[debug]   Processed {}/{} resources ({:.1?})", res_idx + 1, total, t1.elapsed());
            }
        }

        println!("  {} — {} resources ({:.1?})", file_path.file_name().unwrap_or_default().to_string_lossy(), file_resource_count, t0.elapsed());
    }

    println!("Loaded {} resources across {} graphs", all_resources.len(), graphs_by_id.len());

    if all_resources.is_empty() {
        eprintln!("No resources found. Check that business_data/ contains valid JSON files.");
        std::process::exit(1);
    }

    // Assign pages
    let page_index = assign_pages(
        &all_summaries,
        &PageConfig {
            target_page_size: page_size,
        },
    );
    println!("Assigned to {} pages (target size: {})", page_index.page_meta.len(), page_size);

    // Build page records + summary quads
    let mut dict = Dictionary::new();
    let mut summary_builder = SummaryBuilder::new();
    let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();
    let mut resource_names: HashMap<String, String> = HashMap::new();

    for resource in &all_resources {
        let resource_id = &resource.resourceinstance.resourceinstanceid;
        let graph_id = &resource.resourceinstance.graph_id;
        let tiles = resource.tiles.as_deref().unwrap_or_default();

        let page_id = match page_index.resource_to_page.get(resource_id.as_str()) {
            Some(&pid) => pid,
            None => continue,
        };
        let subject_id = dict.intern(&ros_madair_core::uri::resource_uri(base_uri, resource_id));

        let graph = match graphs_by_id.get(graph_id.as_str()) {
            Some(g) => g,
            None => continue,
        };

        // Store name from resource metadata
        if !resource.resourceinstance.name.is_empty() {
            resource_names.insert(resource_id.clone(), resource.resourceinstance.name.clone());
        }

        for tile in tiles {
            for (node_id, value) in &tile.data {
                if value.is_null() {
                    continue;
                }
                let node = match graph.graph.get_node_by_id(node_id) {
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
                let pred_id = dict.intern(&ros_madair_core::uri::node_uri(base_uri, alias));

                let object_vals = quantize_tile_value(value, qtype, &node.datatype, &mut dict, base_uri);

                for object_val in object_vals {
                    let record = PageRecord {
                        object_val,
                        subject_id,
                    };
                    page_records
                        .entry(page_id)
                        .or_default()
                        .push((pred_id, record));

                    // For resource-instance links, resolve to the target's
                    // page. Concepts/literals use a high sentinel so they
                    // never collide with real page IDs in the overview.
                    const NON_PAGE_SENTINEL: u32 = u32::MAX;
                    let page_o = match qtype {
                        QuantizeType::DictionaryId => {
                            if let Some(term) = dict.resolve(object_val) {
                                if let Some(rid) = term.strip_prefix(&resource_prefix(base_uri)) {
                                    page_index
                                        .resource_to_page
                                        .get(rid)
                                        .copied()
                                        .unwrap_or(NON_PAGE_SENTINEL)
                                } else {
                                    // Concept or other non-resource dict entry
                                    NON_PAGE_SENTINEL
                                }
                            } else {
                                NON_PAGE_SENTINEL
                            }
                        }
                        _ => object_val, // literal bucket (dates, geo, bool)
                    };
                    summary_builder.add(page_id, pred_id, page_o, subject_id);

                    // Emit reverse record on the target's page for resource-instance links
                    if page_o != NON_PAGE_SENTINEL && qtype == QuantizeType::DictionaryId {
                        let reverse_pred_id = dict.intern(&format!("!{}", ros_madair_core::uri::node_uri(base_uri, alias)));
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
    }

    // Generate RDF triples for the N-Triples export
    for ig in graphs_by_id.values() {
        if let Ok(schema_triples) = graph_schema_to_triples(&ig.graph, base_uri) {
            all_triples.extend(schema_triples);
        }
    }
    let mut triple_count = 0;
    for resource in &all_resources {
        let graph = match graphs_by_id.get(&resource.resourceinstance.graph_id) {
            Some(g) => g,
            None => continue,
        };
        let tiles = resource.tiles.as_deref().unwrap_or_default();
        match ros_madair_core::resource_to_triples(
            &graph.graph,
            &resource.resourceinstance.resourceinstanceid,
            tiles,
            base_uri,
        ) {
            Ok(triples) => {
                triple_count += triples.len();
                all_triples.extend(triples);
            }
            Err(_) => {}
        }
    }

    // Write outputs
    println!("\nWriting index...");

    let summary_quads = summary_builder.build();
    println!("  Summary: {} quads", summary_quads.len());
    let summary_bytes = serialize_summary(&summary_quads);
    fs::write(output_path.join("summary.bin"), &summary_bytes).unwrap();

    let dict_bytes = dict.to_bytes();
    println!("  Dictionary: {} terms ({} bytes)", dict.len(), dict_bytes.len());
    fs::write(output_path.join("dictionary.bin"), &dict_bytes).unwrap();

    let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, base_uri);
    let resource_map_bytes = resource_map.to_bytes();
    println!("  Resource map: {} entries ({} bytes)", resource_map.len(), resource_map_bytes.len());
    fs::write(output_path.join("resource_map.bin"), &resource_map_bytes).unwrap();

    // Build per-page resource metadata from alizarin-loaded descriptors.
    let mut page_resource_meta: HashMap<u32, Vec<ResourceMeta>> = HashMap::new();
    for resource in &all_resources {
        let resource_id = &resource.resourceinstance.resourceinstanceid;
        let graph_id = &resource.resourceinstance.graph_id;
        let page_id = match page_index.resource_to_page.get(resource_id.as_str()) {
            Some(&pid) => pid,
            None => continue,
        };
        let dict_id = match dict.lookup(&ros_madair_core::uri::resource_uri(base_uri, resource_id)) {
            Some(id) => id,
            None => continue,
        };
        let descriptors = &resource.resourceinstance.descriptors;
        let name = descriptors.name.as_deref()
            .unwrap_or(&resource.resourceinstance.name)
            .to_string();
        let slug = descriptors.slug.clone().unwrap_or_default();
        let model = graphs_by_id.get(graph_id.as_str())
            .map(|g| g.graph.name.get("en"))
            .unwrap_or_default();
        page_resource_meta.entry(page_id).or_default().push(ResourceMeta {
            dict_id,
            name,
            slug,
            model,
        });
    }

    // Only write pages that have records; filter page_meta to match.
    let mut live_page_meta: Vec<_> = Vec::new();
    let mut total_page_bytes = 0u64;
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
            let meta = page_resource_meta.get(&pm.page_id)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let page_bytes = write_page_file(&mut blocks, meta);
            total_page_bytes += page_bytes.len() as u64;
            fs::write(
                output_path.join(format!("pages/page_{:04}.dat", pm.page_id)),
                &page_bytes,
            )
            .unwrap();
            live_page_meta.push(pm.clone());
        }
    }
    let skipped = page_index.page_meta.len() - live_page_meta.len();
    if skipped > 0 {
        println!("  Skipped {} empty pages (no quantizable data)", skipped);
    }
    println!("  Pages: {} files ({} bytes total)", live_page_meta.len(), total_page_bytes);

    let page_meta_json = serde_json::to_string_pretty(&live_page_meta).unwrap();
    fs::write(output_path.join("page_meta.json"), &page_meta_json).unwrap();

    let names_json = serde_json::to_string(&resource_names).unwrap();
    fs::write(output_path.join("resource_names.json"), &names_json).unwrap();
    println!("  Resource names: {} entries ({} bytes)", resource_names.len(), names_json.len());

    let nt_output = triples_to_ntriples(&all_triples);
    fs::write(output_path.join("all.nt"), &nt_output).unwrap();
    println!("  N-Triples: {} triples ({} bytes)", triple_count, nt_output.len());

    println!("\nDone! Output: {}", output_dir);
    println!("  summary.bin       {} bytes", summary_bytes.len());
    println!("  dictionary.bin    {} bytes", dict_bytes.len());
    println!("  resource_map.bin  {} bytes", resource_map_bytes.len());
    println!("  page_meta.json    {} bytes", page_meta_json.len());
    println!("  pages/            {} files, {} bytes total", live_page_meta.len(), total_page_bytes);
    println!("  all.nt            {} bytes", nt_output.len());
}

