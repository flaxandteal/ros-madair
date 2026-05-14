// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Build RosMadair index from a Arches prebuild export.
//!
//! Uses alizarin-core's PrebuildLoader to read graph definitions and
//! business_data JSON (with tiles), then builds the page-based static
//! index for browser-side SPARQL queries.
//!
//! Two-pass streaming build:
//!   Pass 1 — extract lightweight ResourceSummary + reference IDs per file
//!   Pass 2 — process resources per file (records, tiles, N-Triples)
//!
//! Resources are loaded per-file and dropped between files, avoiding the
//! multi-GB peak from holding all resources in memory simultaneously.
//!
//! Usage:
//!   ros-madair-build [--debug] <prebuild_dir> [output_dir] [page_size] [base_uri] [bd_file]
//!
//! The prebuild directory should have the standard starches-builder layout:
//!   prebuild/
//!     graphs/resource_models/*.json
//!     business_data/**/*.json

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Instant;

use alizarin_core::graph::{IndexedGraph, StaticGraph};
use alizarin_core::loader::PrebuildLoader;

use ros_madair_core::{
    collect_reference_ids, extract_resource_summary, prepare_routing,
    process_resource_batch, finalize_build, BuildAccumulator, ResourceSummary,
};

fn main() {
    println!("ros-madair-build {}", env!("CARGO_PKG_VERSION"));

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

    let base_uri: &str = args.get(4).map(|s| s.as_str()).unwrap_or("https://example.org/");
    let base_uri = if base_uri.ends_with('/') {
        base_uri.to_string()
    } else {
        format!("{base_uri}/")
    };
    let base_uri = base_uri.as_str();

    println!("  rdf_base_uri: {base_uri}");

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

    // Consume IndexedGraphs → StaticGraphs early (needed for extract_resource_summary)
    let graphs: HashMap<String, StaticGraph> = graphs_by_id.into_iter()
        .map(|(id, ig)| (id, ig.graph))
        .collect();

    // Load SKOS vocabularies
    let all_collections = loader.load_collections(base_uri).unwrap_or_else(|e| {
        eprintln!("  Warning: Failed to load SKOS collections: {}", e);
        Vec::new()
    });
    let vocab_file_count = loader.find_collection_files().map(|f| f.len()).unwrap_or(0);

    // Prepare business_data file list
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
    // Skip files starting with _ (e.g. _all.json)
    let bd_files: Vec<_> = bd_files.into_iter().filter(|p| {
        let dominated = p.file_name()
            .and_then(|f| f.to_str())
            .map(|f| f.starts_with('_'))
            .unwrap_or(false);
        if dominated {
            println!("  Skipping {}", p.display());
        }
        !dominated
    }).collect();

    // -----------------------------------------------------------------------
    // Pass 1: extract lightweight summaries + reference IDs per file
    // -----------------------------------------------------------------------
    println!("\nPass 1: extracting summaries from {} file(s)...", bd_files.len());
    let t_pass1 = Instant::now();
    let mut summaries: Vec<ResourceSummary> = Vec::new();
    let mut all_reference_ids: HashSet<String> = HashSet::new();
    let mut total_resource_count = 0usize;

    for (file_idx, file_path) in bd_files.iter().enumerate() {
        let t0 = Instant::now();
        if debug {
            println!("[debug] Pass 1 file {}/{}: {}", file_idx + 1, bd_files.len(), file_path.display());
        }

        let resources = match loader.load_all_full_resources_from_file(file_path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Warning: Failed to parse {}: {}", file_path.display(), e);
                continue;
            }
        };

        let mut file_count = 0usize;
        for resource in &resources {
            let graph = match graphs.get(&resource.resourceinstance.graph_id) {
                Some(g) => g,
                None => continue,
            };
            let tiles = resource.tiles.as_deref().unwrap_or_default();

            let (centroid, concept_ids) = extract_resource_summary(tiles, graph);
            summaries.push(ResourceSummary {
                resource_id: resource.resourceinstance.resourceinstanceid.clone(),
                graph_id: resource.resourceinstance.graph_id.clone(),
                centroid,
                concept_ids,
            });

            for ref_id in collect_reference_ids(tiles, graph) {
                all_reference_ids.insert(ref_id);
            }

            file_count += 1;
        }
        // resources dropped here — full tile data freed

        total_resource_count += file_count;
        println!("  {} — {} resources ({:.1?})",
            file_path.file_name().unwrap_or_default().to_string_lossy(),
            file_count, t0.elapsed());
    }

    println!("Pass 1 done: {} resources, {} reference IDs ({:.1?})",
        total_resource_count, all_reference_ids.len(), t_pass1.elapsed());

    if total_resource_count == 0 {
        eprintln!("No resources found. Check that business_data/ contains valid JSON files.");
        std::process::exit(1);
    }

    // -----------------------------------------------------------------------
    // Compute routing (page assignments + concept indexes)
    // -----------------------------------------------------------------------
    let t_routing = Instant::now();
    println!("\nComputing page assignments...");
    let (routing, dict) = prepare_routing(
        &summaries, &all_reference_ids, &all_collections, base_uri, Some(page_size),
    ).unwrap_or_else(|e| {
        eprintln!("Routing failed: {}", e);
        std::process::exit(1);
    });
    println!("  {} pages assigned ({:.1?})", routing.page_index.page_meta.len(), t_routing.elapsed());

    // Free Pass 1 intermediates
    drop(summaries);
    drop(all_reference_ids);

    // -----------------------------------------------------------------------
    // Pass 2: process resources per file (records, tiles, N-Triples)
    // -----------------------------------------------------------------------
    let t_pass2 = Instant::now();
    println!("\nPass 2: building index (page_size={})...", page_size);
    fs::create_dir_all(output_path).expect("Failed to create output dir");

    let mut acc = BuildAccumulator::new(dict, output_path).unwrap_or_else(|e| {
        eprintln!("Failed to create accumulator: {}", e);
        std::process::exit(1);
    });

    // Write schema triples first (from graphs, no resources needed)
    acc.write_schema_triples(&graphs, base_uri).unwrap_or_else(|e| {
        eprintln!("Failed to write schema triples: {}", e);
        std::process::exit(1);
    });

    for (file_idx, file_path) in bd_files.iter().enumerate() {
        let t0 = Instant::now();
        if debug {
            println!("[debug] Pass 2 file {}/{}: {}", file_idx + 1, bd_files.len(), file_path.display());
        }

        let resources = match loader.load_all_full_resources_from_file(file_path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Warning: Failed to parse {}: {}", file_path.display(), e);
                continue;
            }
        };

        let count = resources.len();
        process_resource_batch(&resources, &graphs, &routing, base_uri, &mut acc)
            .unwrap_or_else(|e| {
                eprintln!("Failed processing {}: {}", file_path.display(), e);
                std::process::exit(1);
            });
        // resources dropped here — full tile data freed

        if debug {
            println!("[debug]   Processed {} resources ({:.1?})", count, t0.elapsed());
        }
    }
    println!("  Pass 2 done ({:.1?})", t_pass2.elapsed());

    // -----------------------------------------------------------------------
    // Finalize: write all accumulated data to disk
    // -----------------------------------------------------------------------
    let t_finalize = Instant::now();
    let stats = finalize_build(output_path, &routing, acc, base_uri)
        .unwrap_or_else(|e| {
            eprintln!("Finalize failed: {}", e);
            std::process::exit(1);
        });
    println!("  Finalized ({:.1?})", t_finalize.elapsed());

    // Copy graph definitions to output for schema registration by downstream consumers
    let graphs_out = output_path.join("graphs");
    fs::create_dir_all(&graphs_out).expect("Failed to create graphs dir");
    let graphs_src = Path::new(prebuild_dir).join("graphs").join("resource_models");
    if graphs_src.is_dir() {
        for entry in fs::read_dir(&graphs_src).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let dest = graphs_out.join(entry.file_name());
                fs::copy(&path, &dest).unwrap();
            }
        }
        println!("  Graphs: copied from {}", graphs_src.display());
    } else {
        println!("  Graphs: WARNING — no graphs/resource_models/ found in prebuild");
    }

    // Copy vocabulary XML files to output
    if let Ok(vocab_files) = loader.find_collection_files() {
        if !vocab_files.is_empty() {
            let vocabs_out = output_path.join("vocabularies");
            fs::create_dir_all(&vocabs_out).expect("Failed to create vocabularies dir");

            for path in &vocab_files {
                if path.extension().and_then(|e| e.to_str()) == Some("xml") {
                    let dest = vocabs_out.join(path.file_name().unwrap());
                    fs::copy(path, &dest).unwrap_or_default();
                }
            }
        }
        println!(
            "  Vocabularies: {} files, {} collections",
            vocab_file_count, all_collections.len()
        );
    } else {
        println!("  Vocabularies: no reference_data/ found in prebuild");
    }

    // Print summary
    println!("\nDone! Output: {}", output_dir);
    println!("  summary.bin       {} bytes", stats.summary_bytes);
    println!("  dictionary.bin    {} bytes", stats.dictionary_bytes);
    println!("  resource_map.bin  {} bytes", stats.resource_map_bytes);
    println!("  page_meta.json    {} bytes", stats.page_meta_bytes);
    println!("  pages/            {} files, {} bytes total", stats.page_count, stats.page_total_bytes);
    println!("  tiles/            {} files, {} bytes total", stats.tile_count, stats.tile_total_bytes);
    println!("  all.nt            {} bytes", stats.ntriples_bytes);
}
