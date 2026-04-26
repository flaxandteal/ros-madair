// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Query planning and execution over page-based indexes.
//!
//! Contains the core logic for:
//! - Parsing triple patterns into a fetch plan (which pages + predicate blocks)
//! - Executing patterns over loaded page records to produce matching subject IDs
//!
//! These are used by both the WASM client (browser) and the local engine (Python/CLI).

use std::collections::{HashMap, HashSet};

use crate::concept_intervals::ConceptIntervalIndex;
use crate::{binary_search_object, range_search_object, Dictionary, PageMeta, PageRecord, SummaryIndex};

/// A plan for which pages and predicate blocks to fetch.
#[derive(Debug, Clone)]
pub struct FetchPlan {
    /// Pages to fetch, with the predicate blocks needed from each.
    pub pages: Vec<PageFetchSpec>,
    /// Estimated total bytes to fetch.
    pub estimated_bytes: u64,
    /// If the query can be answered from summary quads alone (e.g., COUNT),
    /// the result is here and `pages` will be empty.
    pub summary_result: Option<SummaryResult>,
}

#[derive(Debug, Clone)]
pub struct PageFetchSpec {
    pub page_id: u32,
    /// Predicate dictionary IDs needed from this page.
    pub predicates: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct SummaryResult {
    pub count: u64,
}

/// A parsed triple pattern from a SPARQL BGP.
#[derive(Debug, Clone)]
pub struct TriplePattern {
    pub subject: PatternTerm,
    pub predicate: PatternTerm,
    pub object: PatternTerm,
}

#[derive(Debug, Clone)]
pub enum PatternTerm {
    /// A bound URI.
    Uri(String),
    /// A variable (?x).
    Variable(String),
}

impl PatternTerm {
    pub fn is_bound(&self) -> bool {
        matches!(self, PatternTerm::Uri(_))
    }

