use std::collections::{HashMap, HashSet};
use std::fs;

use ros_madair_core::{
    binary_search_object, full_header_size, parse_page_header, parse_records,
    Dictionary, PageMeta, PageRecord, SummaryIndex, MIN_HEADER_PROBE,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base = args.get(1).map(|s| s.as_str()).unwrap_or("example/static/ros-madair");

    // Load dictionary
    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    println!("=== Dictionary ({} terms) ===", dict.len());
    let sample = dict.len().min(20) as u32;
    for id in 0..sample {
        if let Some(uri) = dict.resolve(id) {
            println!("  {} → {:?}", id, uri);
        }
    }
    if dict.len() > 20 {
        println!("  ... ({} more)", dict.len() - 20);
    }

    // Load summary
    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();
    println!("\n=== Summary ({} quads) ===", summary.len());

    // Load page meta
    let meta_str = fs::read_to_string(format!("{base}/page_meta.json")).unwrap();
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str).unwrap();
    println!("\n=== Page Meta ({} pages) ===", page_meta.len());
    let page_sample = page_meta.len().min(10);
    for pm in &page_meta[..page_sample] {
        println!("  page {} — {} resources, bbox {:?}", pm.page_id, pm.resource_count, pm.bbox);
    }
    if page_meta.len() > 10 {
        println!("  ... ({} more)", page_meta.len() - 10);
    }

    // Discover predicates from the summary — iterate pages to gather predicate frequencies
    let mut pred_freq: HashMap<u32, usize> = HashMap::new();
    for pm in &page_meta {
        for q in summary.lookup_s(pm.page_id) {
            *pred_freq.entry(q.predicate).or_default() += q.edge_count as usize;
        }
    }
    let mut pred_vec: Vec<(u32, usize)> = pred_freq.into_iter().collect();
    pred_vec.sort_by(|a, b| b.1.cmp(&a.1));

    println!("\n=== Top predicates ===");
    for (pid, count) in pred_vec.iter().take(10) {
        let uri = dict.resolve(*pid).unwrap_or("?");
        println!("  id={} edges={} → {}", pid, count, uri);
    }

    // Pick the most frequent predicate for a demo query
    if let Some(&(top_pred_id, _)) = pred_vec.first() {
        let top_pred_uri = dict.resolve(top_pred_id).unwrap_or("?");

        // Find an object for this predicate by scanning a page
        let p_quads = summary.lookup_p(top_pred_id);
        let mut sample_obj_id: Option<u32> = None;

        if let Some(q) = p_quads.first() {
            let path = format!("{base}/pages/page_{:04}.dat", q.page_s);
            if let Ok(data) = fs::read(&path) {
                if data.len() >= MIN_HEADER_PROBE {
                    let header_size = full_header_size(&data[..MIN_HEADER_PROBE]).unwrap();
                    if let Ok(header) = parse_page_header(&data[..header_size.min(data.len())]) {
                        if let Some((start, end)) = header.predicate_byte_range(top_pred_id) {
                            let records = parse_records(&data[start as usize..end as usize]);
                            if let Some(rec) = records.first() {
                                sample_obj_id = Some(rec.object_val);
                            }
                        }
                    }
                }
            }
        }

        // Run the demo query
        if let Some(obj_id) = sample_obj_id {
            let obj_uri = dict.resolve(obj_id).unwrap_or("?");
            let query = format!(
                r#"[{{"s": "?x", "p": "{}", "o": "{}"}}]"#,
                top_pred_uri, obj_uri
            );
            println!("\n=== Demo query ===");
            simulate_client_query(base, &dict, &summary, &page_meta, &query);
        }

        // Also run an open query (?s pred ?o) on a less frequent predicate
        if pred_vec.len() > 2 {
            let (alt_pred_id, _) = pred_vec[2];
            let alt_pred_uri = dict.resolve(alt_pred_id).unwrap_or("?");
            let query = format!(
                r#"[{{"s": "?x", "p": "{}", "o": "?y"}}]"#,
                alt_pred_uri
            );
            println!("\n=== Open query ===");
            simulate_client_query(base, &dict, &summary, &page_meta, &query);
        }
    }

    // Validate all pages are readable
    println!("\n=== Page validation ===");
    let mut ok = 0;
    let mut fail = 0;
    for pm in &page_meta {
        let path = format!("{base}/pages/page_{:04}.dat", pm.page_id);
        match fs::read(&path) {
            Ok(data) => {
                if data.len() < MIN_HEADER_PROBE {
                    println!("  page {} — too small ({} bytes)", pm.page_id, data.len());
                    fail += 1;
                    continue;
                }
                match full_header_size(&data[..MIN_HEADER_PROBE]) {
                    Ok(hs) => {
                        match parse_page_header(&data[..hs.min(data.len())]) {
                            Ok(header) => {
                                ok += 1;
                                // Spot-check: verify each predicate block is within bounds
                                for pe in &header.entries {
                                    if let Some((_start, end)) = header.predicate_byte_range(pe.pred_id) {
                                        if end as usize > data.len() {
                                            println!("  page {} pred {} — block exceeds file ({} > {})",
                                                pm.page_id, pe.pred_id, end, data.len());
                                            fail += 1;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                println!("  page {} — header parse error: {}", pm.page_id, e);
                                fail += 1;
                            }
                        }
                    }
                    Err(e) => {
                        println!("  page {} — probe error: {}", pm.page_id, e);
                        fail += 1;
                    }
                }
            }
            Err(e) => {
                println!("  page {} — read error: {}", pm.page_id, e);
                fail += 1;
            }
        }
    }
    println!("  {} OK, {} failed", ok, fail);
}

/// Simulate the WASM client's complete query flow.
fn simulate_client_query(
    base: &str,
    dict: &Dictionary,
    summary: &SummaryIndex,
    page_meta: &[PageMeta],
    patterns_json: &str,
) {
    println!("  Query: {}", patterns_json);

    #[derive(serde::Deserialize)]
    struct RawPattern { s: String, p: String, o: String }

    fn parse_term(s: &str) -> (bool, String) {
        if s.starts_with('?') {
            (true, s[1..].to_string())
        } else {
            (false, s.to_string())
        }
    }

    let raw_patterns: Vec<RawPattern> = serde_json::from_str(patterns_json).unwrap();

    // Plan
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

        let (s_var, _) = parse_term(&rp.s);
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

    println!("  Plan: {} pages to fetch", page_predicates.len());

    // Fetch + execute
    let mut records: HashMap<(u32, u32), Vec<PageRecord>> = HashMap::new();

    for (&page_id, preds) in &page_predicates {
        let path = format!("{base}/pages/page_{:04}.dat", page_id);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if data.len() < MIN_HEADER_PROBE { continue; }

        let header_size = match full_header_size(&data[..MIN_HEADER_PROBE]) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let header = match parse_page_header(&data[..header_size.min(data.len())]) {
            Ok(h) => h,
            Err(_) => continue,
        };

        for &pred_id in preds {
            if let Some((start, end)) = header.predicate_byte_range(pred_id) {
                let block_bytes = &data[start as usize..end as usize];
                let recs = parse_records(block_bytes);
                records.insert((page_id, pred_id), recs);
            }
        }
    }

    // Execute patterns
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

        result_sets.push(matches);
    }

    // Intersect
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

    // Resolve URIs
    let uris: Vec<&str> = results.iter()
        .filter_map(|&id| dict.resolve(id))
        .collect();

    println!("  RESULT: {} matches", uris.len());
    for uri in uris.iter().take(10) {
        println!("    {}", uri);
    }
    if uris.len() > 10 {
        println!("    ... ({} more)", uris.len() - 10);
    }
}
