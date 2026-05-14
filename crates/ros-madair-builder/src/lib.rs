// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

// PyO3 macro expansion generates identity conversions for PyResult return types.
#![allow(clippy::useless_conversion)]

//! PyO3 bindings for RosMadair build-time index generation.
//!
//! Reads alizarin graph definitions and resource tiles, builds the page-based
//! index (summary quads + per-page binary files + dictionary), and writes them
//! to a static output directory.

use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use alizarin_core::graph::{
    StaticGraph, StaticResource, StaticResourceDescriptors, StaticResourceMetadata, StaticTile,
};
use ros_madair_core::{
    build_to_memory, parse_tile_content_header,
    ConceptIntervalIndex, ConceptTree,
    Dictionary, LocalQueryEngine, PageMeta,
    ResourceMap, SummaryIndex,
};

/// A resource with its parsed tiles, ready for indexing.
struct ResourceData {
    resource_id: String,
    graph_id: String,
    tiles: Vec<StaticTile>,
}

#[pyclass]
pub struct IndexBuilder {
    base_uri: String,
    graphs: HashMap<String, StaticGraph>,
    resources: Vec<ResourceData>,
    /// resource_id → (name, slug, model) for embedding in page files.
    resource_metadata: HashMap<String, (String, String, String)>,
    /// Parsed SKOS collections for concept hierarchy export.
    vocabulary_collections: Vec<alizarin_core::skos::SkosCollection>,
}

#[pymethods]
impl IndexBuilder {
    #[new]
    fn new(base_uri: String) -> Self {
        Self {
            base_uri,
            graphs: HashMap::new(),
            resources: Vec::new(),
            resource_metadata: HashMap::new(),
            vocabulary_collections: Vec::new(),
        }
    }

    /// Set metadata for a resource to be embedded in its page file.
    fn set_resource_meta(&mut self, resource_id: &str, name: &str, slug: &str, model: &str) {
        self.resource_metadata.insert(
            resource_id.to_string(),
            (name.to_string(), slug.to_string(), model.to_string()),
        );
    }

    /// Add a graph definition (JSON string).
    fn add_graph(&mut self, graph_json: &str) -> PyResult<()>{
        let mut graph: StaticGraph = serde_json::from_str(graph_json)
            .map_err(|e| PyValueError::new_err(format!("Invalid graph JSON: {e}")))?;
        graph.build_indices();
        let graph_id = graph.graphid.clone();
        self.graphs.insert(graph_id, graph);
        Ok(())
    }

    /// Add resources for a graph (JSON array of resource objects with tiles).
    ///
    /// Accepts both flat format (`{resourceinstanceid, tiles}`) and
    /// StaticResource format (`{resourceinstance: {resourceinstanceid, ...}, tiles}`).
    fn add_resources(&mut self, graph_id: &str, resources_json: &str) -> PyResult<()>{
        #[derive(serde::Deserialize)]
        struct ResourceInstanceMeta {
            resourceinstanceid: Option<String>,
        }

        #[derive(serde::Deserialize)]
        struct ResourceInput {
            resourceinstanceid: Option<String>,
            resourceinstance_id: Option<String>,
            resourceinstance: Option<ResourceInstanceMeta>,
            tiles: Option<Vec<StaticTile>>,
        }

        let inputs: Vec<ResourceInput> = serde_json::from_str(resources_json)
            .map_err(|e| PyValueError::new_err(format!("Invalid resources JSON: {e}")))?;

        for input in inputs {
            let resource_id = input
                .resourceinstanceid
                .or(input.resourceinstance_id)
                .or_else(|| input.resourceinstance.and_then(|ri| ri.resourceinstanceid))
                .ok_or_else(|| PyValueError::new_err("Resource missing ID"))?;
            let tiles = input.tiles.unwrap_or_default();
            self.resources.push(ResourceData {
                resource_id,
                graph_id: graph_id.to_string(),
                tiles,
            });
        }

        Ok(())
    }