    pub fn as_uri(&self) -> Option<&str> {
        match self {
            PatternTerm::Uri(u) => Some(u),
            _ => None,
        }
    }
}

/// Plan a query against the summary index.
///
/// Takes manually-parsed triple patterns and looks up matching pages
/// in the summary index, producing a minimal fetch plan.
///
/// When `concept_intervals` is provided, concept-typed object URIs are
/// resolved via DFS interval range lookup on the OPS index instead of
/// exact-match lookup.
pub fn plan_from_patterns(
    patterns: &[TriplePattern],
    summary: &SummaryIndex,
    dict: &Dictionary,
    page_meta: &[PageMeta],
    concept_intervals: Option<&ConceptIntervalIndex>,
) -> FetchPlan {
    if patterns.is_empty() {
        return FetchPlan {
            pages: Vec::new(),
            estimated_bytes: 0,
            summary_result: None,
        };
    }

    // Group patterns by subject variable name (for intersection).
    // Patterns with a bound subject or different variable names are independent.
    let mut var_groups: HashMap<String, Vec<usize>> = HashMap::new();
    let mut independent: Vec<usize> = Vec::new();
    for (i, pattern) in patterns.iter().enumerate() {
        match &pattern.subject {
            PatternTerm::Variable(name) => {
                var_groups.entry(name.clone()).or_default().push(i);
            }
            PatternTerm::Uri(_) => {
                independent.push(i);
            }
        }
    }

    // Compute per-pattern page sets (page_id -> set of pred_ids needed).
    let mut per_pattern: Vec<HashMap<u32, HashSet<u32>>> = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let mut pages_for_pattern: HashMap<u32, HashSet<u32>> = HashMap::new();

        let pred_uri = match &pattern.predicate {
            PatternTerm::Uri(u) => u.as_str(),
            PatternTerm::Variable(_) => {
                // Variable predicate -- need all pages.
                for pm in page_meta {
                    pages_for_pattern.entry(pm.page_id).or_default();
                }
                per_pattern.push(pages_for_pattern);
                continue;
            }
        };

        let pred_id = match dict.lookup(pred_uri) {
            Some(id) => id,
            None => {
                // Predicate not in dictionary -- no matches possible.
                per_pattern.push(pages_for_pattern);
                continue;
            }
        };

        match (&pattern.subject, &pattern.object) {
            // ?s pred ?o -- need all pages that have this predicate
            (PatternTerm::Variable(_), PatternTerm::Variable(_)) => {
                let quads = summary.lookup_p(pred_id);
                for q in quads {
                    pages_for_pattern
                        .entry(q.page_s)
                        .or_default()
                        .insert(pred_id);
                }
            }
            // ?s pred <obj> -- find pages with this (pred, obj) via OPS index
            (PatternTerm::Variable(_), PatternTerm::Uri(obj_uri)) => {
                if let Some(obj_id) = dict.lookup(obj_uri) {
                    // Try DFS interval range lookup for concepts
                    let concept_quads: Option<Vec<&_>> = concept_intervals
                        .and_then(|ci| ci.lookup(obj_id))
                        .map(|(dfs_enter, dfs_leave)| {
                            let lo = ConceptIntervalIndex::encode_for_summary(dfs_enter);
                            let hi = ConceptIntervalIndex::encode_for_summary(dfs_leave);
                            summary.lookup_op_range(lo, hi, pred_id)
                        });

                    if let Some(quads) = concept_quads {
                        if !quads.is_empty() {
                            for q in quads {
                                pages_for_pattern
                                    .entry(q.page_s)
                                    .or_default()
                                    .insert(pred_id);
                            }
                        } else {
                            // Concept found in interval index but no summary
                            // quads — no matches for this predicate.
                        }
                    } else {
                        // Not a concept or no interval index — original logic.
                        // Try OPS index first: look up by (page_o, pred).
                        let mut quads = summary.lookup_op(obj_id, pred_id);
                        if quads.is_empty() {
                            quads = summary.lookup_op(u32::MAX, pred_id);
                        }
                        if quads.is_empty() {
                            quads = summary.lookup_p(pred_id);
                        }
                        for q in quads {
                            pages_for_pattern
                                .entry(q.page_s)
                                .or_default()
                                .insert(pred_id);
                        }
                    }
                }
            }
            // <subj> pred ?o -- find pages containing the subject
            (PatternTerm::Uri(subj_uri), PatternTerm::Variable(_)) => {
                if let Some(_subj_id) = dict.lookup(subj_uri) {
                    // Without a resource->page index, fall back to all pages
                    // that have this predicate.
                    let quads = summary.lookup_p(pred_id);
                    for q in quads {
                        pages_for_pattern
                            .entry(q.page_s)
                            .or_default()
                            .insert(pred_id);
                    }
                }
            }
            // <subj> pred <obj> -- fully bound, still need the page
            (PatternTerm::Uri(_), PatternTerm::Uri(_)) => {
                let quads = summary.lookup_p(pred_id);
                for q in quads {
                    pages_for_pattern
                        .entry(q.page_s)
                        .or_default()
                        .insert(pred_id);
                }
            }
        }

        per_pattern.push(pages_for_pattern);
    }

    // Merge page sets: intersect pages within each variable group,
    // then union across groups and independent patterns.
    let mut page_predicates: HashMap<u32, HashSet<u32>> = HashMap::new();

    // Process variable groups -- intersect page sets for patterns sharing a variable
    for indices in var_groups.values() {
        if indices.is_empty() {
            continue;
        }
        let mut sorted_indices = indices.clone();
        sorted_indices.sort_by_key(|&i| per_pattern[i].len());

        let first = sorted_indices[0];
        let mut intersection_pages: HashSet<u32> = per_pattern[first].keys().copied().collect();

        for &i in &sorted_indices[1..] {
            let other_pages: HashSet<u32> = per_pattern[i].keys().copied().collect();
            intersection_pages = intersection_pages.intersection(&other_pages).copied().collect();
        }

        // Collect predicates from all patterns for the surviving pages
        for page_id in intersection_pages {
            let preds = page_predicates.entry(page_id).or_default();
            for &i in indices {
                if let Some(pattern_preds) = per_pattern[i].get(&page_id) {
                    preds.extend(pattern_preds);
                }
            }
        }
    }

