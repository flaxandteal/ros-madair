// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Rós Madair WASM client — browser-based query engine over page-based static files.
//!
//! ## Usage from JS
//!
//! ```js
//! import { init, SparqlStore } from 'ros-madair-client';
//!
//! await init();
//! const store = new SparqlStore('https://cdn.example.org/ros-madair/');
//! await store.loadSummary();
//!
//! const results = await store.query(`
//!   SELECT ?place WHERE {
//!     ?place <.../node/monument_type> <.../concept/church> .
//!   }
//! `);
//! ```

pub mod fetch;
pub mod page_cache;
pub mod planner;

use wasm_bindgen::prelude::*;
use std::collections::HashMap;

use ros_madair_core::{
    binary_search_object, parse_records, parse_resource_meta,
    Dictionary, PageMeta, PageRecord, ResourceMap, ResourceMeta, SummaryIndex,
    TileContentHeader,
};

use crate::fetch::{fetch_full, fetch_page_header, fetch_predicate_blocks, fetch_resource_meta, fetch_tile_header, fetch_tile_blob};
use crate::page_cache::PageCache;
use crate::planner::{plan_from_patterns, PatternTerm, TriplePattern};

#[wasm_bindgen]
pub struct SparqlStore {
    base_url: String,
    summary: Option<SummaryIndex>,
    dictionary: Option<Dictionary>,
    page_meta: Option<Vec<PageMeta>>,
    resource_map: Option<ResourceMap>,
    /// Loaded resource metadata indexed by dict_id.
    resource_meta: HashMap<u32, ResourceMeta>,
    cache: PageCache,
    /// Loaded records indexed by (page_id, pred_id) → sorted records.
    records: HashMap<(u32, u32), Vec<PageRecord>>,
    /// Cached tile content headers indexed by page_id.
    tile_headers: HashMap<u32, TileContentHeader>,
}

