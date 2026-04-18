// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Assign resources to pages using a two-tier strategy:
//!
//! 1. **Tier 1 — Characteristic set:** Group by graph_id. Resources with the
//!    same predicate set (same model) go together.
//! 2. **Tier 2 — Hilbert sub-sort:** Within each graph_id group, sort by 3D
//!    Hilbert curve over (centroid_x, centroid_y, type_bucket).
//! 3. **Slice into pages:** Cut the sorted sequence into pages of
//!    ~target_page_size resources.

use std::collections::{HashMap, HashSet};

use crate::hilbert::{concept_coordinate, hilbert_index_3d};

/// Summary of a resource for page assignment.
#[derive(Debug, Clone)]
pub struct ResourceSummary {
    pub resource_id: String,
    pub graph_id: String,
    pub centroid: Option<(f64, f64)>,
    pub concept_ids: Vec<String>,
}

/// A resource's page assignment.
#[derive(Debug, Clone)]
pub struct PageAssignment {
    pub resource_id: String,
    pub page_id: u32,
    pub graph_id: String,
}

/// Configuration for page assignment.
#[derive(Debug, Clone)]
pub struct PageConfig {
    /// Target number of resources per page.
    pub target_page_size: usize,
}

impl Default for PageConfig {
    fn default() -> Self {
        Self {
            target_page_size: 2000,
        }
    }
}

/// Metadata about a page.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PageMeta {
    pub page_id: u32,
    pub graph_id: String,
    pub resource_count: usize,
    /// Bounding box: (min_lng, min_lat, max_lng, max_lat), if any resources have geometry.
    pub bbox: Option<(f64, f64, f64, f64)>,
}

/// Result of page assignment.
#[derive(Debug, Clone)]
pub struct PageIndex {
    pub assignments: Vec<PageAssignment>,
    pub page_meta: Vec<PageMeta>,
    /// resource_id → page_id lookup
    pub resource_to_page: HashMap<String, u32>,
}