    /// Add vocabulary data from a SKOS RDF/XML string.
    ///
    /// Parsed collections will be written as `concept_hierarchy.json` during build,
    /// enabling label-based concept resolution without runtime vocabulary loading.
    fn add_vocabulary_xml(&mut self, xml_content: &str, base_uri: &str) -> PyResult<()> {
        let collections = alizarin_core::skos::parse_skos_to_collections(xml_content, base_uri)
            .map_err(|e| PyValueError::new_err(format!("Failed to parse SKOS XML: {e}")))?;
        self.vocabulary_collections.extend(collections);
        Ok(())
    }

    /// Add vocabulary data from a JSON string (serialized SkosCollection or array thereof).
    ///
    /// Parsed collections will be written as `concept_hierarchy.json` during build.
    fn add_vocabulary_json(&mut self, json_content: &str) -> PyResult<()> {
        if let Ok(coll) = serde_json::from_str::<alizarin_core::skos::SkosCollection>(json_content) {
            self.vocabulary_collections.push(coll);
        } else if let Ok(colls) = serde_json::from_str::<Vec<alizarin_core::skos::SkosCollection>>(json_content) {
            self.vocabulary_collections.extend(colls);
        } else {
            return Err(PyValueError::new_err("Failed to parse JSON as SkosCollection or Vec<SkosCollection>"));
        }
        Ok(())
    }

    /// Build the index and write output files.
    ///
    /// Delegates to [`ros_madair_core::build_to_memory`] for the core build
    /// pipeline, then writes the resulting artifacts to `output_dir`.
    #[pyo3(signature = (output_dir, page_size=None))]
    fn build(&self, output_dir: &str, page_size: Option<usize>) -> PyResult<()> {
        let static_resources = self.to_static_resources();

        let artifacts = build_to_memory(
            &self.base_uri,
            &self.graphs,
            &static_resources,
            &self.vocabulary_collections,
            page_size,
        )
        .map_err(|e| PyValueError::new_err(format!("Build failed: {e}")))?;

        let output = PathBuf::from(output_dir);
        for (name, bytes) in &artifacts {
            let path = output.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| PyValueError::new_err(format!("Failed to create dir for {name}: {e}")))?;
            }
            fs::write(&path, bytes)
                .map_err(|e| PyValueError::new_err(format!("Failed to write {name}: {e}")))?;
        }

        Ok(())
    }
}

impl IndexBuilder {
    /// Convert internal `ResourceData` to `StaticResource` for `build_to_memory`.
    fn to_static_resources(&self) -> Vec<StaticResource> {
        self.resources
            .iter()
            .map(|r| {
                let (name, slug, _model) = self.resource_metadata
                    .get(&r.resource_id)
                    .cloned()
                    .unwrap_or_default();
                StaticResource {
                    resourceinstance: StaticResourceMetadata {
                        descriptors: StaticResourceDescriptors {
                            name: if name.is_empty() { None } else { Some(name.clone()) },
                            slug: if slug.is_empty() { None } else { Some(slug) },
                            description: None,
                            map_popup: None,
                        },
                        graph_id: r.graph_id.clone(),
                        name,
                        resourceinstanceid: r.resource_id.clone(),
                        publication_id: None,
                        principaluser_id: None,
                        legacyid: None,
                        graph_publication_id: None,
                        createdtime: None,
                        lastmodified: None,
                    },
                    tiles: Some(r.tiles.clone()),
                    metadata: HashMap::new(),
                    cache: None,
                    scopes: None,
                    tiles_loaded: None,
                }
            })
            .collect()
    }
}

// =============================================================================
// Index Reader — exposes ros-madair-core binary parsers to Python
// =============================================================================

