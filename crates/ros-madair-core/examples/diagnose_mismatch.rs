// Diagnose: compare binary index full scan vs OPS-scoped scan for Monument B
// to find where extra results come from.

use std::collections::HashSet;
use std::fs;
use ros_madair_core::{
    binary_search_object, parse_page_header, parse_records,
    Dictionary, PageMeta, SummaryIndex,
};

fn main() {
    let base = "example/static/index";

    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();

    let meta_str = fs::read_to_string(format!("{base}/page_meta.json")).unwrap();
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str).unwrap();

    let n = "https://example.org/node/";
    let c = "https://example.org/concept/";

    let pred_uri = format!("{n}monument_type_n1");
    let concept_b_uri = format!("{c}50ae187e-98ab-7ff3-6925-a75398112e70");
    let concept_a_uri = format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b");

    let pred_id = dict.lookup(&pred_uri).unwrap();
    let concept_b_id = dict.lookup(&concept_b_uri).unwrap();
    let concept_a_id = dict.lookup(&concept_a_uri).unwrap();

    println!("pred_id (monument_type_n1) = {}", pred_id);
    println!("concept_a_id = {}", concept_a_id);
    println!("concept_b_id = {}", concept_b_id);

    // 1. OPS-scoped scan for concept_b
    let ops_quads = summary.lookup_op(concept_b_id, pred_id);
    let ops_pages: HashSet<u32> = ops_quads.iter().map(|q| q.page_s).collect();
    println!("\nOPS(concept_b, monument_type_n1) → {} pages", ops_pages.len());

    let mut ops_subjects = HashSet::new();
    for &page_id in &ops_pages {
        let path = format!("{base}/pages/page_{:04}.dat", page_id);
        let data = fs::read(&path).unwrap();
        let header = parse_page_header(&data).unwrap();
        if let Some((start, end)) = header.predicate_byte_range(pred_id) {
            let recs = parse_records(&data[start as usize..end as usize]);
            let (lo, hi) = binary_search_object(&recs, concept_b_id);
            for rec in &recs[lo..hi] {
                ops_subjects.insert(rec.subject_id);
            }
        }
    }
    println!("OPS-scoped scan: {} distinct subjects", ops_subjects.len());

    // 2. Full scan: ALL pages, look for concept_b in monument_type_n1 blocks
    let mut full_subjects = HashSet::new();
    let mut extra_pages = Vec::new();  // pages with concept_b NOT in OPS result
    for pm in &page_meta {
        let path = format!("{base}/pages/page_{:04}.dat", pm.page_id);
        let data = fs::read(&path).unwrap();
        let header = parse_page_header(&data).unwrap();
        if let Some((start, end)) = header.predicate_byte_range(pred_id) {
            let recs = parse_records(&data[start as usize..end as usize]);
            let (lo, hi) = binary_search_object(&recs, concept_b_id);
            let count = hi - lo;
            if count > 0 {
                for rec in &recs[lo..hi] {
                    full_subjects.insert(rec.subject_id);
                }
                if !ops_pages.contains(&pm.page_id) {
                    extra_pages.push((pm.page_id, count));
                }
            }
        }
    }
    println!("Full scan: {} distinct subjects", full_subjects.len());

    if !extra_pages.is_empty() {
        println!("\n!!! {} pages have concept_b matches NOT in OPS result:", extra_pages.len());
        for (pid, count) in &extra_pages {
            println!("  page {} → {} records", pid, count);
        }
    }

    let only_in_full: Vec<_> = full_subjects.difference(&ops_subjects).collect();
    let only_in_ops: Vec<_> = ops_subjects.difference(&full_subjects).collect();
    println!("\nOnly in full scan (extra): {}", only_in_full.len());
    println!("Only in OPS scan (missing): {}", only_in_ops.len());

    // 3. Check: do the "extra" subjects have concept_b in the N-Triples?
    if !only_in_full.is_empty() {
        println!("\nSample extra subjects (first 5):");
        for &sid in only_in_full.iter().take(5) {
            let uri = dict.resolve(*sid).unwrap_or("?");
            println!("  {} ({})", sid, uri);
        }
    }

    // 4. Also check concept_a full scan vs OPS
    let ops_a_quads = summary.lookup_op(concept_a_id, pred_id);
    let ops_a_pages: HashSet<u32> = ops_a_quads.iter().map(|q| q.page_s).collect();
    println!("\n--- Concept A comparison ---");
    println!("OPS(concept_a, monument_type_n1) → {} pages", ops_a_pages.len());

    let mut full_a_subjects = HashSet::new();
    let mut extra_a_pages = Vec::new();
    for pm in &page_meta {
        let path = format!("{base}/pages/page_{:04}.dat", pm.page_id);
        let data = fs::read(&path).unwrap();
        let header = parse_page_header(&data).unwrap();
        if let Some((start, end)) = header.predicate_byte_range(pred_id) {
            let recs = parse_records(&data[start as usize..end as usize]);
            let (lo, hi) = binary_search_object(&recs, concept_a_id);
            let count = hi - lo;
            if count > 0 {
                for rec in &recs[lo..hi] {
                    full_a_subjects.insert(rec.subject_id);
                }
                if !ops_a_pages.contains(&pm.page_id) {
                    extra_a_pages.push((pm.page_id, count));
                }
            }
        }
    }
    println!("Full scan: {} distinct subjects", full_a_subjects.len());
    println!("OPS-scoped: {} distinct subjects", {
        let mut s = HashSet::new();
        for &pid in &ops_a_pages {
            let path = format!("{base}/pages/page_{:04}.dat", pid);
            let data = fs::read(&path).unwrap();
            let header = parse_page_header(&data).unwrap();
            if let Some((start, end)) = header.predicate_byte_range(pred_id) {
                let recs = parse_records(&data[start as usize..end as usize]);
                let (lo, hi) = binary_search_object(&recs, concept_a_id);
                for rec in &recs[lo..hi] { s.insert(rec.subject_id); }
            }
        }
        s.len()
    });
    if !extra_a_pages.is_empty() {
        println!("!!! {} extra pages for concept_a", extra_a_pages.len());
    } else {
        println!("No extra pages for concept_a (OPS is complete)");
    }

    // 5. Check: is monument_type (without _n1) also being counted?
    let pred2_uri = format!("{n}monument_type");
    if let Some(pred2_id) = dict.lookup(&pred2_uri) {
        println!("\n--- monument_type (without _n1) ---");
        println!("pred_id (monument_type) = {}", pred2_id);
        println!("Same as monument_type_n1? {}", pred2_id == pred_id);
    } else {
        println!("\nmonument_type (without _n1) not in dictionary");
    }
}

// Check: how many subjects does monument_type (pred_id=15) have for concept_b?