    // Process independent patterns (bound subject) -- union their pages
    for &i in &independent {
        for (&page_id, preds) in &per_pattern[i] {
            page_predicates.entry(page_id).or_default().extend(preds);
        }
    }

    // Build fetch specs
    let mut pages: Vec<PageFetchSpec> = page_predicates
        .into_iter()
        .map(|(page_id, preds)| PageFetchSpec {
            page_id,
            predicates: preds.into_iter().collect(),
        })
        .collect();
    pages.sort_by_key(|p| p.page_id);

    // Rough byte estimate: header (~200B) + 8 bytes per record x estimated records
    let estimated_bytes: u64 = pages
        .iter()
        .map(|p| {
            let header_est = 200u64;
            let records_est = p.predicates.len() as u64 * 100 * 8; // ~100 records per pred
            header_est + records_est
        })
        .sum();

    FetchPlan {
        pages,
        estimated_bytes,
        summary_result: None,
    }
}

/// Execute triple patterns over loaded page records.
///
/// `records` is indexed by `(page_id, pred_id)` -> sorted `PageRecord` vec.
/// Returns matching subject dict IDs (sorted).
///
/// When `concept_intervals` is provided, concept-typed object URIs use
/// DFS range search instead of exact binary search.
pub fn execute_patterns(
    patterns: &[TriplePattern],
    records: &HashMap<(u32, u32), Vec<PageRecord>>,
    dict: &Dictionary,
    concept_intervals: Option<&ConceptIntervalIndex>,
) -> Vec<u32> {
    if patterns.is_empty() {
        return Vec::new();
    }

    let mut result_sets: Vec<HashSet<u32>> = Vec::new();

    for pattern in patterns {
        let pred_uri = match &pattern.predicate {
            PatternTerm::Uri(u) => u.as_str(),
            PatternTerm::Variable(_) => continue, // TODO: variable predicates
        };

        let pred_id = match dict.lookup(pred_uri) {
            Some(id) => id,
            None => {
                result_sets.push(HashSet::new());
                continue;
            }
        };

        let mut matches = HashSet::new();

        // Scan all loaded blocks for this predicate
        for (&(_, pid), recs) in records {
            if pid != pred_id {
                continue;
            }

            match &pattern.object {
                PatternTerm::Uri(obj_uri) => {
                    if let Some(obj_id) = dict.lookup(obj_uri) {
                        // Try DFS range search for concepts
                        let interval = concept_intervals
                            .and_then(|ci| ci.lookup(obj_id));

                        if let Some((dfs_enter, dfs_leave)) = interval {
                            let (lo, hi) = range_search_object(recs, dfs_enter, dfs_leave);
                            for rec in &recs[lo..hi] {
                                matches.insert(rec.subject_id);
                            }
                        } else {
                            let (lo, hi) = binary_search_object(recs, obj_id);
                            for rec in &recs[lo..hi] {
                                matches.insert(rec.subject_id);
                            }
                        }
                    }
                }
                PatternTerm::Variable(_) => {
                    for rec in recs {
                        matches.insert(rec.subject_id);
                    }
                }
            }
        }

        result_sets.push(matches);
    }

    // Intersect all result sets
    if result_sets.is_empty() {
        return Vec::new();
    }

    let mut iter = result_sets.into_iter();
    let mut intersection = iter.next().unwrap();
    for set in iter {
        intersection = intersection.intersection(&set).copied().collect();
    }

    let mut results: Vec<u32> = intersection.into_iter().collect();
    results.sort();
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{serialize_summary, SummaryBuilder};

    fn setup() -> (SummaryIndex, Dictionary, Vec<PageMeta>) {
        let mut dict = Dictionary::new();
        let type_pred = dict.intern("http://example.org/node/monument_type");
        let church_concept = dict.intern("http://example.org/concept/church");
        let _castle_concept = dict.intern("http://example.org/concept/castle");

        let mut builder = SummaryBuilder::new();
        // Page 0 has churches (type_pred -> church_concept page)
        builder.add(0, type_pred, church_concept, 100);
        builder.add(0, type_pred, church_concept, 101);
        // Page 1 has churches too
        builder.add(1, type_pred, church_concept, 200);
        // Page 2 has no churches

        let quads = builder.build();
        let bytes = serialize_summary(&quads);
        let summary = SummaryIndex::from_bytes(&bytes).unwrap();

        let page_meta = vec![
            PageMeta { page_id: 0, graph_id: "g".into(), resource_count: 100, bbox: None },
            PageMeta { page_id: 1, graph_id: "g".into(), resource_count: 100, bbox: None },
            PageMeta { page_id: 2, graph_id: "g".into(), resource_count: 50, bbox: None },
        ];

        (summary, dict, page_meta)
    }

    #[test]
    fn test_plan_variable_subject_bound_object() {
        let (summary, dict, page_meta) = setup();

        let patterns = vec![TriplePattern {
            subject: PatternTerm::Variable("s".into()),
            predicate: PatternTerm::Uri("http://example.org/node/monument_type".into()),
            object: PatternTerm::Uri("http://example.org/concept/church".into()),
        }];

        let plan = plan_from_patterns(&patterns, &summary, &dict, &page_meta, None);
        let page_ids: HashSet<u32> = plan.pages.iter().map(|p| p.page_id).collect();
        assert!(page_ids.contains(&0));
        assert!(page_ids.contains(&1));
    }

    #[test]
    fn test_plan_unknown_predicate() {
        let (summary, dict, page_meta) = setup();

        let patterns = vec![TriplePattern {
            subject: PatternTerm::Variable("s".into()),
            predicate: PatternTerm::Uri("http://example.org/node/nonexistent".into()),
            object: PatternTerm::Variable("o".into()),
        }];

        let plan = plan_from_patterns(&patterns, &summary, &dict, &page_meta, None);
        assert!(plan.pages.is_empty());
    }

    #[test]
    fn test_empty_patterns() {
        let (summary, dict, page_meta) = setup();
        let plan = plan_from_patterns(&[], &summary, &dict, &page_meta, None);
        assert!(plan.pages.is_empty());
    }

    #[test]
    fn test_execute_patterns_basic() {
        let mut dict = Dictionary::new();
        let pred_id = dict.intern("http://example.org/node/type");
        let obj_id = dict.intern("http://example.org/concept/church");
        let subj1 = dict.intern("http://example.org/resource/r1");
        let subj2 = dict.intern("http://example.org/resource/r2");
        let subj3 = dict.intern("http://example.org/resource/r3");

        let mut records: HashMap<(u32, u32), Vec<PageRecord>> = HashMap::new();
        records.insert(
            (0, pred_id),
            vec![
                PageRecord { object_val: obj_id, subject_id: subj1 },
                PageRecord { object_val: obj_id, subject_id: subj2 },
                PageRecord { object_val: obj_id + 1, subject_id: subj3 },
            ],
        );

        let patterns = vec![TriplePattern {
            subject: PatternTerm::Variable("s".into()),
            predicate: PatternTerm::Uri("http://example.org/node/type".into()),
            object: PatternTerm::Uri("http://example.org/concept/church".into()),
        }];

        let results = execute_patterns(&patterns, &records, &dict, None);
        assert_eq!(results.len(), 2);
        assert!(results.contains(&subj1));
        assert!(results.contains(&subj2));
    }
}
