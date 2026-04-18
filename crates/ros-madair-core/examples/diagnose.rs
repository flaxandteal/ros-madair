use std::collections::{HashMap, HashSet};
use std::fs;

use ros_madair_core::{
    binary_search_object, full_header_size, parse_page_header, parse_records,
    Dictionary, PageMeta, PageRecord, SummaryIndex,
};

fn main() {
    let base = "example/static/ros-madair";

    // Load dictionary
    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    println!("=== Dictionary ({} terms) ===", dict.len());
    // Dump all terms
    for id in 0..dict.len() as u32 {
        if let Some(uri) = dict.resolve(id) {
            println!("  {} → {:?}", id, uri);
        }
    }

    // Load summary
    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();
    println!("\n=== Summary ({} quads) ===", summary.len());

    // Load page meta
    let meta_str = fs::read_to_string(format!("{base}/page_meta.json")).unwrap();
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str).unwrap();
    println!("\n=== Page Meta ===");
    for pm in &page_meta {
        println!("  page {} — {} resources, bbox {:?}", pm.page_id, pm.resource_count, pm.bbox);
    }

    // Simulate query: ?place monument_type church
    let pred_uri = "https://example.org/node/monument_type";
    let obj_uri = "https://example.org/concept/church";

    let pred_id = dict.lookup(pred_uri);
    let obj_id = dict.lookup(obj_uri);
    println!("\n=== Query ===");
    println!("pred '{}' → id {:?}", pred_uri, pred_id);
    println!("obj  '{}' → id {:?}", obj_uri, obj_id);

    // === Simulate EXACT client flow ===
    println!("\n=== Simulating WASM client flow ===");
    simulate_client_query(base, &dict, &summary, &page_meta,
        r#"[{"s": "?place", "p": "https://example.org/node/monument_type", "o": "https://example.org/concept/church"}]"#);

    simulate_client_query(base, &dict, &summary, &page_meta,
        r#"[{"s": "?person", "p": "https://example.org/node/occupation", "o": "https://example.org/concept/architect"},{"s": "?person", "p": "https://example.org/node/lived_in", "o": "https://example.org/concept/bangor"}]"#);

    if let (Some(pred_id), Some(obj_id)) = (pred_id, obj_id) {
        // OPS lookup
        let op_quads = summary.lookup_op(obj_id, pred_id);
        println!("\nOPS lookup (obj={}, pred={}) → {} quads", obj_id, pred_id, op_quads.len());
        for q in op_quads {
            println!("  page_s={}, pred={}, page_o={}, edges={}, subjects={}",
                q.page_s, q.predicate, q.page_o, q.edge_count, q.subject_count);
        }

        // P lookup (broader)
        let p_quads = summary.lookup_p(pred_id);
        println!("\nP lookup (pred={}) → {} quads", pred_id, p_quads.len());
        for q in p_quads {
            println!("  page_s={}, pred={}, page_o={}, edges={}, subjects={}",
                q.page_s, q.predicate, q.page_o, q.edge_count, q.subject_count);
        }

        // Load page files and search
        println!("\n=== Page Records ===");
        for pm in &page_meta {
            let path = format!("{base}/pages/page_{:04}.dat", pm.page_id);
            let data = fs::read(&path).unwrap();
            let header = parse_page_header(&data).unwrap();
            println!("\nPage {} header: {} predicates", pm.page_id, header.entries.len());
            for pe in &header.entries {
                println!("  pred_id={}, offset={}, records={}",
                    pe.pred_id, pe.offset, pe.record_count);
                if let Some(uri) = dict.resolve(pe.pred_id) {
                    println!("    → {}", uri);
                }
            }

            // Try to get monument_type block
            if let Some((start, end)) = header.predicate_byte_range(pred_id) {
                let block = &data[start as usize..end as usize];
                let records = parse_records(block);
                println!("  monument_type block: {} records", records.len());
                for rec in &records {
                    let subj = dict.resolve(rec.subject_id).unwrap_or("?");
                    let obj = dict.resolve(rec.object_val).unwrap_or("?");
                    println!("    subject_id={} ({}) object_val={} ({})",
                        rec.subject_id, subj, rec.object_val, obj);
                }

                // Binary search for church
                let (lo, hi) = binary_search_object(&records, obj_id);
                println!("  binary_search for obj_id={}: range [{}, {})", obj_id, lo, hi);
                for rec in &records[lo..hi] {
                    let subj = dict.resolve(rec.subject_id).unwrap_or("?");
                    println!("    MATCH: subject_id={} ({})", rec.subject_id, subj);
                }
            } else {
                println!("  No monument_type block in this page");
            }
        }
    }
}

/// Simulate the WASM client's complete query flow.
fn simulate_client_query(
    base: &str,
    dict: &Dictionary,
    summary: &SummaryIndex,
    page_meta: &[PageMeta],
    patterns_json: &str,
) {
    println!("\n--- Query: {} ---", patterns_json);

    // Step 1: Parse JSON patterns (same as client lib.rs)
    #[derive(serde::Deserialize)]
    struct RawPattern { s: String, p: String, o: String }

    fn parse_term(s: &str) -> (bool, String) {
        if s.starts_with('?') {
            (true, s[1..].to_string())  // variable
        } else {
            (false, s.to_string())  // URI
        }
    }

    let raw_patterns: Vec<RawPattern> = serde_json::from_str(patterns_json).unwrap();

    // Step 2: Plan (mirrors planner.rs plan_from_patterns)
    let mut page_predicates: HashMap<u32, HashSet<u32>> = HashMap::new();

    for rp in &raw_patterns {
        let (p_var, p_uri) = parse_term(&rp.p);
        if p_var {
            for pm in page_meta {
                page_predicates.entry(pm.page_id).or_default();
            }
            continue;
        }

        let pred_id = match dict.lookup(&p_uri) {
            Some(id) => id,
            None => {
                println!("  Predicate '{}' not in dictionary → skip", p_uri);
                continue;
            }
        };

        let (s_var, _s_uri) = parse_term(&rp.s);
        let (o_var, o_uri) = parse_term(&rp.o);

        if s_var && !o_var {
            // ?s pred <obj> — OPS lookup
            if let Some(obj_id) = dict.lookup(&o_uri) {
                let quads = summary.lookup_op(obj_id, pred_id);
                println!("  OPS({},{}) → {} quads", obj_id, pred_id, quads.len());
                for q in quads {
                    page_predicates.entry(q.page_s).or_default().insert(pred_id);
                }
                // Also check page_id as object (resource links)
                for pm in page_meta {
                    let op_quads = summary.lookup_op(pm.page_id, pred_id);
                    for q in op_quads {
                        page_predicates.entry(q.page_s).or_default().insert(pred_id);
                    }
                }
            } else {
                println!("  Object '{}' not in dictionary → skip", o_uri);
            }
        } else if s_var && o_var {
            // ?s pred ?o — P lookup
            let quads = summary.lookup_p(pred_id);
            for q in quads {
                page_predicates.entry(q.page_s).or_default().insert(pred_id);
            }
        } else {
            let quads = summary.lookup_p(pred_id);
            for q in quads {
                page_predicates.entry(q.page_s).or_default().insert(pred_id);
            }
        }
    }

    println!("  Plan: {} pages", page_predicates.len());
    for (pid, preds) in &page_predicates {
        println!("    page {} → preds {:?}", pid, preds);
    }

    // Step 3: Fetch page data (simulate with file reads + range slicing)
    let mut records: HashMap<(u32, u32), Vec<PageRecord>> = HashMap::new();

    for (&page_id, preds) in &page_predicates {
        let path = format!("{base}/pages/page_{:04}.dat", page_id);
        let data = fs::read(&path).unwrap();

        // Simulate: client first fetches header probe (7 bytes)
        let probe = &data[0..7];
        let header_size = full_header_size(probe).unwrap();

        // Then fetches full header
        let header = parse_page_header(&data[0..header_size]).unwrap();

        // Then fetches predicate blocks
        for &pred_id in preds {
            if let Some((start, end)) = header.predicate_byte_range(pred_id) {
                let block_bytes = &data[start as usize..end as usize];
                let recs = parse_records(block_bytes);
                println!("    page {} pred {} → {} records", page_id, pred_id, recs.len());
                records.insert((page_id, pred_id), recs);
            }
        }
    }

    // Step 4: Execute patterns (mirrors client lib.rs execute_patterns)
    let mut result_sets: Vec<HashSet<u32>> = Vec::new();

    for rp in &raw_patterns {
        let (p_var, p_uri) = parse_term(&rp.p);
        if p_var { continue; }

        let pred_id = match dict.lookup(&p_uri) {
            Some(id) => id,
            None => {
                result_sets.push(HashSet::new());
                continue;
            }
        };

        let mut matches = HashSet::new();
        let (o_var, o_uri) = parse_term(&rp.o);

        for (&(_, pid), recs) in &records {
            if pid != pred_id { continue; }

            if !o_var {
                if let Some(obj_id) = dict.lookup(&o_uri) {
                    let (lo, hi) = binary_search_object(recs, obj_id);
                    for rec in &recs[lo..hi] {
                        matches.insert(rec.subject_id);
                    }
                }
            } else {
                for rec in recs {
                    matches.insert(rec.subject_id);
                }
            }
        }

        println!("  Pattern p={} → {} matches", p_uri, matches.len());
        result_sets.push(matches);
    }

    // Step 5: Intersect
    if result_sets.is_empty() {
        println!("  RESULT: 0 (no patterns)");
        return;
    }

    let mut iter = result_sets.into_iter();
    let mut intersection = iter.next().unwrap();
    for set in iter {
        intersection = intersection.intersection(&set).copied().collect();
    }

    let mut results: Vec<u32> = intersection.into_iter().collect();
    results.sort();

    // Step 6: Resolve URIs
    let uris: Vec<&str> = results.iter()
        .filter_map(|&id| dict.resolve(id))
        .collect();

    println!("  RESULT: {} matches", uris.len());
    for uri in &uris {
        println!("    {}", uri);
    }
}