/// Read-side counterpart to IndexBuilder.
///
/// Loads dictionary, resource_map, summary, and page metadata from a built
/// index directory. Supports both tile reading and summary-driven queries
/// using the same Rust logic as the WASM client.
#[pyclass]
pub struct IndexReader {
    base_uri: String,
    dict: Dictionary,
    resource_map: ResourceMap,
    concept_intervals: Option<ConceptIntervalIndex>,
    concept_tree: Option<ConceptTree>,
    index_dir: PathBuf,
    summary: SummaryIndex,
    page_meta: Vec<PageMeta>,
    /// Additional layers loaded via `add_layer()`. Each entry is
    /// (index_dir, base_uri, name).
    extra_layers: Vec<(PathBuf, String, String)>,
}

#[pymethods]
impl IndexReader {
    /// Open an index directory previously created by `IndexBuilder.build()`.
    #[new]
    fn new(index_dir: &str, base_uri: &str) -> PyResult<Self> {
        let index_dir = PathBuf::from(index_dir);

        let dict_bytes = fs::read(index_dir.join("dictionary.bin"))
            .map_err(|e| PyValueError::new_err(format!("Failed to read dictionary.bin: {e}")))?;
        let dict = Dictionary::from_bytes(&dict_bytes)
            .map_err(|e| PyValueError::new_err(format!("Failed to parse dictionary: {e}")))?;

        let rmap_bytes = fs::read(index_dir.join("resource_map.bin"))
            .map_err(|e| PyValueError::new_err(format!("Failed to read resource_map.bin: {e}")))?;
        let resource_map = ResourceMap::from_bytes(&rmap_bytes)
            .map_err(|e| PyValueError::new_err(format!("Failed to parse resource_map: {e}")))?;

        let summary_bytes = fs::read(index_dir.join("summary.bin"))
            .map_err(|e| PyValueError::new_err(format!("Failed to read summary.bin: {e}")))?;
        let summary = SummaryIndex::from_bytes(&summary_bytes)
            .map_err(|e| PyValueError::new_err(format!("Failed to parse summary: {e}")))?;

        let meta_str = fs::read_to_string(index_dir.join("page_meta.json"))
            .map_err(|e| PyValueError::new_err(format!("Failed to read page_meta.json: {e}")))?;
        let page_meta: Vec<PageMeta> = serde_json::from_str(&meta_str)
            .map_err(|e| PyValueError::new_err(format!("Failed to parse page_meta.json: {e}")))?;

        // Load concept intervals (optional — older indices won't have this)
        let concept_intervals = match fs::read(index_dir.join("concept_intervals.bin")) {
            Ok(ci_bytes) => {
                match ConceptIntervalIndex::from_bytes(&ci_bytes) {
                    Ok(ci) => Some(ci),
                    Err(e) => {
                        eprintln!("Warning: Failed to parse concept_intervals.bin: {e}");
                        None
                    }
                }
            }
            Err(_) => None,
        };

        // Load concept tree (optional — older indices won't have this)
        let concept_tree = match fs::read(index_dir.join("concept_tree.bin")) {
            Ok(ct_bytes) => {
                match ConceptTree::from_bytes(&ct_bytes) {
                    Ok(ct) => Some(ct),
                    Err(e) => {
                        eprintln!("Warning: Failed to parse concept_tree.bin: {e}");
                        None
                    }
                }
            }
            Err(_) => None,
        };

        Ok(Self {
            base_uri: base_uri.to_string(),
            dict,
            resource_map,
            concept_intervals,
            concept_tree,
            index_dir,
            summary,
            page_meta,
            extra_layers: Vec::new(),
        })
    }

    /// Add an additional index layer (e.g. a corpus overlay).
    ///
    /// The layer is loaded lazily when a query is executed. `name` is used
    /// for optional layer filtering in queries.
    fn add_layer(&mut self, index_dir: &str, base_uri: &str, name: &str) -> PyResult<()> {
        let dir = PathBuf::from(index_dir);
        if !dir.join("dictionary.bin").exists() {
            return Err(PyValueError::new_err(format!(
                "Not a valid index directory (missing dictionary.bin): {}",
                index_dir
            )));
        }
        self.extra_layers.push((dir, base_uri.to_string(), name.to_string()));
        Ok(())
    }

