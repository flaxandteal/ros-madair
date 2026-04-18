// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Generate validation.json — ground-truth expected results for each example query.
//!
//! Reads the binary index directly and writes expected result URIs + counts
//! for the HTML demo to validate against.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use ros_madair_core::{
    binary_search_object, parse_page_header, parse_records,
    Dictionary, PageMeta, SummaryIndex,
};

fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "example/static/index".to_string());
    let output = std::env::args()
        .nth(2)
        .unwrap_or_else(|| format!("{}/validation.json", base));

    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();

    let meta_str = fs::read_to_string(format!("{base}/page_meta.json")).unwrap();
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str).unwrap();

    let c = "https://example.org/concept/";
    let n = "https://example.org/node/";

    // Same queries as the HTML demo examples
    let queries: Vec<(&str, Vec<(String, Option<String>)>)> = vec![
        ("monument_a", vec![
            (format!("{n}monument_type_n1"), Some(format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b"))),
        ]),
        ("monument_b", vec![
            (format!("{n}monument_type_n1"), Some(format!("{c}50ae187e-98ab-7ff3-6925-a75398112e70"))),
        ]),
        ("townland", vec![
            (format!("{n}townland"), Some(format!("{c}afad673d-a7f1-d1ed-12b9-abd1ac321134"))),
        ]),
        ("type_townland", vec![
            (format!("{n}monument_type_n1"), Some(format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b"))),
            (format!("{n}townland"), Some(format!("{c}afad673d-a7f1-d1ed-12b9-abd1ac321134"))),
        ]),
        ("type_grade", vec![
            (format!("{n}monument_type_n1"), Some(format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b"))),
            (format!("{n}grade"), Some(format!("{c}e47378ae-6ede-a0ab-dba8-97e840833f25"))),
        ]),
        ("type_townland_grade", vec![
            (format!("{n}monument_type_n1"), Some(format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b"))),
            (format!("{n}townland"), Some(format!("{c}afad673d-a7f1-d1ed-12b9-abd1ac321134"))),
            (format!("{n}grade"), Some(format!("{c}e47378ae-6ede-a0ab-dba8-97e840833f25"))),
        ]),
        ("all_typed", vec![
            (format!("{n}monument_type_n1"), None),
        ]),
        ("all_graded", vec![
            (format!("{n}grade"), None),
        ]),
    ];

    let mut results = serde_json::Map::new();

    for (name, patterns) in &queries {
        let (uris, pages_touched) = run_query(&base, &dict, &summary, &page_meta, patterns);
        println!("{:30} → {:>6} results, {:>3} pages scanned", name, uris.len(), pages_touched);
        let entry = serde_json::json!({
            "count": uris.len(),
            "uris": uris,
        });
        results.insert(name.to_string(), entry);
    }

    let json = serde_json::to_string_pretty(&serde_json::Value::Object(results)).unwrap();
    fs::write(&output, &json).unwrap();
    println!("\nWrote {}", output);
}

fn run_query(
    base: &str,
    dict: &Dictionary,
    summary: &SummaryIndex,
    _page_meta: &[PageMeta],
    patterns: &[(String, Option<String>)], // (pred_uri, Some(obj_uri) or None for variable)
) -> (Vec<String>, usize) {
    // Resolve pred IDs; for bound objects resolve obj IDs too
    let resolved: Vec<(u32, Option<u32>)> = patterns.iter().filter_map(|(p, o)| {
        let pid = dict.lookup(p)?;
        let oid = match o {
            Some(uri) => Some(dict.lookup(uri)?),
            None => None,
        };
        Some((pid, oid))
    }).collect();

    if resolved.len() != patterns.len() {
        return (Vec::new(), 0);
    }

    // Find all pages that have any of the needed predicates
    let mut page_preds: HashMap<u32, HashSet<u32>> = HashMap::new();
    for &(pred_id, obj_id) in &resolved {
        match obj_id {
            Some(oid) => {
                let quads = summary.lookup_op(oid, pred_id);
                for q in quads {
                    page_preds.entry(q.page_s).or_default().insert(pred_id);
                }
            }
            None => {
                let quads = summary.lookup_p(pred_id);
                for q in quads {
                    page_preds.entry(q.page_s).or_default().insert(pred_id);
                }
            }
        }
    }

    // For multi-pattern: intersect page sets (same logic as the fixed planner)
    if resolved.len() > 1 {
        let per_pattern_pages: Vec<HashSet<u32>> = resolved.iter().map(|&(pred_id, obj_id)| {
            match obj_id {
                Some(oid) => {
                    summary.lookup_op(oid, pred_id).iter().map(|q| q.page_s).collect()
                }
                None => {
                    summary.lookup_p(pred_id).iter().map(|q| q.page_s).collect()
                }
            }
        }).collect();

        let mut intersection: HashSet<u32> = per_pattern_pages[0].clone();
        for s in &per_pattern_pages[1..] {
            intersection = intersection.intersection(s).copied().collect();
        }

        page_preds.retain(|pid, _| intersection.contains(pid));
        // Ensure all predicates are listed for surviving pages
        for page_id in &intersection {
            let preds = page_preds.entry(*page_id).or_default();
            for &(pred_id, _) in &resolved {
                preds.insert(pred_id);
            }
        }
    }

    let pages_touched = page_preds.len();

    // Execute
    let mut result_sets: Vec<HashSet<u32>> = Vec::new();
    for &(pred_id, obj_id) in &resolved {
        let mut matches = HashSet::new();
        for (&page_id, preds) in &page_preds {
            if !preds.contains(&pred_id) { continue; }
            let path = format!("{base}/pages/page_{:04}.dat", page_id);
            let data = match fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let header = parse_page_header(&data).unwrap();
            if let Some((start, end)) = header.predicate_byte_range(pred_id) {
                let recs = parse_records(&data[start as usize..end as usize]);
                match obj_id {
                    Some(oid) => {
                        let (lo, hi) = binary_search_object(&recs, oid);
                        for rec in &recs[lo..hi] {
                            matches.insert(rec.subject_id);
                        }
                    }
                    None => {
                        for rec in &recs {
                            matches.insert(rec.subject_id);
                        }
                    }
                }
            }
        }
        result_sets.push(matches);
    }

    if result_sets.is_empty() { return (Vec::new(), pages_touched); }
    let mut iter = result_sets.into_iter();
    let mut intersection = iter.next().unwrap();
    for set in iter {
        intersection = intersection.intersection(&set).copied().collect();
    }

    // Sort URIs for stable comparison
    let uris: BTreeSet<String> = intersection.iter()
        .filter_map(|&id| dict.resolve(id).map(String::from))
        .collect();

    (uris.into_iter().collect(), pages_touched)
}
