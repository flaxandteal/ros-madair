// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! File-based query engine for local (non-WASM) consumers.
//!
//! Mirrors the WASM client's plan-then-fetch-then-execute workflow,
//! but reads page files from disk instead of issuing HTTP Range requests.
//! Supports multiple layers (base + corpus overlays).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::concept_intervals::ConceptIntervalIndex;
use crate::concept_tree::ConceptTree;
use crate::query::{execute_patterns, execute_single_pattern, plan_from_patterns, PatternTerm, TriplePattern};
use crate::uri::node_uri;
use crate::{
    parse_page_header, parse_records, parse_resource_meta, parse_tile_content_header,
    Dictionary, PageMeta, PageRecord, ResourceMap, ResourceMeta, SummaryIndex,
};

/// Per-layer index data for a local query engine.
struct LocalLayer {
    dict: Dictionary,
    summary: SummaryIndex,
    resource_map: ResourceMap,
    concept_intervals: Option<ConceptIntervalIndex>,
    concept_tree: Option<ConceptTree>,
    page_meta: Vec<PageMeta>,
    pages_dir: PathBuf,
    base_uri: String,
    name: String,
}

/// A local (filesystem-backed) query engine over a Rós Madair index.
///
/// Supports multiple layers (base + overlays). Loads the lightweight routing
/// structures at construction, then reads individual page files on demand
/// during query execution.
pub struct LocalQueryEngine {
    layers: Vec<LocalLayer>,
}

/// Load a layer from an index directory.
fn load_local_layer(index_dir: &Path, base_uri: &str, name: &str) -> Result<LocalLayer, String> {
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

    let concept_intervals = match fs::read(index_dir.join("concept_intervals.bin")) {
        Ok(ci_bytes) => Some(
            ConceptIntervalIndex::from_bytes(&ci_bytes)
                .map_err(|e| format!("Failed to parse concept_intervals.bin: {e}"))?
        ),
        Err(_) => None,
    };

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

    Ok(LocalLayer {
        dict,
        summary,
        resource_map,
        concept_intervals,
        concept_tree,
        page_meta,
        pages_dir,
        base_uri: base_uri.to_string(),
        name: name.to_string(),
    })
}

