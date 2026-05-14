// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Rós Madair WASM client — browser-based query engine over page-based static files.
//!
//! Supports multi-layer indexes: a base layer plus optional corpus layers.
//! Queries run transparently across all layers (or optionally a single layer).
//!
//! ## Usage from JS
//!
//! ```js
//! import { init, SparqlStore } from 'ros-madair-client';
//!
//! await init();
//! const store = new SparqlStore('https://cdn.example.org/base/index/');
//! await store.loadSummary();
//!
//! // Optional: add corpus layers
//! await store.addLayer('https://cdn.example.org/corpus1/index/', 'corpus1');
//!
//! // Query across all layers
//! const results = await store.queryPatterns(`[...]`);
//!
//! // Query specific layer
//! const results = await store.queryPatterns(`[...]`, 'corpus1');
//! ```

pub mod fetch;
pub mod page_cache;
pub mod planner;

use wasm_bindgen::prelude::*;
use std::collections::{HashMap, HashSet};

use ros_madair_core::{
    parse_records, parse_resource_meta, parse_tile_content_header,
    ConceptIntervalIndex, Dictionary, PageMeta, PageRecord, ResourceMap,
    ResourceMeta, SummaryIndex, TileContentHeader,
};

use crate::fetch::{fetch_full, fetch_page_header, fetch_predicate_blocks, fetch_resource_meta, fetch_tile_header, fetch_tile_blob};
use crate::page_cache::PageCache;
use crate::planner::{plan_from_patterns, execute_single_pattern, PatternTerm, TriplePattern};

/// Per-layer index data. Each layer has its own dictionary, summary, pages, etc.
struct Layer {
    base_url: String,
    name: String,
    summary: SummaryIndex,
    dictionary: Dictionary,
    page_meta: Vec<PageMeta>,
    resource_map: Option<ResourceMap>,
    concept_intervals: Option<ConceptIntervalIndex>,
    resource_meta: HashMap<u32, ResourceMeta>,
    cache: PageCache,
    /// Loaded records indexed by (page_id, pred_id) → sorted records.
    records: HashMap<(u32, u32), Vec<PageRecord>>,
    /// Cached tile content headers indexed by page_id.
    tile_headers: HashMap<u32, TileContentHeader>,
    /// Cached full tile file bytes (for tile source bridge).
    tile_file_cache: HashMap<u32, Vec<u8>>,
}

#[wasm_bindgen]
pub struct SparqlStore {
    layers: Vec<Layer>,
}

/// Load index data from a base URL into a Layer.
async fn load_layer(base_url: &str, name: &str) -> Result<Layer, JsValue> {
    let base = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{}/", base_url)
    };

    let summary_bytes = fetch_full(&format!("{}summary.bin", base))
        .await
        .map_err(|e| JsValue::from_str(&e))?;
    let summary = SummaryIndex::from_bytes(&summary_bytes)
        .map_err(|e| JsValue::from_str(&e))?;

    let dict_bytes = fetch_full(&format!("{}dictionary.bin", base))
        .await
        .map_err(|e| JsValue::from_str(&e))?;
    let dictionary = Dictionary::from_bytes(&dict_bytes)
        .map_err(|e| JsValue::from_str(&e))?;

    let meta_bytes = fetch_full(&format!("{}page_meta.json", base))
        .await
        .map_err(|e| JsValue::from_str(&e))?;
    let meta_str = String::from_utf8(meta_bytes)
        .map_err(|e| JsValue::from_str(&format!("Invalid UTF-8 in page_meta: {}", e)))?;
    let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str)
        .map_err(|e| JsValue::from_str(&format!("Invalid page_meta JSON: {}", e)))?;

    let resource_map = match fetch_full(&format!("{}resource_map.bin", base)).await {
        Ok(rm_bytes) => Some(
            ResourceMap::from_bytes(&rm_bytes)
                .map_err(|e| JsValue::from_str(&e))?,
        ),
        Err(_) => None,
    };

    let concept_intervals = match fetch_full(&format!("{}concept_intervals.bin", base)).await {
        Ok(ci_bytes) => Some(
            ConceptIntervalIndex::from_bytes(&ci_bytes)
                .map_err(|e| JsValue::from_str(&e))?,
        ),
        Err(_) => None,
    };

    Ok(Layer {
        base_url: base,
        name: name.to_string(),
        summary,
        dictionary,
        page_meta,
        resource_map,
        concept_intervals,
        resource_meta: HashMap::new(),
        cache: PageCache::new(),
        records: HashMap::new(),
        tile_headers: HashMap::new(),
        tile_file_cache: HashMap::new(),
    })
}