    /// Number of loaded layers (base + extras).
    fn layer_count(&self) -> usize {
        1 + self.extra_layers.len()
    }

    /// List all resource IDs (without URI prefix) in the index.
    fn list_resource_ids(&self) -> Vec<String> {
        let prefix = ros_madair_core::uri::resource_prefix(&self.base_uri);
        let mut ids = Vec::new();
        for i in 0..self.dict.len() {
            if self.resource_map.is_resource(i as u32) {
                if let Some(term) = self.dict.resolve(i as u32) {
                    if let Some(rid) = term.strip_prefix(&prefix) {
                        ids.push(rid.to_string());
                    }
                }
            }
        }
        ids
    }

    /// Look up a term string by dictionary ID.
    fn resolve_term(&self, dict_id: u32) -> Option<String> {
        self.dict.resolve(dict_id).map(String::from)
    }

    /// Look up a dictionary ID by term string.
    fn lookup_term(&self, term: &str) -> Option<u32> {
        self.dict.lookup(term)
    }

    /// Get the page ID for a resource (by bare resource ID, no URI prefix).
    fn page_for_resource(&self, resource_id: &str) -> Option<u32> {
        let uri = ros_madair_core::uri::resource_uri(&self.base_uri, resource_id);
        let dict_id = self.dict.lookup(&uri)?;
        self.resource_map.page_for(dict_id)
    }

    /// Read the raw tile blob (MessagePack bytes) for a resource from its tile file.
    ///
    /// Returns bytes that can be decoded with msgpack to get `Vec<StaticTile>`.
    fn read_tile_blob(&self, resource_id: &str) -> PyResult<Option<Vec<u8>>> {
        let uri = ros_madair_core::uri::resource_uri(&self.base_uri, resource_id);
        let dict_id = match self.dict.lookup(&uri) {
            Some(id) => id,
            None => return Ok(None),
        };
        let page_id = match self.resource_map.page_for(dict_id) {
            Some(p) => p,
            None => return Ok(None),
        };

        let tile_path = self.index_dir.join("tiles").join(format!("tile_{:04}.dat", page_id));
        if !tile_path.exists() {
            return Ok(None);
        }

        let data = fs::read(&tile_path)
            .map_err(|e| PyValueError::new_err(format!("Failed to read {}: {e}", tile_path.display())))?;

        let header = parse_tile_content_header(&data)
            .map_err(|e| PyValueError::new_err(format!("Failed to parse tile header: {e}")))?;

        match header.entry_for_subject(dict_id) {
            Some(entry) => {
                let start = entry.blob_offset as usize;
                let end = start + entry.blob_size as usize;
                if end > data.len() {
                    return Err(PyValueError::new_err("Tile blob extends beyond file"));
                }
                Ok(Some(data[start..end].to_vec()))
            }
            None => Ok(None),
        }
    }

    /// Return list of resource IDs assigned to a given page.
    fn resources_for_page(&self, page_id: u32) -> Vec<String> {
        let prefix = ros_madair_core::uri::resource_prefix(&self.base_uri);
        let mut ids = Vec::new();
        for i in 0..self.dict.len() {
            let dict_id = i as u32;
            if self.resource_map.page_for(dict_id) == Some(page_id) {
                if let Some(term) = self.dict.resolve(dict_id) {
                    if let Some(rid) = term.strip_prefix(&prefix) {
                        ids.push(rid.to_string());
                    }
                }
            }
        }
        ids
    }

    /// Read tiles for a resource and return them as a JSON string.
    ///
    /// Decodes the MessagePack blob and re-serializes as JSON for interop
    /// with alizarin's `build_tree_from_tiles(tiles_json, ...)`.
    fn read_tiles_json(&self, resource_id: &str) -> PyResult<Option<String>> {
        let blob = match self.read_tile_blob(resource_id)? {
            Some(b) => b,
            None => return Ok(None),
        };

        let tiles: Vec<StaticTile> = rmp_serde::from_slice(&blob)
            .map_err(|e| PyValueError::new_err(format!("Failed to decode msgpack tiles: {e}")))?;

        let json = serde_json::to_string(&tiles)
            .map_err(|e| PyValueError::new_err(format!("Failed to serialize tiles to JSON: {e}")))?;

        Ok(Some(json))
    }