/// Load page records for a fetch plan from a local layer's pages directory.
fn load_plan_records(
    layer: &LocalLayer,
    plan: &crate::query::FetchPlan,
) -> Result<HashMap<(u32, u32), Vec<PageRecord>>, String> {
    let mut records: HashMap<(u32, u32), Vec<PageRecord>> = HashMap::new();
    for spec in &plan.pages {
        let page_path = layer.pages_dir.join(format!("page_{:04}.dat", spec.page_id));
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
    Ok(records)
}

impl LocalQueryEngine {
    /// Open a Rós Madair index directory.
    pub fn open(index_dir: &Path, base_uri: &str) -> Result<Self, String> {
        let layer = load_local_layer(index_dir, base_uri, "base")?;
        Ok(Self { layers: vec![layer] })
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
        Self {
            layers: vec![LocalLayer {
                dict, summary, resource_map, concept_intervals,
                concept_tree: None, page_meta, pages_dir,
                base_uri, name: "base".to_string(),
            }],
        }
    }

    /// Add an additional index layer (e.g. a corpus overlay).
    pub fn add_layer(&mut self, index_dir: &Path, base_uri: &str, name: &str) -> Result<(), String> {
        let layer = load_local_layer(index_dir, base_uri, name)?;
        self.layers.push(layer);
        Ok(())
    }

    /// Number of loaded layers.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Borrow the concept tree (base layer).
    pub fn concept_tree(&self) -> Option<&ConceptTree> {
        self.layers.first().and_then(|l| l.concept_tree.as_ref())
    }

    /// Execute triple patterns across layers, returning matching resource URIs.
    ///
    /// Each pattern is executed against all targeted layers, results are unioned
    /// per-pattern (resolved to URIs), then intersected across patterns.
    ///
    /// `layer_filter` optionally restricts to a named layer.
    pub fn query_patterns_multi(
        &self,
        patterns: &[TriplePattern],
        layer_filter: Option<&str>,
    ) -> Result<Vec<String>, String> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }

        let target_layers: Vec<usize> = if let Some(name) = layer_filter {
            self.layers.iter()
                .enumerate()
                .filter(|(_, l)| l.name == name)
                .map(|(i, _)| i)
                .collect()
        } else {
            (0..self.layers.len()).collect()
        };

        if target_layers.is_empty() {
            return Err(format!("Unknown layer: '{}'", layer_filter.unwrap_or("")));
        }

        // Single layer fast path
        if target_layers.len() == 1 {
            let li = target_layers[0];
            let layer = &self.layers[li];
            let plan = plan_from_patterns(
                patterns, &layer.summary, &layer.dict, &layer.page_meta,
                layer.concept_intervals.as_ref(),
            );
            let records = load_plan_records(layer, &plan)?;
            let dict_ids = execute_patterns(
                patterns, &records, &layer.dict,
                layer.concept_intervals.as_ref(),
            );
            let uris: Vec<String> = dict_ids.into_iter()
                .filter_map(|id| layer.dict.resolve(id).map(String::from))
                .collect();
            return Ok(uris);
        }

        // Multi-layer: per-pattern union, then intersection
        let mut pattern_uri_sets: Vec<HashSet<String>> = vec![HashSet::new(); patterns.len()];

        for &li in &target_layers {
            let layer = &self.layers[li];
            let plan = plan_from_patterns(
                patterns, &layer.summary, &layer.dict, &layer.page_meta,
                layer.concept_intervals.as_ref(),
            );
            let records = load_plan_records(layer, &plan)?;

            for (pi, pattern) in patterns.iter().enumerate() {
                let matches = execute_single_pattern(
                    pattern, &records, &layer.dict,
                    layer.concept_intervals.as_ref(),
                );
                for subject_id in matches {
                    if let Some(uri) = layer.dict.resolve(subject_id) {
                        pattern_uri_sets[pi].insert(uri.to_string());
                    }
                }
            }
        }

        let mut result_iter = pattern_uri_sets.into_iter();
        let mut result = result_iter.next().unwrap();
        for set in result_iter {
            result = result.intersection(&set).cloned().collect();
        }

        let mut uris: Vec<String> = result.into_iter().collect();
        uris.sort();
        Ok(uris)
    }

    /// Execute triple patterns against the base layer's page files.
    ///
    /// Returns matching resource dict IDs (sorted). For multi-layer queries,
    /// use `query_patterns_multi` instead.
    pub fn query_patterns(&self, patterns: &[TriplePattern]) -> Result<Vec<u32>, String> {
        let layer = self.layers.first()
            .ok_or_else(|| "No layers loaded".to_string())?;
        let plan = plan_from_patterns(
            patterns, &layer.summary, &layer.dict, &layer.page_meta,
            layer.concept_intervals.as_ref(),
        );
        let records = load_plan_records(layer, &plan)?;
        Ok(execute_patterns(
            patterns, &records, &layer.dict,
            layer.concept_intervals.as_ref(),
        ))
    }

    /// Convenience: single-predicate query on base layer.
    pub fn query_predicate(
        &self,
        pred_alias: &str,
        obj_uri: Option<&str>,
    ) -> Result<Vec<u32>, String> {
        let base_uri = self.layers.first()
            .map(|l| l.base_uri.as_str())
            .unwrap_or("");
        let pred_full = node_uri(base_uri, pred_alias);
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

    /// Borrow the dictionary (base layer).
    pub fn dictionary(&self) -> &Dictionary {
        &self.layers[0].dict
    }

    /// Borrow the resource map (base layer).
    pub fn resource_map(&self) -> &ResourceMap {
        &self.layers[0].resource_map
    }

    /// Borrow the page metadata (base layer).
    pub fn page_meta(&self) -> &[PageMeta] {
        &self.layers[0].page_meta
    }

    /// The base URI (base layer).
    pub fn base_uri(&self) -> &str {
        &self.layers[0].base_uri
    }

    /// Find a resource across all layers. Returns `(layer_index, dict_id, page_id)`.
    pub fn find_resource(&self, uri: &str) -> Result<(usize, u32, u32), String> {
        for (li, layer) in self.layers.iter().enumerate() {
            if let Some(dict_id) = layer.dict.lookup(uri) {
                if let Some(page_id) = layer.resource_map.page_for(dict_id) {
                    return Ok((li, dict_id, page_id));
                }
            }
        }
        Err(format!("Resource not found in any layer: {}", uri))
    }

    /// Load resource metadata from a page file's embedded metadata section.
    pub fn load_resource_meta(&self, page_id: u32, layer_index: Option<usize>) -> Result<Vec<ResourceMeta>, String> {
        let li = layer_index.unwrap_or(0);
        let layer = self.layers.get(li)
            .ok_or_else(|| format!("Invalid layer index: {}", li))?;

        let page_path = layer.pages_dir.join(format!("page_{:04}.dat", page_id));
        let data = fs::read(&page_path)
            .map_err(|e| format!("Failed to read {}: {e}", page_path.display()))?;
        let header = parse_page_header(&data)?;

        match header.resource_meta_range {
            Some((offset, size)) => {
                let start = offset as usize;
                let end = start + size as usize;
                if end > data.len() {
                    return Err(format!(
                        "Resource meta range [{}, {}) exceeds page file size {}",
                        start, end, data.len()
                    ));
                }
                parse_resource_meta(&data[start..end])
            }
            None => Ok(Vec::new()),
        }
    }

    /// Load and parse tiles for a resource, optionally filtering by nodegroup.
    ///
    /// Reads the tile file from disk, finds the resource's blob via the header,
    /// and deserializes it.
    pub fn load_tiles_for_resource(
        &self,
        resource_uri: &str,
        nodegroup_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let (li, subject_id, page_id) = self.find_resource(resource_uri)?;
        let layer = &self.layers[li];

        let tiles_dir = layer.pages_dir.parent()
            .ok_or_else(|| "Cannot determine tiles directory".to_string())?
            .join("tiles");
        let tile_path = tiles_dir.join(format!("tile_{:04}.dat", page_id));
        let data = fs::read(&tile_path)
            .map_err(|e| format!("Failed to read {}: {e}", tile_path.display()))?;

        let header = parse_tile_content_header(&data)?;
        let entry = header.entry_for_subject(subject_id)
            .ok_or_else(|| format!(
                "No tile entry for subject_id {} in page {}",
                subject_id, page_id
            ))?;

        let start = entry.blob_offset as usize;
        let end = start + entry.blob_size as usize;
        if end > data.len() {
            return Err(format!(
                "Tile blob range [{}, {}) exceeds file size {}",
                start, end, data.len()
            ));
        }
        let blob = &data[start..end];

        #[derive(serde::Deserialize)]
        struct ResourceBlob {
            tiles: Vec<serde_json::Value>,
            #[serde(default, rename = "__cache")]
            cache: Option<serde_json::Value>,
            #[serde(default, rename = "__scopes")]
            scopes: Option<serde_json::Value>,
        }

        let parsed = if header.version >= 2 {
            rmp_serde::from_slice::<ResourceBlob>(blob)
                .map_err(|e| format!("Failed to deserialize v2 tile blob: {e}"))?
        } else {
            let tiles: Vec<serde_json::Value> = rmp_serde::from_slice(blob)
                .map_err(|e| format!("Failed to deserialize v1 tile data: {e}"))?;
            ResourceBlob { tiles, cache: None, scopes: None }
        };

        let filtered: Vec<&serde_json::Value> = match nodegroup_id {
            Some(ng_id) => parsed.tiles.iter()
                .filter(|t| t.get("nodegroup_id").and_then(|v| v.as_str()) == Some(ng_id))
                .collect(),
            None => parsed.tiles.iter().collect(),
        };

        Ok(serde_json::json!({
            "tiles": filtered,
            "__cache": parsed.cache,
            "__scopes": parsed.scopes,
        }))
    }

    /// Load raw tile blob bytes for a resource (unfiltered).
    pub fn load_tile_blob(&self, resource_uri: &str) -> Result<Vec<u8>, String> {
        let (li, subject_id, page_id) = self.find_resource(resource_uri)?;
        let layer = &self.layers[li];

        let tiles_dir = layer.pages_dir.parent()
            .ok_or_else(|| "Cannot determine tiles directory".to_string())?
            .join("tiles");
        let tile_path = tiles_dir.join(format!("tile_{:04}.dat", page_id));
        let data = fs::read(&tile_path)
            .map_err(|e| format!("Failed to read {}: {e}", tile_path.display()))?;

        let header = parse_tile_content_header(&data)?;
        let entry = header.entry_for_subject(subject_id)
            .ok_or_else(|| format!(
                "No tile entry for subject_id {} in page {}",
                subject_id, page_id
            ))?;

        let start = entry.blob_offset as usize;
        let end = start + entry.blob_size as usize;
        if end > data.len() {
            return Err(format!(
                "Tile blob range [{}, {}) exceeds file size {}",
                start, end, data.len()
            ));
        }

        Ok(data[start..end].to_vec())
    }

    /// Borrow a layer's dictionary by index.
    pub fn layer_dictionary(&self, layer_index: usize) -> Option<&Dictionary> {
        self.layers.get(layer_index).map(|l| &l.dict)
    }

    /// Borrow a layer's resource map by index.
    pub fn layer_resource_map(&self, layer_index: usize) -> Option<&ResourceMap> {
        self.layers.get(layer_index).map(|l| &l.resource_map)
    }

    /// Get layer names.
    pub fn layer_names(&self) -> Vec<&str> {
        self.layers.iter().map(|l| l.name.as_str()).collect()
    }

    /// Look up a summary's forward connections from a page.
    pub fn summary_from_page(&self, page_id: u32, layer_index: Option<usize>) -> Vec<crate::SummaryQuad> {
        let li = layer_index.unwrap_or(0);
        self.layers.get(li)
            .map(|l| l.summary.lookup_s(page_id).to_vec())
            .unwrap_or_default()
    }

    /// Look up a summary's reverse connections to a page.
    pub fn summary_to_page(&self, page_id: u32, layer_index: Option<usize>) -> Vec<crate::SummaryQuad> {
        let li = layer_index.unwrap_or(0);
        self.layers.get(li)
            .map(|l| l.summary.lookup_o(page_id).to_vec())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    // Integration tests require a built index on disk.
    // Unit tests for plan_from_patterns and execute_patterns are in query.rs.
}
