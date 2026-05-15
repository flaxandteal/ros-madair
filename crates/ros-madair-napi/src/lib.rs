// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! NAPI bindings for Rós Madair — Node.js native query engine.
//!
//! Wraps [`ros_madair_core::LocalQueryEngine`] with `#[napi]` attributes,
//! providing a synchronous, filesystem-backed alternative to the WASM
//! `SparqlStore`.

use std::path::Path;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use ros_madair_core::LocalQueryEngine;

fn engine_err(e: String) -> napi::Error {
    napi::Error::from_reason(e)
}

#[napi]
pub struct NapiSparqlStore {
    engine: LocalQueryEngine,
}

#[napi]
impl NapiSparqlStore {
    /// Open an index directory.
    ///
    /// Loads dictionary, summary, resource_map, concept indexes, and page
    /// metadata synchronously from disk.
    #[napi(constructor)]
    pub fn new(index_dir: String, base_uri: String) -> Result<Self> {
        let engine = LocalQueryEngine::open(Path::new(&index_dir), &base_uri)
            .map_err(engine_err)?;
        Ok(Self { engine })
    }

    /// Add an additional index layer (e.g. a corpus overlay).
    #[napi(js_name = "addLayer")]
    pub fn add_layer(&mut self, index_dir: String, base_uri: String, name: String) -> Result<()> {
        self.engine
            .add_layer(Path::new(&index_dir), &base_uri, &name)
            .map_err(engine_err)
    }

    /// Number of loaded layers.
    #[napi(js_name = "layerCount")]
    pub fn layer_count(&self) -> u32 {
        self.engine.layer_count() as u32
    }

    /// Get layer names as JSON array.
    #[napi(js_name = "layerNames")]
    pub fn layer_names(&self) -> String {
        serde_json::to_string(&self.engine.layer_names()).unwrap_or_else(|_| "[]".to_string())
    }

    /// Execute triple patterns (JSON) against the index.
    ///
    /// Input: `[{"s": "?x", "p": "http://...", "o": "http://..."}]`
    /// Returns: JSON array of matching subject URIs.
    #[napi(js_name = "queryPatterns")]
    pub fn query_patterns(
        &self,
        patterns_json: String,
        layer: Option<String>,
    ) -> Result<String> {
        let raw_patterns: Vec<RawPattern> = serde_json::from_str(&patterns_json)
            .map_err(|e| napi::Error::from_reason(format!("Invalid patterns JSON: {e}")))?;

        let patterns: Vec<ros_madair_core::TriplePattern> = raw_patterns
            .iter()
            .map(|rp| ros_madair_core::TriplePattern {
                subject: parse_term(&rp.s),
                predicate: parse_term(&rp.p),
                object: parse_term(&rp.o),
            })
            .collect();

        let uris = self.engine
            .query_patterns_multi(&patterns, layer.as_deref())
            .map_err(engine_err)?;

        serde_json::to_string(&uris)
            .map_err(|e| napi::Error::from_reason(format!("Serialization error: {e}")))
    }

    /// Look up a term's dictionary ID. Searches all layers, returns first match.
    #[napi(js_name = "lookupTerm")]
    pub fn lookup_term(&self, uri: String) -> Option<u32> {
        self.engine.dictionary().lookup(&uri)
    }

    /// Resolve a dictionary ID to its term string.
    #[napi(js_name = "resolveTerm")]
    pub fn resolve_term(&self, id: u32, layer_index: Option<u32>) -> Option<String> {
        let li = layer_index.unwrap_or(0) as usize;
        self.engine
            .layer_dictionary(li)
            .and_then(|d| d.resolve(id).map(String::from))
    }

    /// Get the page ID for a resource URI. Searches all layers.
    ///
    /// Returns JSON `{"page_id": N, "layer_index": N}` or null.
    #[napi(js_name = "pageForResource")]
    pub fn page_for_resource(&self, uri: String) -> Option<String> {
        match self.engine.find_resource(&uri) {
            Ok((li, _dict_id, page_id)) => {
                let result = serde_json::json!({
                    "page_id": page_id,
                    "layer_index": li,
                });
                Some(result.to_string())
            }
            Err(_) => None,
        }
    }

    /// Check if a URI is a known resource in any layer.
    #[napi(js_name = "isResource")]
    pub fn is_resource(&self, uri: String) -> bool {
        self.engine.find_resource(&uri).is_ok()
    }

