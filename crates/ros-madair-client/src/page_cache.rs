// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Coverage tracking for loaded page data.
//!
//! Tracks which (page_id, predicate_id) combinations have been loaded,
//! so subsequent queries can skip already-fetched data.

use std::collections::{HashMap, HashSet};

use crate::planner::{FetchPlan, PageFetchSpec};

/// Tracks which page/predicate combinations have been loaded into the store.
#[derive(Debug, Default)]
pub struct PageCache {
    /// page_id → set of loaded predicate IDs.
    loaded: HashMap<u32, HashSet<u32>>,
    /// Total records loaded across all pages.
    record_count: usize,
    /// Pages whose resource metadata section has been loaded.
    meta_loaded: HashSet<u32>,
}

impl PageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check which predicates are missing for a page.
    pub fn gaps(&self, page_id: u32, needed_preds: &[u32]) -> Vec<u32> {
        match self.loaded.get(&page_id) {
            None => needed_preds.to_vec(),
            Some(loaded_preds) => needed_preds
                .iter()
                .filter(|p| !loaded_preds.contains(p))
                .copied()
                .collect(),
        }
    }

    /// Mark predicates as loaded for a page.
    pub fn mark_loaded(&mut self, page_id: u32, preds: &[u32], records: usize) {
        let entry = self.loaded.entry(page_id).or_default();
        for &p in preds {
            entry.insert(p);
        }
        self.record_count += records;
    }

    /// Reduce a fetch plan by removing already-loaded data.
    pub fn reduce_plan(&self, plan: &FetchPlan) -> FetchPlan {
        let pages: Vec<PageFetchSpec> = plan
            .pages
            .iter()
            .filter_map(|spec| {
                let missing = self.gaps(spec.page_id, &spec.predicates);
                if missing.is_empty() {
                    None // fully loaded
                } else {
                    Some(PageFetchSpec {
                        page_id: spec.page_id,
                        predicates: missing,
                    })
                }
            })
            .collect();

        let estimated_bytes = pages
            .iter()
            .map(|p| 200u64 + p.predicates.len() as u64 * 100 * 8)
            .sum();

        FetchPlan {
            pages,
            estimated_bytes,
            summary_result: plan.summary_result.clone(),
        }
    }

    /// Total records currently loaded.
    pub fn record_count(&self) -> usize {
        self.record_count
    }

    /// Number of pages with at least one predicate loaded.
    pub fn page_count(&self) -> usize {
        self.loaded.len()
    }

    /// Check if a specific (page, predicate) is loaded.
    pub fn is_loaded(&self, page_id: u32, pred_id: u32) -> bool {
        self.loaded
            .get(&page_id)
            .is_some_and(|preds| preds.contains(&pred_id))
    }

    /// Mark a page's resource metadata as loaded.
    pub fn mark_meta_loaded(&mut self, page_id: u32) {
        self.meta_loaded.insert(page_id);
    }

    /// Check if a page's resource metadata has been loaded.
    pub fn is_meta_loaded(&self, page_id: u32) -> bool {
        self.meta_loaded.contains(&page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_cache() {
        let cache = PageCache::new();
        assert_eq!(cache.gaps(0, &[1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(cache.record_count(), 0);
        assert_eq!(cache.page_count(), 0);
    }

    #[test]
    fn test_mark_and_check() {
        let mut cache = PageCache::new();
        cache.mark_loaded(0, &[10, 20], 50);

        assert!(cache.is_loaded(0, 10));
        assert!(cache.is_loaded(0, 20));
        assert!(!cache.is_loaded(0, 30));
        assert!(!cache.is_loaded(1, 10));
        assert_eq!(cache.record_count(), 50);
        assert_eq!(cache.page_count(), 1);
    }

    #[test]
    fn test_gaps() {
        let mut cache = PageCache::new();
        cache.mark_loaded(0, &[10, 20], 50);

        assert_eq!(cache.gaps(0, &[10, 20, 30]), vec![30]);
        assert!(cache.gaps(0, &[10, 20]).is_empty());
        assert_eq!(cache.gaps(1, &[10]), vec![10]);
    }

    #[test]
    fn test_reduce_plan() {
        let mut cache = PageCache::new();
        cache.mark_loaded(0, &[10, 20], 50);

        let plan = FetchPlan {
            pages: vec![
                PageFetchSpec {
                    page_id: 0,
                    predicates: vec![10, 20, 30],
                },
                PageFetchSpec {
                    page_id: 1,
                    predicates: vec![10],
                },
            ],
            estimated_bytes: 10000,
            summary_result: None,
        };

        let reduced = cache.reduce_plan(&plan);
        assert_eq!(reduced.pages.len(), 2);
        // Page 0 should only need pred 30
        assert_eq!(reduced.pages[0].predicates, vec![30]);
        // Page 1 unchanged
        assert_eq!(reduced.pages[1].predicates, vec![10]);
    }

    #[test]
    fn test_reduce_plan_fully_loaded() {
        let mut cache = PageCache::new();
        cache.mark_loaded(0, &[10, 20], 50);

        let plan = FetchPlan {
            pages: vec![PageFetchSpec {
                page_id: 0,
                predicates: vec![10, 20],
            }],
            estimated_bytes: 5000,
            summary_result: None,
        };

        let reduced = cache.reduce_plan(&plan);
        assert!(reduced.pages.is_empty());
    }
}
