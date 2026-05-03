// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Diagnose the relations.ts flow against a real index.
//!
//! Mimics exactly what starches `relations.ts` does:
//!   1. page_for_resource(uri) → page_id
//!   2. summary_from_page(page_id) → quads
//!   3. filter quads to resource-link predicates
//!   4. load_predicate_records → records
//!   5. filter records by subject_id, check object is /resource/
//!
//! Usage:
//!   cargo run --example diagnose_relations -- <index_dir> <resource_uri>
//!
//! Example:
//!   cargo run --example diagnose_relations -- /path/to/index https://example.org/resource/some-uuid

use std::collections::HashSet;
use std::fs;

use ros_madair_core::{
    parse_page_header, parse_records, Dictionary, PageMeta, ResourceMap, SummaryIndex,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <index_dir> <resource_uri>", args[0]);
        eprintln!();
        eprintln!("If no resource_uri, pass 'all' to scan all resources.");
        std::process::exit(1);
    }

    let base = &args[1];
    let target_uri = &args[2];

    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();

    let rmap_bytes = fs::read(format!("{base}/resource_map.bin")).unwrap();
    let rmap = ResourceMap::from_bytes(&rmap_bytes).unwrap();

    let meta_str = fs::read_to_string(format!("{base}/page_meta.json")).unwrap();
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str).unwrap();
    let valid_pages: HashSet<u32> = page_meta.iter().map(|pm| pm.page_id).collect();

    println!("Index: {base}");
    println!("  Dictionary: {} terms", dict.len());
    println!("  Pages: {}", page_meta.len());
    println!("  Valid page IDs: {:?}", valid_pages);
    println!();

    // Count resource URIs and reverse predicates in dictionary
    let mut resource_count = 0;
    let mut reverse_pred_count = 0;
    for id in 0..dict.len() as u32 {
        if let Some(term) = dict.resolve(id) {
            if term.contains("/resource/") {
                resource_count += 1;
            }
            if term.starts_with('!') {
                reverse_pred_count += 1;
            }
        }
    }
    println!("  Resource URIs in dict: {resource_count}");
    println!("  Reverse predicates (!xxx) in dict: {reverse_pred_count}");

    // List all reverse predicates
    if reverse_pred_count > 0 {
        println!("  Reverse predicates:");
        for id in 0..dict.len() as u32 {
            if let Some(term) = dict.resolve(id) {
                if term.starts_with('!') {
                    println!("    [{id}] {term}");
                }
            }
        }
    }
    println!();

    if target_uri == "all" {
        // Scan all resources
        let mut total_relations = 0;
        for id in 0..dict.len() as u32 {
            if let Some(term) = dict.resolve(id) {
                if term.contains("/resource/") {
                    let count = diagnose_resource(
                        base, &dict, &summary, &rmap, &valid_pages, term, id, false,
                    );
                    if count > 0 {
                        println!("  {term}: {count} relations");
                        total_relations += count;
                    }
                }
            }
        }
        println!("\nTotal relations found across all resources: {total_relations}");
    } else {
        let dict_id = match dict.lookup(target_uri) {
            Some(id) => id,
            None => {
                eprintln!("ERROR: URI not in dictionary: {target_uri}");
                // Try to find similar
                for id in 0..dict.len() as u32 {
                    if let Some(term) = dict.resolve(id) {
                        if term.contains("/resource/") {
                            eprintln!("  Available: {term}");
                        }
                    }
                }
                std::process::exit(1);
            }
        };
        diagnose_resource(
            base, &dict, &summary, &rmap, &valid_pages, target_uri, dict_id, true,
        );
    }
}

fn diagnose_resource(
    base: &str,
    dict: &Dictionary,
    summary: &SummaryIndex,
    rmap: &ResourceMap,
    valid_pages: &HashSet<u32>,
    uri: &str,
    dict_id: u32,
    verbose: bool,
) -> usize {
    if verbose {
        println!("=== Diagnosing: {uri} (dict_id={dict_id}) ===\n");
    }

    // Step 1: page_for_resource
    let page_id = match rmap.page_for(dict_id) {
        Some(p) => p,
        None => {
            if verbose {
                println!("FAIL: resource not in resource_map");
            }
            return 0;
        }
    };
    if verbose {
        println!("Step 1: page_for_resource → page {page_id}");
    }

    // Step 2: summary_from_page
    let quads = summary.lookup_s(page_id);
    if verbose {
        println!("Step 2: summary_from_page({page_id}) → {} quads", quads.len());
        for q in quads {
            let pred = dict.resolve(q.predicate).unwrap_or("?");
            let page_o_valid = valid_pages.contains(&q.page_o);
            println!(
                "  page_s={} pred=[{}] {} page_o={} {} edges={} subjects={}",
                q.page_s,
                q.predicate,
                pred,
                q.page_o,
                if page_o_valid { "(VALID PAGE)" } else { "(not a page)" },
                q.edge_count,
                q.subject_count,
            );
        }
        println!();
    }

    // Step 3: filter to resource-link quads
    let resource_quads: Vec<_> = quads
        .iter()
        .filter(|q| {
            let pred = dict.resolve(q.predicate).unwrap_or("");
            pred.starts_with('!') || valid_pages.contains(&q.page_o)
        })
        .collect();

    if verbose {
        println!(
            "Step 3: filtered to {} resource-link quads (reverse or valid page_o)",
            resource_quads.len()
        );
        println!();
    }

    // Step 4+5: load records, filter by subject, check /resource/
    let mut found = 0;
    for quad in &resource_quads {
        let pred_uri = dict.resolve(quad.predicate).unwrap_or("?");
        let page_path = format!("{base}/pages/page_{:04}.dat", page_id);
        let data = match fs::read(&page_path) {
            Ok(d) => d,
            Err(e) => {
                if verbose {
                    println!("  FAIL: can't read {page_path}: {e}");
                }
                continue;
            }
        };
        let header = match parse_page_header(&data) {
            Ok(h) => h,
            Err(e) => {
                if verbose {
                    println!("  FAIL: can't parse page header: {e}");
                }
                continue;
            }
        };

        let entry = match header.entries.iter().find(|e| e.pred_id == quad.predicate) {
            Some(e) => e,
            None => {
                if verbose {
                    println!("  WARN: pred {pred_uri} not in page {page_id} header");
                }
                continue;
            }
        };

        let start = entry.offset as usize;
        let end = start + entry.record_count as usize * 8;
        let records = parse_records(&data[start..end]);

        if verbose {
            println!(
                "Step 4: load_predicate_records(page={page_id}, pred={pred_uri}) → {} records",
                records.len()
            );
        }

        for rec in &records {
            if rec.subject_id != dict_id {
                continue;
            }
            let obj = dict.resolve(rec.object_val);
            let is_resource = obj.map(|o| o.contains("/resource/")).unwrap_or(false);

            if verbose {
                println!(
                    "  subject_id={} matches! object_val={} → {} {}",
                    rec.subject_id,
                    rec.object_val,
                    obj.unwrap_or("(unresolvable)"),
                    if is_resource { "✓ RESOURCE LINK" } else { "✗ not a resource" },
                );
            }

            if is_resource {
                found += 1;
            }
        }
    }

    if verbose {
        println!("\n=== Result: {found} resource relations found ===");
    }
    found
}
