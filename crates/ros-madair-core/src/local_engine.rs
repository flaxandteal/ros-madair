// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! File-based query engine for local (non-WASM) consumers.
//!
//! Mirrors the WASM client's plan-then-fetch-then-execute workflow,
//! but reads page files from disk instead of issuing HTTP Range requests.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::concept_intervals::ConceptIntervalIndex;
use crate::concept_tree::ConceptTree;
use crate::query::{execute_patterns, plan_from_patterns, PatternTerm, TriplePattern};
use crate::uri::node_uri;
use crate::{parse_page_header, parse_records, Dictionary, PageMeta, PageRecord, ResourceMap, SummaryIndex};

/// A local (filesystem-backed) query engine over a Rós Madair index.
///
/// Loads the lightweight routing structures (summary, dictionary, resource_map,
/// page_meta) at construction, then reads individual page files on demand during
/// query execution.
pub struct LocalQueryEngine {
    dict: Dictionary,
    summary: SummaryIndex,
    resource_map: ResourceMap,
    concept_intervals: Option<ConceptIntervalIndex>,
    concept_tree: Option<ConceptTree>,
    page_meta: Vec<PageMeta>,
    pages_dir: PathBuf,
    base_uri: String,
}

impl LocalQueryEngine {
    /// Open a Rós Madair index directory.
    ///
    /// Expects the directory to contain:
    /// - `dictionary.bin`
    /// - `summary.bin`
    /// - `resource_map.bin`
    /// - `page_meta.json`
    /// - `pages/page_XXXX.dat`
    pub fn open(index_dir: &Path, base_uri: &str) -> Result<Self, String> {
        let dict_bytes = fs::read(index_dir.join("dictionary.bin"))
            .map_err(|e| format!("Failed to read dictionary.bin: {e}"))?;
        let dict = Dictionary::from_bytes(&dict_bytes)?;

        let summary_bytes = fs::read(index_dir.join("summary.bin"))
            .map_err(|e| format!("Failed to read summary.bin: {e}"))?;
        let summary = SummaryIndex::from_bytes(&summary_bytes)?;

        let rmap_bytes = fs::read(index_dir.join("resource_map.bin"))
            .map_err(|e| format!("Failed to read resource_map.bin: {e}"))?;
        let resource_map = ResourceMap::from_bytes(&rmap_bytes)?;

        let meta_str = fs::read_to_string(index_dir.join("page_meta.json"))
            .map_err(|e| format!("Failed to read page_meta.json: {e}"))?;
        let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str)
            .map_err(|e| format!("Failed to parse page_meta.json: {e}"))?;

        // Load concept intervals (optional — older indices won't have this file)
        let concept_intervals = match fs::read(index_dir.join("concept_intervals.bin")) {
            Ok(ci_bytes) => Some(
                ConceptIntervalIndex::from_bytes(&ci_bytes)
                    .map_err(|e| format!("Failed to parse concept_intervals.bin: {e}"))?
            ),
            Err(_) => None,
        };

        // Load concept tree (optional ��� older indices won't have this file)
        let concept_tree = match fs::read(index_dir.join("concept_tree.bin")) {
            Ok(ct_bytes) => Some(
                ConceptTree::from_bytes(&ct_bytes)
                    .map_err(|e| format!("Failed to parse concept_tree.bin: {e}"))?
            ),
            Err(_) => None,
        };

        let pages_dir = index_dir.join("pages");
        if !pages_dir.is_dir() {
            return Err(format!("Pages directory not found: {}", pages_dir.display()));
        }

        Ok(Self {
            dict,
            summary,
            resource_map,
            concept_intervals,
            concept_tree,
            page_meta,
            pages_dir,
            base_uri: base_uri.to_string(),
        })
    }

    /// Construct from pre-loaded structures (used when caller already has them).
    pub fn from_parts(
        dict: Dictionary,
        summary: SummaryIndex,
        resource_map: ResourceMap,
        concept_intervals: Option<ConceptIntervalIndex>,
        page_meta: Vec<PageMeta>,
        pages_dir: PathBuf,
        base_uri: String,
    ) -> Self {
        Self { dict, summary, resource_map, concept_intervals, concept_tree: None, page_meta, pages_dir, base_uri }
    }

    /// Borrow the concept tree.
    pub fn concept_tree(&self) -> Option<&ConceptTree> {
        self.concept_tree.as_ref()
    }

    /// Execute triple patterns against local page files.
    ///
    /// Returns matching resource dict IDs (sorted).
    pub fn query_patterns(&self, patterns: &[TriplePattern]) -> Result<Vec<u32>, String> {
        let plan = plan_from_patterns(
            patterns,
            &self.summary,
            &self.dict,
            &self.page_meta,
            self.concept_intervals.as_ref(),
        );

        let mut records: HashMap<(u32, u32), Vec<PageRecord>> = HashMap::new();

        for spec in &plan.pages {
            let page_path = self.pages_dir.join(format!("page_{:04}.dat", spec.page_id));
            let data = fs::read(&page_path)
                .map_err(|e| format!("Failed to read {}: {e}", page_path.display()))?;

            let header = parse_page_header(&data)?;

            for &pred_id in &spec.predicates {
                if let Some(entry) = header.entries.iter().find(|e| e.pred_id == pred_id) {
                    let start = entry.offset as usize;
                    let end = start + entry.record_count as usize * 8;
                    if end > data.len() {
                        return Err(format!(
                            "Predicate block overflows page file (page={}, pred={})",
                            spec.page_id, pred_id
                        ));
                    }
                    let recs = parse_records(&data[start..end]);
                    records.insert((spec.page_id, pred_id), recs);
                }
            }
        }

        Ok(execute_patterns(patterns, &records, &self.dict, self.concept_intervals.as_ref()))
    }

    /// Convenience: single-predicate query.
    ///
    /// `pred_alias` is the node alias (e.g. "type", "name") — it will be
    /// expanded to the full predicate URI using `base_uri`.
    /// `obj_uri` is an optional exact object URI to filter on.
    pub fn query_predicate(
        &self,
        pred_alias: &str,
        obj_uri: Option<&str>,
    ) -> Result<Vec<u32>, String> {
        let pred_full = node_uri(&self.base_uri, pred_alias);
        let pattern = TriplePattern {
            subject: PatternTerm::Variable("s".into()),
            predicate: PatternTerm::Uri(pred_full),
            object: match obj_uri {
                Some(u) => PatternTerm::Uri(u.to_string()),
                None => PatternTerm::Variable("o".into()),
            },
        };
        self.query_patterns(&[pattern])
    }

    /// Borrow the dictionary.
    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// Borrow the resource map.
    pub fn resource_map(&self) -> &ResourceMap {
        &self.resource_map
    }

    /// Borrow the page metadata.
    pub fn page_meta(&self) -> &[PageMeta] {
        &self.page_meta
    }

    /// The base URI this engine was opened with.
    pub fn base_uri(&self) -> &str {
        &self.base_uri
    }
}

#[cfg(test)]
mod tests {
    // Integration tests require a built index on disk.
    // Unit tests for plan_from_patterns and execute_patterns are in query.rs.
}
