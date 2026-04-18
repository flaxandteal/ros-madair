// Quick test: run example queries against index to verify result counts
use std::collections::{HashMap, HashSet};
use std::fs;
use ros_madair_core::{
    binary_search_object, parse_page_header, parse_records,
    Dictionary, PageMeta, SummaryIndex,
};

fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "example/static/index".to_string());

    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    let summary_bytes = fs::read(format!("{base}/summary.bin")).unwrap();
    let summary = SummaryIndex::from_bytes(&summary_bytes).unwrap();

    let meta_str = fs::read_to_string(format!("{base}/page_meta.json")).unwrap();
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str).unwrap();

    let c = "https://example.org/concept/";
    let n = "https://example.org/node/";

    let owned_queries: Vec<(&str, Vec<(String, String)>)> = vec![
        ("Monument Type A", vec![
            (format!("{n}monument_type_n1"), format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b")),
        ]),
        ("Monument Type B", vec![
            (format!("{n}monument_type_n1"), format!("{c}50ae187e-98ab-7ff3-6925-a75398112e70")),
        ]),
        ("Townland (top)", vec![
            (format!("{n}townland"), format!("{c}afad673d-a7f1-d1ed-12b9-abd1ac321134")),
        ]),
        ("Type A + Townland", vec![
            (format!("{n}monument_type_n1"), format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b")),
            (format!("{n}townland"), format!("{c}afad673d-a7f1-d1ed-12b9-abd1ac321134")),
        ]),
        ("Type A + Grade", vec![
            (format!("{n}monument_type_n1"), format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b")),
            (format!("{n}grade"), format!("{c}e47378ae-6ede-a0ab-dba8-97e840833f25")),
        ]),
        ("Type A + Townland + Grade (3-way)", vec![
            (format!("{n}monument_type_n1"), format!("{c}43bf135f-e369-f6f4-a85d-9a54fb1fa44b")),
            (format!("{n}townland"), format!("{c}afad673d-a7f1-d1ed-12b9-abd1ac321134")),
            (format!("{n}grade"), format!("{c}e47378ae-6ede-a0ab-dba8-97e840833f25")),
        ]),
    ];

    for (name, patterns) in &owned_queries {
        let result = run_query(&base, &dict, &summary, &page_meta, patterns);
        println!("{:45} → {} results", name, result);
    }
}

fn run_query(
    base: &str,
    dict: &Dictionary,
    summary: &SummaryIndex,
    _page_meta: &[PageMeta],
    patterns: &[(String, String)],
) -> usize {
    let resolved: Vec<(u32, u32)> = patterns.iter().filter_map(|(p, o)| {
        let pid = dict.lookup(p)?;
        let oid = dict.lookup(o)?;
        Some((pid, oid))
    }).collect();

    if resolved.len() != patterns.len() {
        return 0;
    }

    let mut page_preds: HashMap<u32, HashSet<u32>> = HashMap::new();
    for &(pred_id, obj_id) in &resolved {
        let quads = summary.lookup_op(obj_id, pred_id);
        for q in quads {
            page_preds.entry(q.page_s).or_default().insert(pred_id);
        }
    }

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
                let (lo, hi) = binary_search_object(&recs, obj_id);
                for rec in &recs[lo..hi] {
                    matches.insert(rec.subject_id);
                }
            }
        }
        result_sets.push(matches);
    }

    if result_sets.is_empty() { return 0; }
    let mut iter = result_sets.into_iter();
    let mut intersection = iter.next().unwrap();
    for set in iter {
        intersection = intersection.intersection(&set).copied().collect();
    }
    intersection.len()
}