#[wasm_bindgen]
impl SparqlStore {
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: &str) -> Self {
        let base = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{}/", base_url)
        };
        Self {
            base_url: base,
            summary: None,
            dictionary: None,
            page_meta: None,
            resource_map: None,
            resource_meta: HashMap::new(),
            cache: PageCache::new(),
            records: HashMap::new(),
            tile_headers: HashMap::new(),
        }
    }

    /// Load summary index, dictionary, and page metadata. Call once at init.
    pub async fn load_summary(&mut self) -> Result<(), JsValue> {
        let summary_bytes = fetch_full(&format!("{}summary.bin", self.base_url))
            .await
            .map_err(|e| JsValue::from_str(&e))?;
        self.summary = Some(
            SummaryIndex::from_bytes(&summary_bytes)
                .map_err(|e| JsValue::from_str(&e))?,
        );

        let dict_bytes = fetch_full(&format!("{}dictionary.bin", self.base_url))
            .await
            .map_err(|e| JsValue::from_str(&e))?;
        self.dictionary = Some(
            Dictionary::from_bytes(&dict_bytes)
                .map_err(|e| JsValue::from_str(&e))?,
        );

        let meta_bytes = fetch_full(&format!("{}page_meta.json", self.base_url))
            .await
            .map_err(|e| JsValue::from_str(&e))?;
        let meta_str = String::from_utf8(meta_bytes)
            .map_err(|e| JsValue::from_str(&format!("Invalid UTF-8 in page_meta: {}", e)))?;
        self.page_meta = Some(
            serde_json::from_str(&meta_str)
                .map_err(|e| JsValue::from_str(&format!("Invalid page_meta JSON: {}", e)))?,
        );

        // Load resource map (optional — may not exist for older indices)
        match fetch_full(&format!("{}resource_map.bin", self.base_url)).await {
            Ok(rm_bytes) => {
                self.resource_map = Some(
                    ResourceMap::from_bytes(&rm_bytes)
                        .map_err(|e| JsValue::from_str(&e))?,
                );
            }
            Err(_) => {
                // resource_map.bin not available — graph explorer won't work
                // but query functionality is unaffected
                self.resource_map = None;
            }
        }

        Ok(())
    }

    /// Execute a query given triple patterns as JSON.
    ///
    /// Input format: `[{"s": "?x", "p": "http://...", "o": "http://..."}]`
    /// where `?`-prefixed values are variables and others are URIs.
    ///
    /// Returns matching subject IDs as a JSON array of strings.
    pub async fn query_patterns(&mut self, patterns_json: &str) -> Result<JsValue, JsValue> {
        let summary = self
            .summary
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Summary not loaded — call loadSummary() first"))?;
        let dict = self
            .dictionary
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Dictionary not loaded"))?;
        let page_meta = self
            .page_meta
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Page meta not loaded"))?;

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

        // Plan
        let plan = plan_from_patterns(&patterns, summary, dict, page_meta);
        let reduced = self.cache.reduce_plan(&plan);

        // Fetch needed pages
        for spec in &reduced.pages {
            let page_url = format!("{}pages/page_{:04}.dat", self.base_url, spec.page_id);
            let header = fetch_page_header(&page_url)
                .await
                .map_err(|e| JsValue::from_str(&e))?;

            let blocks = fetch_predicate_blocks(&page_url, &header, &spec.predicates)
                .await
                .map_err(|e| JsValue::from_str(&e))?;

            for (pred_id, block_bytes) in blocks {
                let records = parse_records(&block_bytes);
                let count = records.len();
                self.records
                    .insert((spec.page_id, pred_id), records);
                self.cache
                    .mark_loaded(spec.page_id, &[pred_id], count);
            }
        }

        // Execute query over all loaded records.
        let results = execute_patterns(&patterns, &self.records, dict);

        // Return as JSON array of URIs
        let result_uris: Vec<String> = results
            .into_iter()
            .filter_map(|subject_id| dict.resolve(subject_id).map(String::from))
            .collect();

        serde_wasm_bindgen::to_value(&result_uris)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }

    /// Reset the cache and loaded records for a fresh query run.
    pub fn reset_cache(&mut self) {
        self.cache = PageCache::new();
        self.records.clear();
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> JsValue {
        let stats = serde_json::json!({
            "pages_loaded": self.cache.page_count(),
            "records_loaded": self.cache.record_count(),
        });
        JsValue::from_str(&stats.to_string())
    }

    /// Load records for a (page, predicate) pair, fetching the page file if needed.
    ///
    /// Returns JSON array: `[{subject: "uri", object: "uri_or_null", subject_id, object_val}]`
    pub async fn load_predicate_records(
        &mut self,
        page_id: u32,
        pred_uri: &str,
    ) -> Result<JsValue, JsValue> {
        let dict = self
            .dictionary
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Dictionary not loaded"))?;

        let pred_id = match dict.lookup(pred_uri) {
            Some(id) => id,
            None => return Ok(JsValue::from_str("[]")),
        };

        // Load page data if not cached
        if !self.records.contains_key(&(page_id, pred_id)) {
            let page_url = format!("{}pages/page_{:04}.dat", self.base_url, page_id);
            let header = fetch_page_header(&page_url)
                .await
                .map_err(|e| JsValue::from_str(&e))?;
            let blocks = fetch_predicate_blocks(&page_url, &header, &[pred_id])
                .await
                .map_err(|e| JsValue::from_str(&e))?;
            for (pid, block_bytes) in blocks {
                let records = parse_records(&block_bytes);
                let count = records.len();
                self.records.insert((page_id, pid), records);
                self.cache.mark_loaded(page_id, &[pid], count);
            }
        }

        let dict = self.dictionary.as_ref().unwrap();
        let result: Vec<serde_json::Value> = self
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

    /// Look up a term's dictionary ID. Returns null if not found.
    pub fn lookup_term(&self, uri: &str) -> JsValue {
        match self.dictionary.as_ref().and_then(|d| d.lookup(uri)) {
            Some(id) => JsValue::from(id),
            None => JsValue::NULL,
        }
    }

    /// Resolve a dictionary ID to its term string. Returns null if not found.
    pub fn resolve_term(&self, id: u32) -> JsValue {
        match self.dictionary.as_ref().and_then(|d| d.resolve(id)) {
            Some(term) => JsValue::from_str(term),
            None => JsValue::NULL,
        }
    }

    /// Get the page ID for a resource URI. Returns null if not found.
    pub fn page_for_resource(&self, uri: &str) -> JsValue {
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return JsValue::NULL,
        };
        let rmap = match self.resource_map.as_ref() {
            Some(r) => r,
            None => return JsValue::NULL,
        };
        match dict.lookup(uri).and_then(|id| rmap.page_for(id)) {
            Some(page_id) => JsValue::from(page_id),
            None => JsValue::NULL,
        }
    }

    /// Check if a URI is a known resource.
    pub fn is_resource(&self, uri: &str) -> bool {
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return false,
        };
        let rmap = match self.resource_map.as_ref() {
            Some(r) => r,
            None => return false,
        };
        dict.lookup(uri)
            .map(|id| rmap.is_resource(id))
            .unwrap_or(false)
    }

    /// Get page metadata as JSON array.
    pub fn page_meta_json(&self) -> JsValue {
        match &self.page_meta {
            Some(meta) => {
                let json = serde_json::to_string(meta).unwrap_or_else(|_| "[]".to_string());
                JsValue::from_str(&json)
            }
            None => JsValue::NULL,
        }
    }

    /// Get all forward connections from a page (quads where page_s == page_id).
    ///
    /// Returns JSON array: `[{page_s, predicate, pred_uri, page_o, edge_count, subject_count}]`
    pub fn summary_from_page(&self, page_id: u32) -> JsValue {
        let summary = match self.summary.as_ref() {
            Some(s) => s,
            None => return JsValue::NULL,
        };
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return JsValue::NULL,
        };

        let quads = summary.lookup_s(page_id);
        let result: Vec<serde_json::Value> = quads
            .iter()
            .map(|q| {
                let pred_uri = dict.resolve(q.predicate).unwrap_or("?").to_string();
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

    /// Get all reverse connections to a page (quads where page_o == page_id).
    ///
    /// Returns JSON array: `[{page_s, predicate, pred_uri, page_o, edge_count, subject_count}]`
    pub fn summary_to_page(&self, page_id: u32) -> JsValue {
        let summary = match self.summary.as_ref() {
            Some(s) => s,
            None => return JsValue::NULL,
        };
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return JsValue::NULL,
        };

        let quads = summary.lookup_o(page_id);
        let result: Vec<serde_json::Value> = quads
            .iter()
            .map(|q| {
                let pred_uri = dict.resolve(q.predicate).unwrap_or("?").to_string();
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

    /// Check whether resource_map was loaded.
    pub fn has_resource_map(&self) -> bool {
        self.resource_map.is_some()
    }

    /// Load resource metadata from a page file's embedded metadata section.
    ///
    /// Fetches the metadata via a single range request if not already cached.
    /// Returns JSON array: `[{dict_id, name, slug, model}]`
    pub async fn load_resource_meta(&mut self, page_id: u32) -> Result<JsValue, JsValue> {
        if !self.cache.is_meta_loaded(page_id) {
            let page_url = format!("{}pages/page_{:04}.dat", self.base_url, page_id);
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
                    self.resource_meta.insert(m.dict_id, m);
                }
            }
            self.cache.mark_meta_loaded(page_id);
        }

        // Return metadata for this page's resources
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return Ok(JsValue::from_str("[]")),
        };
        let rmap = match self.resource_map.as_ref() {
            Some(r) => r,
            None => return Ok(JsValue::from_str("[]")),
        };

        let result: Vec<serde_json::Value> = self.resource_meta.values()
            .filter(|m| rmap.page_for(m.dict_id) == Some(page_id))
            .map(|m| {
                let uri = dict.resolve(m.dict_id).unwrap_or("?");
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

    /// Look up metadata for a resource URI.
    ///
    /// Returns JSON: `{name, slug, model}` or null if not loaded.
    /// The page containing this resource must have been loaded via
    /// `load_resource_meta` first.
    pub fn resource_info(&self, uri: &str) -> JsValue {
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return JsValue::NULL,
        };
        let dict_id = match dict.lookup(uri) {
            Some(id) => id,
            None => return JsValue::NULL,
        };
        match self.resource_meta.get(&dict_id) {
            Some(m) => {
                let json = serde_json::json!({
                    "name": m.name,
                    "slug": m.slug,
                    "model": m.model,
                });
                JsValue::from_str(&json.to_string())
            }
            None => JsValue::NULL,
        }
    }

    /// List all resource URIs on a given page. Returns JSON array of strings.
    pub fn resources_on_page(&self, page_id: u32) -> JsValue {
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return JsValue::from_str("[]"),
        };
        let rmap = match self.resource_map.as_ref() {
            Some(r) => r,
            None => return JsValue::from_str("[]"),
        };

        let mut uris: Vec<&str> = Vec::new();
        for i in 0..rmap.len() {
            if rmap.page_for(i as u32) == Some(page_id) {
                if let Some(term) = dict.resolve(i as u32) {
                    uris.push(term);
                }
            }
        }

        let json = serde_json::to_string(&uris).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    /// Get all summary quads as JSON (for building the full page-level overview graph).
    ///
    /// Returns JSON array: `[{page_s, predicate, pred_uri, page_o, edge_count, subject_count}]`
    pub fn summary_all_quads(&self) -> JsValue {
        let summary = match self.summary.as_ref() {
            Some(s) => s,
            None => return JsValue::NULL,
        };
        let dict = match self.dictionary.as_ref() {
            Some(d) => d,
            None => return JsValue::NULL,
        };

        // Iterate all pages and collect their forward quads (SPO order covers everything)
        let mut result: Vec<serde_json::Value> = Vec::new();
        if let Some(meta) = &self.page_meta {
            for pm in meta {
                for q in summary.lookup_s(pm.page_id) {
                    let pred_uri = dict.resolve(q.predicate).unwrap_or("?").to_string();
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
        }

        let json = serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string());
        JsValue::from_str(&json)
    }

    // --- Tile content methods ---

    /// Load full-fidelity tiles for a resource, returning JSON (StaticTile array).
    ///
    /// If `nodegroup_id` is provided, filters to tiles matching that nodegroup.
    /// Uses resource_map for O(1) page lookup, then Range requests to tile content file.
    pub async fn load_tiles_for_resource(
        &mut self,
        resource_uri: &str,
        nodegroup_id: Option<String>,
    ) -> Result<JsValue, JsValue> {
        let dict = self
            .dictionary
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Dictionary not loaded"))?;
        let rmap = self
            .resource_map
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Resource map not loaded"))?;

        let subject_id = dict
            .lookup(resource_uri)
            .ok_or_else(|| JsValue::from_str(&format!("Unknown resource URI: {}", resource_uri)))?;

        let page_id = rmap
            .page_for(subject_id)
            .ok_or_else(|| JsValue::from_str(&format!("No page for subject_id {}", subject_id)))?;

        // Fetch and cache tile header if needed
        if !self.tile_headers.contains_key(&page_id) {
            let tile_url = format!("{}tiles/tile_{:04}.dat", self.base_url, page_id);
            let header = fetch_tile_header(&tile_url)
                .await
                .map_err(|e| JsValue::from_str(&e))?;
            self.tile_headers.insert(page_id, header);
        }

        let header = self.tile_headers.get(&page_id).unwrap();
        let entry = header
            .entry_for_subject(subject_id)
            .ok_or_else(|| JsValue::from_str(&format!(
                "No tile entry for subject_id {} in page {}",
                subject_id, page_id
            )))?;

        // Fetch the blob
        let tile_url = format!("{}tiles/tile_{:04}.dat", self.base_url, page_id);
        let blob = fetch_tile_blob(&tile_url, entry.blob_offset, entry.blob_size)
            .await
            .map_err(|e| JsValue::from_str(&e))?;

        // Deserialize MessagePack → Vec<serde_json::Value>
        // We use Value rather than StaticTile to avoid pulling alizarin-core into WASM.
        let tiles: Vec<serde_json::Value> = rmp_serde::from_slice(&blob)
            .map_err(|e| JsValue::from_str(&format!("Failed to deserialize tile data: {}", e)))?;

        // Optionally filter by nodegroup_id
        let filtered: Vec<&serde_json::Value> = match &nodegroup_id {
            Some(ng_id) => tiles
                .iter()
                .filter(|t| t.get("nodegroup_id").and_then(|v| v.as_str()) == Some(ng_id.as_str()))
                .collect(),
            None => tiles.iter().collect(),
        };

        serde_wasm_bindgen::to_value(&filtered)
            .map_err(|e| JsValue::from_str(&format!("Serialization error: {}", e)))
    }
}

// --- Rust-only accessors (not exposed to JS) ---

impl SparqlStore {
    /// Borrow the loaded dictionary, if any.
    pub fn dictionary(&self) -> Option<&Dictionary> {
        self.dictionary.as_ref()
    }

    /// Borrow the loaded resource map, if any.
    pub fn resource_map(&self) -> Option<&ResourceMap> {
        self.resource_map.as_ref()
    }

    /// The base URL this store fetches from.
    pub fn base_url(&self) -> &str {
        &self.base_url
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

/// Execute triple patterns over all loaded records.
fn execute_patterns(
    patterns: &[TriplePattern],
    records: &HashMap<(u32, u32), Vec<PageRecord>>,
    dict: &Dictionary,
) -> Vec<u32> {
    if patterns.is_empty() {
        return Vec::new();
    }

    let mut result_sets: Vec<std::collections::HashSet<u32>> = Vec::new();

    for pattern in patterns {
        let pred_uri = match &pattern.predicate {
            PatternTerm::Uri(u) => u.as_str(),
            PatternTerm::Variable(_) => continue, // TODO: variable predicates
        };

        let pred_id = match dict.lookup(pred_uri) {
            Some(id) => id,
            None => {
                result_sets.push(std::collections::HashSet::new());
                continue;
            }
        };

        let mut matches = std::collections::HashSet::new();

        // Scan all loaded blocks for this predicate
        for (&(_, pid), recs) in records {
            if pid != pred_id {
                continue;
            }

            match &pattern.object {
                PatternTerm::Uri(obj_uri) => {
                    if let Some(obj_id) = dict.lookup(obj_uri) {
                        let (lo, hi) = binary_search_object(recs, obj_id);
                        for rec in &recs[lo..hi] {
                            matches.insert(rec.subject_id);
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