    // ------------------------------------------------------------------
    // Query methods (summary-driven, fetch only relevant pages)
    // ------------------------------------------------------------------

    /// Query by predicate alias and optional object URI.
    ///
    /// `pred_alias` is a node alias (e.g. "type", "name"). It is expanded to
    /// the full predicate URI `{base_uri}node/{alias}`.
    ///
    /// `object_uri` is an optional exact object URI to filter on (e.g. a
    /// concept URI). Pass `None` to match all objects for this predicate.
    ///
    /// `layer` optionally restricts the search to a named layer.
    ///
    /// Returns resource IDs (bare UUIDs, no URI prefix) that match.
    #[pyo3(signature = (pred_alias, object_uri=None, layer=None))]
    fn query(&self, pred_alias: &str, object_uri: Option<&str>, layer: Option<&str>) -> PyResult<Vec<String>> {
        let engine = self.build_engine()
            .map_err(|e| PyValueError::new_err(format!("Engine error: {e}")))?;

        if self.extra_layers.is_empty() && layer.is_none() {
            let subject_ids = engine.query_predicate(pred_alias, object_uri)
                .map_err(|e| PyValueError::new_err(format!("Query error: {e}")))?;
            Ok(self.dict_ids_to_resource_ids(&subject_ids))
        } else {
            let pred_full = ros_madair_core::uri::node_uri(&self.base_uri, pred_alias);
            let pattern = ros_madair_core::TriplePattern {
                subject: ros_madair_core::PatternTerm::Variable("s".into()),
                predicate: ros_madair_core::PatternTerm::Uri(pred_full),
                object: match object_uri {
                    Some(u) => ros_madair_core::PatternTerm::Uri(u.to_string()),
                    None => ros_madair_core::PatternTerm::Variable("o".into()),
                },
            };
            let uris = engine.query_patterns_multi(&[pattern], layer)
                .map_err(|e| PyValueError::new_err(format!("Query error: {e}")))?;
            Ok(self.uris_to_resource_ids(&uris))
        }
    }

    /// Multi-pattern query (compound filters, AND logic).
    ///
    /// `patterns_json` is a JSON array of objects, each with:
    /// - `pred_alias` (string): node alias
    /// - `object_uri` (string|null): optional exact object URI
    ///
    /// `layer` optionally restricts the search to a named layer.
    ///
    /// Returns resource IDs matching ALL patterns.
    #[pyo3(signature = (patterns_json, layer=None))]
    fn query_compound(&self, patterns_json: &str, layer: Option<&str>) -> PyResult<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct PatternInput {
            pred_alias: String,
            object_uri: Option<String>,
        }

        let inputs: Vec<PatternInput> = serde_json::from_str(patterns_json)
            .map_err(|e| PyValueError::new_err(format!("Invalid patterns JSON: {e}")))?;

        let patterns: Vec<ros_madair_core::TriplePattern> = inputs
            .iter()
            .map(|input| {
                let pred_full = ros_madair_core::uri::node_uri(&self.base_uri, &input.pred_alias);
                ros_madair_core::TriplePattern {
                    subject: ros_madair_core::PatternTerm::Variable("s".into()),
                    predicate: ros_madair_core::PatternTerm::Uri(pred_full),
                    object: match &input.object_uri {
                        Some(u) => ros_madair_core::PatternTerm::Uri(u.clone()),
                        None => ros_madair_core::PatternTerm::Variable("o".into()),
                    },
                }
            })
            .collect();

        let engine = self.build_engine()
            .map_err(|e| PyValueError::new_err(format!("Engine error: {e}")))?;