/// Assign resources to pages.
pub fn assign_pages(summaries: &[ResourceSummary], config: &PageConfig) -> PageIndex {
    if summaries.is_empty() {
        return PageIndex {
            assignments: Vec::new(),
            page_meta: Vec::new(),
            resource_to_page: HashMap::new(),
        };
    }

    // Tier 1: group by graph_id
    let mut by_graph: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, s) in summaries.iter().enumerate() {
        by_graph.entry(s.graph_id.as_str()).or_default().push(i);
    }

    let mut assignments = Vec::with_capacity(summaries.len());
    let mut page_meta = Vec::new();
    let mut resource_to_page = HashMap::with_capacity(summaries.len());
    let mut next_page_id: u32 = 0;

    // Process each graph group
    let mut graph_ids: Vec<&str> = by_graph.keys().copied().collect();
    graph_ids.sort(); // deterministic ordering

    for graph_id in graph_ids {
        let indices = &by_graph[graph_id];

        // Tier 2: compute Hilbert key for each resource in this group
        let mut keyed: Vec<(u64, usize)> = indices
            .iter()
            .map(|&i| {
                let s = &summaries[i];
                let (lng, lat) = s.centroid.unwrap_or((0.0, 0.0));
                let concept_set: HashSet<String> =
                    s.concept_ids.iter().cloned().collect();
                let type_bucket = concept_coordinate(&concept_set);
                let key = hilbert_index_3d(lng, lat, type_bucket);
                (key, i)
            })
            .collect();

        // Sort by Hilbert key
        keyed.sort_by_key(|&(key, _)| key);

        // Slice into pages
        for chunk in keyed.chunks(config.target_page_size) {
            let page_id = next_page_id;
            next_page_id += 1;

            let mut min_lng = f64::MAX;
            let mut min_lat = f64::MAX;
            let mut max_lng = f64::MIN;
            let mut max_lat = f64::MIN;
            let mut has_geo = false;

            for &(_, idx) in chunk {
                let s = &summaries[idx];
                assignments.push(PageAssignment {
                    resource_id: s.resource_id.clone(),
                    page_id,
                    graph_id: s.graph_id.clone(),
                });
                resource_to_page.insert(s.resource_id.clone(), page_id);

                if let Some((lng, lat)) = s.centroid {
                    has_geo = true;
                    min_lng = min_lng.min(lng);
                    min_lat = min_lat.min(lat);
                    max_lng = max_lng.max(lng);
                    max_lat = max_lat.max(lat);
                }
            }

            page_meta.push(PageMeta {
                page_id,
                graph_id: graph_id.to_string(),
                resource_count: chunk.len(),
                bbox: if has_geo {
                    Some((min_lng, min_lat, max_lng, max_lat))
                } else {
                    None
                },
            });
        }
    }

    PageIndex {
        assignments,
        page_meta,
        resource_to_page,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_input() {
        let idx = assign_pages(&[], &PageConfig::default());
        assert!(idx.assignments.is_empty());
        assert!(idx.page_meta.is_empty());
    }

    #[test]
    fn test_single_graph_single_page() {
        let summaries: Vec<ResourceSummary> = (0..50)
            .map(|i| ResourceSummary {
                resource_id: format!("res-{i}"),
                graph_id: "graph-1".to_string(),
                centroid: Some((i as f64 * 0.01, 54.0)),
                concept_ids: vec![],
            })
            .collect();

        let idx = assign_pages(&summaries, &PageConfig { target_page_size: 200 });
        assert_eq!(idx.page_meta.len(), 1);
        assert_eq!(idx.assignments.len(), 50);
        assert!(idx.page_meta[0].bbox.is_some());
    }

    #[test]
    fn test_multiple_pages() {
        let summaries: Vec<ResourceSummary> = (0..500)
            .map(|i| ResourceSummary {
                resource_id: format!("res-{i}"),
                graph_id: "graph-1".to_string(),
                centroid: Some(((i % 360) as f64 - 180.0, (i % 180) as f64 - 90.0)),
                concept_ids: vec![],
            })
            .collect();

        let idx = assign_pages(&summaries, &PageConfig { target_page_size: 200 });
        assert_eq!(idx.page_meta.len(), 3); // 500/200 = 2.5 → 3 pages
        assert_eq!(idx.assignments.len(), 500);
    }

    #[test]
    fn test_multiple_graphs_separate_pages() {
        let mut summaries = Vec::new();
        for i in 0..100 {
            summaries.push(ResourceSummary {
                resource_id: format!("hp-{i}"),
                graph_id: "heritage-place".to_string(),
                centroid: Some((i as f64 * 0.01, 54.0)),
                concept_ids: vec![],
            });
        }
        for i in 0..50 {
            summaries.push(ResourceSummary {
                resource_id: format!("person-{i}"),
                graph_id: "person".to_string(),
                centroid: None,
                concept_ids: vec![],
            });
        }

        let idx = assign_pages(&summaries, &PageConfig { target_page_size: 200 });
        // heritage-place (100) → 1 page, person (50) → 1 page
        assert_eq!(idx.page_meta.len(), 2);

        // Each page has resources from only one graph
        for pm in &idx.page_meta {
            let page_assignments: Vec<_> = idx
                .assignments
                .iter()
                .filter(|a| a.page_id == pm.page_id)
                .collect();
            let graphs: HashSet<_> = page_assignments.iter().map(|a| &a.graph_id).collect();
            assert_eq!(graphs.len(), 1);
        }
    }

    #[test]
    fn test_resource_to_page_lookup() {
        let summaries = vec![
            ResourceSummary {
                resource_id: "res-1".to_string(),
                graph_id: "g".to_string(),
                centroid: None,
                concept_ids: vec![],
            },
            ResourceSummary {
                resource_id: "res-2".to_string(),
                graph_id: "g".to_string(),
                centroid: None,
                concept_ids: vec![],
            },
        ];

        let idx = assign_pages(&summaries, &PageConfig::default());
        assert!(idx.resource_to_page.contains_key("res-1"));
        assert!(idx.resource_to_page.contains_key("res-2"));
    }
}