#[wasm_bindgen]
impl SparqlStore {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: &str) -> Self {
        // Store base_url for deferred loading via load_summary()
        let _ = base_url; // used in load_summary
        Self {
            layers: Vec::new(),
        }
    }

    /// Load summary index, dictionary, and page metadata for the base layer.
    /// Call once at init. The base_url passed to the constructor is used.
    #[wasm_bindgen(js_name = "loadSummary")]
    pub async fn load_summary(&mut self, base_url: Option<String>) -> Result<(), JsValue> {
        let url = base_url.unwrap_or_default();
        let layer = load_layer(&url, "base").await?;
        if self.layers.is_empty() {
            self.layers.push(layer);
        } else {
            self.layers[0] = layer;
        }
        Ok(())
    }

    /// Add an additional index layer (e.g. a corpus overlay).
    ///
    /// Each layer has its own dictionary, summary, and pages.
    /// Queries run across all layers by default.
    #[wasm_bindgen(js_name = "addLayer")]
    pub async fn add_layer(&mut self, base_url: &str, name: &str) -> Result<(), JsValue> {
        let layer = load_layer(base_url, name).await?;
        self.layers.push(layer);
        Ok(())
    }

    /// Number of loaded layers.
    #[wasm_bindgen(js_name = "layerCount")]
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Get layer names as JSON array.
    #[wasm_bindgen(js_name = "layerNames")]
    pub fn layer_names(&self) -> JsValue {
        let names: Vec<&str> = self.layers.iter().map(|l| l.name.as_str()).collect();
        let json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    /// Execute a query given triple patterns as JSON.
    ///
    /// Input format: `[{"s": "?x", "p": "http://...", "o": "http://..."}]`
    /// where `?`-prefixed values are variables and others are URIs.
    ///
    /// When `layer` is provided, only that named layer is queried.
    /// Otherwise all layers are queried and results are unioned per-pattern,
    /// then intersected across patterns.
    ///
    /// Returns matching subject URIs as a JSON array of strings.
    #[wasm_bindgen(js_name = "queryPatterns")]
    pub async fn query_patterns(
        &mut self,
        patterns_json: &str,
        layer: Option<String>,
    ) -> Result<JsValue, JsValue> {
        if self.layers.is_empty() {
            return Err(JsValue::from_str("No layers loaded — call loadSummary() first"));
        }

        // Parse patterns from JSON
        let raw_patterns: Vec<RawPattern> = serde_json::from_str(patterns_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid patterns JSON: {}", e)))?;

        let patterns: Vec<TriplePattern> = raw_patterns
            .iter()
            .map(|rp| TriplePattern {
                subject: parse_term(&rp.s),
                predicate: parse_term(&rp.p),
                object: parse_term(&rp.o),
            })
            .collect();

        if patterns.is_empty() {
            return serde_wasm_bindgen::to_value(&Vec::<String>::new())
                .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)));
        }

        // Determine which layers to query
        let layer_indices: Vec<usize> = if let Some(ref name) = layer {
            self.layers.iter()
                .enumerate()
                .filter(|(_, l)| l.name == *name)
                .map(|(i, _)| i)
                .collect()
        } else {
            (0..self.layers.len()).collect()
        };

        if layer_indices.is_empty() {
            return Err(JsValue::from_str(&format!(
                "Unknown layer: '{}'", layer.unwrap_or_default()
            )));
        }

        // For each layer: plan → fetch → execute per-pattern
        // Collect per-pattern URI sets across all layers
        let mut pattern_uri_sets: Vec<HashSet<String>> = vec![HashSet::new(); patterns.len()];

        for &li in &layer_indices {
            // Plan for this layer
            let plan = {
                let l = &self.layers[li];
                plan_from_patterns(
                    &patterns, &l.summary, &l.dictionary, &l.page_meta,
                    l.concept_intervals.as_ref(),
                )
            };
            let reduced = self.layers[li].cache.reduce_plan(&plan);

            // Fetch needed pages for this layer
            for spec in &reduced.pages {
                let page_url = format!("{}pages/page_{:04}.dat", self.layers[li].base_url, spec.page_id);
                let header = fetch_page_header(&page_url)
                    .await
                    .map_err(|e| JsValue::from_str(&e))?;

                let blocks = fetch_predicate_blocks(&page_url, &header, &spec.predicates)
                    .await
                    .map_err(|e| JsValue::from_str(&e))?;

                for (pred_id, block_bytes) in blocks {
                    let records = parse_records(&block_bytes);
                    let count = records.len();
                    self.layers[li].records.insert((spec.page_id, pred_id), records);
                    self.layers[li].cache.mark_loaded(spec.page_id, &[pred_id], count);
                }
            }

            // Execute each pattern independently against this layer's records
            for (pi, pattern) in patterns.iter().enumerate() {
                let l = &self.layers[li];
                let matches = execute_single_pattern(
                    pattern, &l.records, &l.dictionary,
                    l.concept_intervals.as_ref(),
                );
                // Resolve dict_ids to URIs and add to this pattern's set
                for subject_id in matches {
                    if let Some(uri) = l.dictionary.resolve(subject_id) {
                        pattern_uri_sets[pi].insert(uri.to_string());
                    }
                }
            }
        }

        // Intersect across patterns
        let mut result_iter = pattern_uri_sets.into_iter();
        let mut result = result_iter.next().unwrap();
        for set in result_iter {
            result = result.intersection(&set).cloned().collect();
        }

        let mut result_uris: Vec<String> = result.into_iter().collect();
        result_uris.sort();

        serde_wasm_bindgen::to_value(&result_uris)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    /// Reset the cache and loaded records for all layers.
    #[wasm_bindgen(js_name = "resetCache")]
    pub fn reset_cache(&mut self) {
        for layer in &mut self.layers {
            layer.cache = PageCache::new();
            layer.records.clear();
        }
    }

    /// Get cache statistics across all layers.
    #[wasm_bindgen(js_name = "cacheStats")]
    pub fn cache_stats(&self) -> JsValue {
        let (pages, records) = self.layers.iter().fold((0, 0), |(p, r), l| {
            (p + l.cache.page_count(), r + l.cache.record_count())
        });
        let stats = serde_json::json!({
            "pages_loaded": pages,
            "records_loaded": records,
            "layer_count": self.layers.len(),
        });
        JsValue::from_str(&stats.to_string())
    }

    /// Load records for a (page, predicate) pair on a specific layer.
    ///
    /// `layer_index` defaults to 0 (base layer).
    /// Returns JSON array: `[{subject, object, subject_id, object_val}]`
    #[wasm_bindgen(js_name = "loadPredicateRecords")]
    pub async fn load_predicate_records(
        &mut self,
        page_id: u32,
        pred_uri: &str,
        layer_index: Option<usize>,
    ) -> Result<JsValue, JsValue> {
        let li = layer_index.unwrap_or(0);
        let layer = self.layers.get(li)
            .ok_or_else(|| JsValue::from_str(&format!("Invalid layer index: {}", li)))?;

        let pred_id = match layer.dictionary.lookup(pred_uri) {
            Some(id) => id,
            None => return Ok(JsValue::from_str("[]")),
        };

        // Load page data if not cached
        if !self.layers[li].records.contains_key(&(page_id, pred_id)) {
            let page_url = format!("{}pages/page_{:04}.dat", self.layers[li].base_url, page_id);
            let header = fetch_page_header(&page_url)
                .await
                .map_err(|e| JsValue::from_str(&e))?;
            let blocks = fetch_predicate_blocks(&page_url, &header, &[pred_id])
                .await
                .map_err(|e| JsValue::from_str(&e))?;
            for (pid, block_bytes) in blocks {
                let records = parse_records(&block_bytes);
                let count = records.len();
                self.layers[li].records.insert((page_id, pid), records);
                self.layers[li].cache.mark_loaded(page_id, &[pid], count);
            }
        }

        let dict = &self.layers[li].dictionary;
        let result: Vec<serde_json::Value> = self.layers[li]
            .records
            .get(&(page_id, pred_id))
            .map(|recs| {
                recs.iter()
                    .map(|r| {
                        let subject = dict.resolve(r.subject_id).unwrap_or("?");
                        let object = dict.resolve(r.object_val);
                        serde_json::json!({
                            "subject": subject,
                            "object": object,
                            "subject_id": r.subject_id,
                            "object_val": r.object_val,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
        Ok(JsValue::from_str(&json))
    }

    // --- Graph explorer methods ---

    /// Look up a term's dictionary ID. Searches all layers, returns first match.
    #[wasm_bindgen(js_name = "lookupTerm")]
    pub fn lookup_term(&self, uri: &str) -> JsValue {
        for layer in &self.layers {
            if let Some(id) = layer.dictionary.lookup(uri) {
                return JsValue::from(id);
            }
        }
        JsValue::NULL
    }

    /// Resolve a dictionary ID to its term string on a specific layer.
    #[wasm_bindgen(js_name = "resolveTerm")]
    pub fn resolve_term(&self, id: u32, layer_index: Option<usize>) -> JsValue {
        let li = layer_index.unwrap_or(0);
        match self.layers.get(li).and_then(|l| l.dictionary.resolve(id)) {
            Some(term) => JsValue::from_str(term),
            None => JsValue::NULL,
        }
    }

    /// Get the page ID for a resource URI. Searches all layers.
    ///
    /// Returns JSON: `{page_id, layer_index, layer_name}` or null.
    #[wasm_bindgen(js_name = "pageForResource")]
    pub fn page_for_resource(&self, uri: &str) -> JsValue {
        for (li, layer) in self.layers.iter().enumerate() {
            let rmap = match &layer.resource_map {
                Some(r) => r,
                None => continue,
            };
            if let Some(page_id) = layer.dictionary.lookup(uri).and_then(|id| rmap.page_for(id)) {
                let result = serde_json::json!({
                    "page_id": page_id,
                    "layer_index": li,
                    "layer_name": layer.name,
                });
                return JsValue::from_str(&result.to_string());
            }
        }
        JsValue::NULL
    }

    /// Check if a URI is a known resource in any layer.
    #[wasm_bindgen(js_name = "isResource")]
    pub fn is_resource(&self, uri: &str) -> bool {
        self.layers.iter().any(|l| {
            l.resource_map.as_ref().map_or(false, |rmap| {
                l.dictionary.lookup(uri).map(|id| rmap.is_resource(id)).unwrap_or(false)
            })
        })
    }

    /// Get page metadata as JSON array for a specific layer.
    #[wasm_bindgen(js_name = "pageMetaJson")]
    pub fn page_meta_json(&self, layer_index: Option<usize>) -> JsValue {
        let li = layer_index.unwrap_or(0);
        match self.layers.get(li) {
            Some(layer) => {
                let json = serde_json::to_string(&layer.page_meta).unwrap_or_else(|_| "[]".to_string());
                JsValue::from_str(&json)
            }
            None => JsValue::NULL,
        }
    }

    /// Get all forward connections from a page.
    #[wasm_bindgen(js_name = "summaryFromPage")]
    pub fn summary_from_page(&self, page_id: u32, layer_index: Option<usize>) -> JsValue {
        let li = layer_index.unwrap_or(0);
        let layer = match self.layers.get(li) {
            Some(l) => l,
            None => return JsValue::NULL,
        };

        let quads = layer.summary.lookup_s(page_id);
        let result: Vec<serde_json::Value> = quads
            .iter()
            .map(|q| {
                let pred_uri = layer.dictionary.resolve(q.predicate).unwrap_or("?").to_string();
                serde_json::json!({
                    "page_s": q.page_s,
                    "predicate": q.predicate,
                    "pred_uri": pred_uri,
                    "page_o": q.page_o,
                    "edge_count": q.edge_count,
                    "subject_count": q.subject_count,
                })
            })
            .collect();

        let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    /// Get all reverse connections to a page.
    #[wasm_bindgen(js_name = "summaryToPage")]
    pub fn summary_to_page(&self, page_id: u32, layer_index: Option<usize>) -> JsValue {
        let li = layer_index.unwrap_or(0);
        let layer = match self.layers.get(li) {
            Some(l) => l,
            None => return JsValue::NULL,
        };

        let quads = layer.summary.lookup_o(page_id);
        let result: Vec<serde_json::Value> = quads
            .iter()
            .map(|q| {
                let pred_uri = layer.dictionary.resolve(q.predicate).unwrap_or("?").to_string();
                serde_json::json!({
                    "page_s": q.page_s,
                    "predicate": q.predicate,
                    "pred_uri": pred_uri,
                    "page_o": q.page_o,
                    "edge_count": q.edge_count,
                    "subject_count": q.subject_count,
                })
            })
            .collect();

        let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    /// Check whether resource_map was loaded (on base layer).
    #[wasm_bindgen(js_name = "hasResourceMap")]
    pub fn has_resource_map(&self) -> bool {
        self.layers.first().map_or(false, |l| l.resource_map.is_some())
    }

    /// Load resource metadata from a page file's embedded metadata section.
    #[wasm_bindgen(js_name = "loadResourceMeta")]
    pub async fn load_resource_meta(
        &mut self,
        page_id: u32,
        layer_index: Option<usize>,
    ) -> Result<JsValue, JsValue> {
        let li = layer_index.unwrap_or(0);
        if li >= self.layers.len() {
            return Err(JsValue::from_str(&format!("Invalid layer index: {}", li)));
        }

        if !self.layers[li].cache.is_meta_loaded(page_id) {
            let page_url = format!("{}pages/page_{:04}.dat", self.layers[li].base_url, page_id);
            let header = fetch_page_header(&page_url)
                .await
                .map_err(|e| JsValue::from_str(&e))?;

            if let Some((offset, size)) = header.resource_meta_range {
                let meta_bytes = fetch_resource_meta(&page_url, offset, size)
                    .await
                    .map_err(|e| JsValue::from_str(&e))?;
                let metas = parse_resource_meta(&meta_bytes)
                    .map_err(|e| JsValue::from_str(&e))?;
                for m in metas {
                    self.layers[li].resource_meta.insert(m.dict_id, m);
                }
            }
            self.layers[li].cache.mark_meta_loaded(page_id);
        }

        let layer = &self.layers[li];
        let rmap = match &layer.resource_map {
            Some(r) => r,
            None => return Ok(JsValue::from_str("[]")),
        };

        let result: Vec<serde_json::Value> = layer.resource_meta.values()
            .filter(|m| rmap.page_for(m.dict_id) == Some(page_id))
            .map(|m| {
                let uri = layer.dictionary.resolve(m.dict_id).unwrap_or("?");
                serde_json::json!({
                    "dict_id": m.dict_id,
                    "uri": uri,
                    "name": m.name,
                    "slug": m.slug,
                    "model": m.model,
                })
            })
            .collect();

        let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
        Ok(JsValue::from_str(&json))
    }

    /// Look up metadata for a resource URI across all layers.
    #[wasm_bindgen(js_name = "resourceInfo")]
    pub fn resource_info(&self, uri: &str) -> JsValue {
        for layer in &self.layers {
            if let Some(dict_id) = layer.dictionary.lookup(uri) {
                if let Some(m) = layer.resource_meta.get(&dict_id) {
                    let json = serde_json::json!({
                        "name": m.name,
                        "slug": m.slug,
                        "model": m.model,
                    });
                    return JsValue::from_str(&json.to_string());
                }
            }
        }
        JsValue::NULL
    }

    /// List all resource URIs on a given page in a specific layer.
    #[wasm_bindgen(js_name = "resourcesOnPage")]
    pub fn resources_on_page(&self, page_id: u32, layer_index: Option<usize>) -> JsValue {
        let li = layer_index.unwrap_or(0);
        let layer = match self.layers.get(li) {
            Some(l) => l,
            None => return JsValue::from_str("[]"),
        };
        let rmap = match &layer.resource_map {
            Some(r) => r,
            None => return JsValue::from_str("[]"),
        };

        let mut uris: Vec<&str> = Vec::new();
        for i in 0..rmap.len() {
            if rmap.page_for(i as u32) == Some(page_id) {
                if let Some(term) = layer.dictionary.resolve(i as u32) {
                    uris.push(term);
                }
            }
        }

        let json = serde_json::to_string(&uris).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    /// Get all summary quads as JSON for a specific layer.
    #[wasm_bindgen(js_name = "summaryAllQuads")]
    pub fn summary_all_quads(&self, layer_index: Option<usize>) -> JsValue {
        let li = layer_index.unwrap_or(0);
        let layer = match self.layers.get(li) {
            Some(l) => l,
            None => return JsValue::NULL,
        };

        let mut result: Vec<serde_json::Value> = Vec::new();
        for pm in &layer.page_meta {
            for q in layer.summary.lookup_s(pm.page_id) {
                let pred_uri = layer.dictionary.resolve(q.predicate).unwrap_or("?").to_string();
                result.push(serde_json::json!({
                    "page_s": q.page_s,
                    "predicate": q.predicate,
                    "pred_uri": pred_uri,
                    "page_o": q.page_o,
                    "edge_count": q.edge_count,
                    "subject_count": q.subject_count,
                }));
            }
        }

        let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    // --- Tile content methods ---

    /// Load full-fidelity tiles for a resource, returning JSON (StaticTile array).
    ///
    /// Searches all layers for the resource, using the first match.
    /// If `nodegroup_id` is provided, filters to tiles matching that nodegroup.
    #[wasm_bindgen(js_name = "loadTilesForResource")]
    pub async fn load_tiles_for_resource(
        &mut self,
        resource_uri: &str,
        nodegroup_id: Option<String>,
    ) -> Result<JsValue, JsValue> {
        // Find which layer has this resource
        let (li, subject_id, page_id) = self.find_resource(resource_uri)?;

        // Fetch and cache tile header if needed
        if !self.layers[li].tile_headers.contains_key(&page_id) {
            let tile_url = format!("{}tiles/tile_{:04}.dat", self.layers[li].base_url, page_id);
            let header = fetch_tile_header(&tile_url)
                .await
                .map_err(|e| JsValue::from_str(&e))?;
            self.layers[li].tile_headers.insert(page_id, header);
        }

        let header = self.layers[li].tile_headers.get(&page_id).unwrap();
        let entry = header
            .entry_for_subject(subject_id)
            .ok_or_else(|| JsValue::from_str(&format!(
                "No tile entry for subject_id {} in page {}",
                subject_id, page_id
            )))?;

        let tile_url = format!("{}tiles/tile_{:04}.dat", self.layers[li].base_url, page_id);
        let blob = fetch_tile_blob(&tile_url, entry.blob_offset, entry.blob_size)
            .await
            .map_err(|e| JsValue::from_str(&e))?;

        #[derive(serde::Deserialize)]
        struct ResourceBlob {
            tiles: Vec<serde_json::Value>,
            #[serde(default, rename = "__cache")]
            cache: Option<serde_json::Value>,
            #[serde(default, rename = "__scopes")]
            scopes: Option<serde_json::Value>,
        }

        let parsed = if header.version >= 2 {
            rmp_serde::from_slice::<ResourceBlob>(&blob)
                .map_err(|e| JsValue::from_str(&format!("Failed to deserialize v2 tile blob: {}", e)))?
        } else {
            let tiles: Vec<serde_json::Value> = rmp_serde::from_slice(&blob)
                .map_err(|e| JsValue::from_str(&format!("Failed to deserialize v1 tile data: {}", e)))?;
            ResourceBlob { tiles, cache: None, scopes: None }
        };

        let filtered: Vec<&serde_json::Value> = match &nodegroup_id {
            Some(ng_id) => parsed.tiles
                .iter()
                .filter(|t| t.get("nodegroup_id").and_then(|v| v.as_str()) == Some(ng_id.as_str()))
                .collect(),
            None => parsed.tiles.iter().collect(),
        };

        let result = serde_json::json!({
            "tiles": filtered,
            "__cache": parsed.cache,
            "__scopes": parsed.scopes,
        });

        serde_wasm_bindgen::to_value(&result)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    // --- Tile file cache methods (for tile source bridge) ---

    /// Pre-fetch and cache the entire tile file for the page containing this resource.
    /// Call before `getTileBlobSync()` to ensure data is available.
    #[wasm_bindgen(js_name = "prefetchTileFile")]
    pub async fn prefetch_tile_file(&mut self, resource_uri: &str) -> Result<(), JsValue> {
        let (li, _subject_id, page_id) = self.find_resource(resource_uri)?;
        if self.layers[li].tile_file_cache.contains_key(&page_id) {
            return Ok(());
        }
        let tile_url = format!("{}tiles/tile_{:04}.dat", self.layers[li].base_url, page_id);
        let bytes = fetch_full(&tile_url)
            .await
            .map_err(|e| JsValue::from_str(&e))?;
        self.layers[li].tile_file_cache.insert(page_id, bytes);
        Ok(())
    }

    /// Synchronously extract the raw msgpack blob for a resource's tiles from
    /// cached tile files. Returns `Uint8Array`. Must call `prefetchTileFile` first.
    ///
    /// This is the building block for Phase 2's dynamic tile source bridge —
    /// alizarin's JS callback calls this synchronously.
    #[wasm_bindgen(js_name = "getTileBlobSync")]
    pub fn get_tile_blob_sync(
        &self,
        resource_uri: &str,
        nodegroup_id: Option<String>,
    ) -> Result<Vec<u8>, JsValue> {
        let (li, subject_id, page_id) = self.find_resource(resource_uri)?;
        let file_bytes = self.layers[li]
            .tile_file_cache
            .get(&page_id)
            .ok_or_else(|| {
                JsValue::from_str("Tile file not cached — call prefetchTileFile first")
            })?;

        let header = parse_tile_content_header(file_bytes)
            .map_err(|e| JsValue::from_str(&e))?;
        let entry = header.entry_for_subject(subject_id).ok_or_else(|| {
            JsValue::from_str(&format!(
                "No tile entry for subject_id {} in page {}",
                subject_id, page_id
            ))
        })?;

        let start = entry.blob_offset as usize;
        let end = start + entry.blob_size as usize;

        if end > file_bytes.len() {
            return Err(JsValue::from_str(&format!(
                "Tile blob range [{}, {}) exceeds file size {}",
                start, end, file_bytes.len()
            )));
        }

        // If nodegroup filtering requested, we still return the full blob —
        // filtering happens in the consumer (alizarin side). The blob is raw
        // msgpack, and slicing by nodegroup requires deserialization.
        let _ = nodegroup_id;

        Ok(file_bytes[start..end].to_vec())
    }
}

// --- Rust-only accessors (not exposed to JS) ---

impl SparqlStore {
    /// Borrow the loaded dictionary for a layer.
    pub fn dictionary(&self, layer_index: usize) -> Option<&Dictionary> {
        self.layers.get(layer_index).map(|l| &l.dictionary)
    }

    /// Borrow the loaded resource map for a layer.
    pub fn resource_map(&self, layer_index: usize) -> Option<&ResourceMap> {
        self.layers.get(layer_index).and_then(|l| l.resource_map.as_ref())
    }

    /// The base URL for a layer.
    pub fn base_url(&self, layer_index: usize) -> Option<&str> {
        self.layers.get(layer_index).map(|l| l.base_url.as_str())
    }

    /// Find a resource across all layers. Returns (layer_index, subject_id, page_id).
    fn find_resource(&self, uri: &str) -> Result<(usize, u32, u32), JsValue> {
        for (li, layer) in self.layers.iter().enumerate() {
            let rmap = match &layer.resource_map {
                Some(r) => r,
                None => continue,
            };
            if let Some(subject_id) = layer.dictionary.lookup(uri) {
                if let Some(page_id) = rmap.page_for(subject_id) {
                    return Ok((li, subject_id, page_id));
                }
            }
        }
        Err(JsValue::from_str(&format!("Resource not found in any layer: {}", uri)))
    }
}

#[derive(serde::Deserialize)]
struct RawPattern {
    s: String,
    p: String,
    o: String,
}

fn parse_term(s: &str) -> PatternTerm {
    if let Some(var) = s.strip_prefix('?') {
        PatternTerm::Variable(var.to_string())
    } else {
        PatternTerm::Uri(s.to_string())
    }
}