        if self.extra_layers.is_empty() && layer.is_none() {
            let subject_ids = engine.query_patterns(&patterns)
                .map_err(|e| PyValueError::new_err(format!("Query error: {e}")))?;
            Ok(self.dict_ids_to_resource_ids(&subject_ids))
        } else {
            let uris = engine.query_patterns_multi(&patterns, layer)
                .map_err(|e| PyValueError::new_err(format!("Query error: {e}")))?;
            Ok(self.uris_to_resource_ids(&uris))
        }
    }

    /// Return all page metadata as a JSON string.
    fn page_meta_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.page_meta)
            .map_err(|e| PyValueError::new_err(format!("Serialization error: {e}")))
    }

    // ------------------------------------------------------------------
    // Concept tree methods (label lookup + browsing)
    // ------------------------------------------------------------------

    /// Look up a concept value_id by collection and label (case-insensitive).
    fn lookup_label(&self, collection_id: &str, label: &str) -> Option<String> {
        self.concept_tree
            .as_ref()
            .and_then(|ct| ct.lookup_label(collection_id, label).map(String::from))
    }

    /// List concepts in a collection, optionally under a parent.
    ///
    /// Returns a JSON array of `{value_id, label, has_children}` objects.
    #[pyo3(signature = (collection_id, parent_value_id=None))]
    fn list_concepts(
        &self,
        collection_id: &str,
        parent_value_id: Option<&str>,
    ) -> PyResult<String> {
        let ct = match &self.concept_tree {
            Some(ct) => ct,
            None => return Ok("[]".to_string()),
        };

        let concepts = match parent_value_id {
            None => ct.list_top_level(collection_id),
            Some(pvid) => ct.list_children(collection_id, pvid),
        };

        serde_json::to_string(&concepts)
            .map_err(|e| PyValueError::new_err(format!("Serialization error: {e}")))
    }

    /// Return all collection IDs known to the concept tree.
    fn list_collection_ids(&self) -> Vec<String> {
        match &self.concept_tree {
            Some(ct) => ct.collection_ids().into_iter().map(String::from).collect(),
            None => Vec::new(),
        }
    }
}

impl IndexReader {
    /// Build a LocalQueryEngine from our loaded data.
    ///
    /// Includes all extra layers added via `add_layer()`. We clone the base
    /// layer's data structures — this is fine for one-shot queries.
    fn build_engine(&self) -> Result<LocalQueryEngine, String> {
        let mut engine = LocalQueryEngine::from_parts(
            self.dict.clone(),
            self.summary.clone(),
            self.resource_map.clone(),
            self.concept_intervals.clone(),
            self.page_meta.clone(),
            self.index_dir.join("pages"),
            self.base_uri.clone(),
        );
        for (dir, base_uri, name) in &self.extra_layers {
            engine.add_layer(dir, base_uri, name)?;
        }
        Ok(engine)
    }

    /// Convert dict IDs to bare resource IDs (UUID strings).
    fn dict_ids_to_resource_ids(&self, ids: &[u32]) -> Vec<String> {
        let prefix = ros_madair_core::uri::resource_prefix(&self.base_uri);
        ids.iter()
            .filter_map(|&id| {
                self.dict.resolve(id).and_then(|term| {
                    term.strip_prefix(&prefix).map(String::from)
                })
            })
            .collect()
    }

    /// Convert full URIs to bare resource IDs, trying all known layer prefixes.
    fn uris_to_resource_ids(&self, uris: &[String]) -> Vec<String> {
        let mut prefixes = vec![ros_madair_core::uri::resource_prefix(&self.base_uri)];
        for (_, base_uri, _) in &self.extra_layers {
            prefixes.push(ros_madair_core::uri::resource_prefix(base_uri));
        }
        uris.iter()
            .filter_map(|uri| {
                for prefix in &prefixes {
                    if let Some(rid) = uri.strip_prefix(prefix.as_str()) {
                        return Some(rid.to_string());
                    }
                }
                None
            })
            .collect()
    }
}

#[pymodule]
fn ros_madair(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<IndexBuilder>()?;
    m.add_class::<IndexReader>()?;
    Ok(())
}