    /// Get page metadata as JSON array for a specific layer.
    #[napi(js_name = "pageMetaJson")]
    pub fn page_meta_json(&self, layer_index: Option<u32>) -> String {
        let li = layer_index.unwrap_or(0) as usize;
        // page_meta() returns base layer; for other layers we'd need an accessor
        if li == 0 {
            serde_json::to_string(self.engine.page_meta())
                .unwrap_or_else(|_| "[]".to_string())
        } else {
            "[]".to_string()
        }
    }

    /// Load resource metadata from a page file.
    ///
    /// Returns JSON array of `{dict_id, uri, name, slug, model}`.
    #[napi(js_name = "loadResourceMeta")]
    pub fn load_resource_meta(
        &self,
        page_id: u32,
        layer_index: Option<u32>,
    ) -> Result<String> {
        let li = layer_index.map(|x| x as usize);
        let metas = self.engine
            .load_resource_meta(page_id, li)
            .map_err(engine_err)?;

        let rmap = self.engine.resource_map();
        let dict = self.engine.dictionary();

        let result: Vec<serde_json::Value> = metas.iter()
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

        serde_json::to_string(&result)
            .map_err(|e| napi::Error::from_reason(format!("Serialization error: {e}")))
    }

    /// List all resource URIs on a given page.
    ///
    /// Returns JSON array of URI strings.
    #[napi(js_name = "resourcesOnPage")]
    pub fn resources_on_page(&self, page_id: u32, layer_index: Option<u32>) -> String {
        let li = layer_index.unwrap_or(0) as usize;
        let rmap = match self.engine.layer_resource_map(li) {
            Some(r) => r,
            None => return "[]".to_string(),
        };
        let dict = match self.engine.layer_dictionary(li) {
            Some(d) => d,
            None => return "[]".to_string(),
        };

        let mut uris: Vec<&str> = Vec::new();
        for i in 0..rmap.len() {
            if rmap.page_for(i as u32) == Some(page_id) {
                if let Some(term) = dict.resolve(i as u32) {
                    uris.push(term);
                }
            }
        }

        serde_json::to_string(&uris).unwrap_or_else(|_| "[]".to_string())
    }

    /// Load tiles for a resource, optionally filtering by nodegroup.
    ///
    /// Returns JSON: `{"tiles": [...], "__cache": ..., "__scopes": ...}`
    #[napi(js_name = "loadTilesForResource")]
    pub fn load_tiles_for_resource(
        &self,
        resource_uri: String,
        nodegroup_id: Option<String>,
    ) -> Result<String> {
        let result = self.engine
            .load_tiles_for_resource(&resource_uri, nodegroup_id.as_deref())
            .map_err(engine_err)?;

        serde_json::to_string(&result)
            .map_err(|e| napi::Error::from_reason(format!("Serialization error: {e}")))
    }

    /// Get forward summary connections from a page.
    ///
    /// Returns JSON array of summary quad objects.
    #[napi(js_name = "summaryFromPage")]
    pub fn summary_from_page(&self, page_id: u32, layer_index: Option<u32>) -> String {
        let li = layer_index.map(|x| x as usize);
        let quads = self.engine.summary_from_page(page_id, li);
        let dict = self.engine.dictionary();

        let result: Vec<serde_json::Value> = quads.iter()
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

        serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string())
    }

    /// Get reverse summary connections to a page.
    ///
    /// Returns JSON array of summary quad objects.
    #[napi(js_name = "summaryToPage")]
    pub fn summary_to_page(&self, page_id: u32, layer_index: Option<u32>) -> String {
        let li = layer_index.map(|x| x as usize);
        let quads = self.engine.summary_to_page(page_id, li);
        let dict = self.engine.dictionary();

        let result: Vec<serde_json::Value> = quads.iter()
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

        serde_json::to_string(&result).unwrap_or_else(|_| "[]".to_string())
    }
}

#[derive(serde::Deserialize)]
struct RawPattern {
    s: String,
    p: String,
    o: String,
}

fn parse_term(s: &str) -> ros_madair_core::PatternTerm {
    if let Some(var) = s.strip_prefix('?') {
        ros_madair_core::PatternTerm::Variable(var.to_string())
    } else {
        ros_madair_core::PatternTerm::Uri(s.to_string())
    }
}
