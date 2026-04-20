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

use alizarin_core::graph::{StaticGraph, StaticTile};
use ros_madair_core::{
    assign_pages, quantize_type_for_datatype, quantize_tile_value,
    serialize_summary, write_page_file, write_tile_content_file,
    parse_tile_content_header,
    resource_to_triples, graph_schema_to_triples, triples_to_ntriples,
    Dictionary, PageConfig, PageRecord, PredicateBlock, ResourceMap, ResourceMeta,
    SummaryBuilder, extract_centroid,
    page_assignment::ResourceSummary,
};
use ros_madair_core::datatype_class::{classify_datatype, DatatypeClass};
use ros_madair_core::uri::{node_uri, resource_prefix, resource_uri};
use ros_madair_core::value_extract;

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

    /// Build the index and write output files.
    #[pyo3(signature = (output_dir, page_size=None))]
    fn build(&self, output_dir: &str, page_size: Option<usize>) -> PyResult<()>{
        let output = PathBuf::from(output_dir);
        fs::create_dir_all(output.join("pages"))
            .map_err(|e| PyValueError::new_err(format!("Failed to create output dir: {e}")))?;

        let config = PageConfig {
            target_page_size: page_size.unwrap_or(2000),
        };

        let mut dict = Dictionary::new();

        // Build resource summaries for page assignment
        let summaries: Vec<ResourceSummary> = self
            .resources
            .iter()
            .map(|r| {
                // Extract centroid from any geojson tile data
                let mut centroid = None;
                let mut concept_ids = Vec::new();

                if let Some(graph) = self.graphs.get(&r.graph_id) {
                    for tile in &r.tiles {
                        for (node_id, value) in &tile.data {
                            if let Some(node) = graph.get_node_by_id(node_id) {
                                match classify_datatype(&node.datatype) {
                                    Some(DatatypeClass::GeoJson) => {
                                        if centroid.is_none() {
                                            centroid = extract_centroid(&value.to_string());
                                        }
                                    }
                                    Some(DatatypeClass::Concept | DatatypeClass::DomainValue) => {
                                        concept_ids.extend(value_extract::extract_reference_ids(value));
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }

                ResourceSummary {
                    resource_id: r.resource_id.clone(),
                    graph_id: r.graph_id.clone(),
                    centroid,
                    concept_ids,
                }
            })
            .collect();

        // Assign pages
        let page_index = assign_pages(&summaries, &config);

        // Build page records and summary quads
        let mut summary_builder = SummaryBuilder::new();
        let mut page_records: HashMap<u32, Vec<(u32, PageRecord)>> = HashMap::new();
        // page_id → [(subject_id, msgpack_bytes)] for tile content files
        let mut tile_content: HashMap<u32, Vec<(u32, Vec<u8>)>> = HashMap::new();

        for resource_data in &self.resources {
            let graph = match self.graphs.get(&resource_data.graph_id) {
                Some(g) => g,
                None => continue,
            };

            let page_id = match page_index.resource_to_page.get(&resource_data.resource_id) {
                Some(&p) => p,
                None => continue,
            };

            let subject_id = dict.intern(&resource_uri(
                &self.base_uri, &resource_data.resource_id
            ));

            // Serialize full tile data for the tile content index
            if let Ok(tile_bytes) = rmp_serde::to_vec_named(&resource_data.tiles) {
                if !tile_bytes.is_empty() {
                    tile_content
                        .entry(page_id)
                        .or_default()
                        .push((subject_id, tile_bytes));
                }
            }

            // Process each tile's data for indexed types
            for tile in &resource_data.tiles {
                for (node_id, value) in &tile.data {
                    if value.is_null() {
                        continue;
                    }

                    let node = match graph.get_node_by_id(node_id) {
                        Some(n) => n,
                        None => continue,
                    };

                    let alias = match &node.alias {
                        Some(a) if !a.is_empty() => a.as_str(),
                        _ => continue,
                    };

                    let qtype = match quantize_type_for_datatype(&node.datatype) {
                        Some(qt) => qt,
                        None => continue, // not an indexed type
                    };

                    let pred_id = dict.intern(&node_uri(&self.base_uri, alias));

                    let object_vals = quantize_tile_value(
                        value,
                        qtype,
                        &node.datatype,
                        &mut dict,
                        &self.base_uri,
                    );

                    for object_val in object_vals {
                        let record = PageRecord {
                            subject_id,
                            object_val,
                        };

                        page_records
                            .entry(page_id)
                            .or_default()
                            .push((pred_id, record));

                        // For link types, page_o = page of the target resource.
                        // For literal types, page_o = the quantized value (bucket).
                        // Only resource-instance links resolve to page IDs.
                        // Concepts use a high sentinel to avoid page ID collisions.
                        const NON_PAGE_SENTINEL: u32 = u32::MAX;
                        let page_o = match qtype {
                            ros_madair_core::QuantizeType::DictionaryId => {
                                if let Some(term) = dict.resolve(object_val) {
                                    if let Some(rid) = term.strip_prefix(&resource_prefix(&self.base_uri)) {
                                        page_index
                                            .resource_to_page
                                            .get(rid)
                                            .copied()
                                            .unwrap_or(NON_PAGE_SENTINEL)
                                    } else {
                                        NON_PAGE_SENTINEL
                                    }
                                } else {
                                    NON_PAGE_SENTINEL
                                }
                            }
                            _ => object_val, // literal bucket
                        };

                        summary_builder.add(page_id, pred_id, page_o, subject_id);

                        // Emit reverse record on the target's page for resource-instance links
                        if page_o != NON_PAGE_SENTINEL && qtype == ros_madair_core::QuantizeType::DictionaryId {
                            let reverse_pred_id = dict.intern(&format!("!{}", node_uri(&self.base_uri, alias)));
                            let reverse_record = PageRecord {
                                subject_id: object_val,
                                object_val: subject_id,
                            };
                            page_records
                                .entry(page_o)
                                .or_default()
                                .push((reverse_pred_id, reverse_record));
                            summary_builder.add(page_o, reverse_pred_id, page_id, object_val);
                        }
                    }
                }
            }
        }

        // Write summary quads
        let summary_quads = summary_builder.build();
        let summary_bytes = serialize_summary(&summary_quads);
        fs::write(output.join("summary.bin"), &summary_bytes)
            .map_err(|e| PyValueError::new_err(format!("Failed to write summary: {e}")))?;

        // Write dictionary
        let dict_bytes = dict.to_bytes();
        fs::write(output.join("dictionary.bin"), &dict_bytes)
            .map_err(|e| PyValueError::new_err(format!("Failed to write dictionary: {e}")))?;

        // Write resource map (dict_id → page_id for resource terms)
        let resource_map = ResourceMap::build(&dict, &page_index.resource_to_page, &self.base_uri);
        let resource_map_bytes = resource_map.to_bytes();
        fs::write(output.join("resource_map.bin"), &resource_map_bytes)
            .map_err(|e| PyValueError::new_err(format!("Failed to write resource_map: {e}")))?;

        // Write page metadata
        let page_meta_json = serde_json::to_string_pretty(&page_index.page_meta)
            .map_err(|e| PyValueError::new_err(format!("Failed to serialize page meta: {e}")))?;
        fs::write(output.join("page_meta.json"), &page_meta_json)
            .map_err(|e| PyValueError::new_err(format!("Failed to write page meta: {e}")))?;

        // Build per-page resource metadata
        let mut page_resource_meta: HashMap<u32, Vec<ResourceMeta>> = HashMap::new();
        if !self.resource_metadata.is_empty() {
            for resource_data in &self.resources {
                let page_id = match page_index.resource_to_page.get(&resource_data.resource_id) {
                    Some(&p) => p,
                    None => continue,
                };
                let dict_id = match dict.lookup(&resource_uri(
                    &self.base_uri, &resource_data.resource_id
                )) {
                    Some(id) => id,
                    None => continue,
                };
                if let Some((name, slug, model)) = self.resource_metadata.get(&resource_data.resource_id) {
                    page_resource_meta.entry(page_id).or_default().push(ResourceMeta {
                        dict_id,
                        name: name.clone(),
                        slug: slug.clone(),
                        model: model.clone(),
                    });
                }
            }
        }
        let has_meta = !page_resource_meta.is_empty();

        // Write page files
        for pm in &page_index.page_meta {
            if let Some(records) = page_records.get(&pm.page_id) {
                // Group by predicate
                let mut by_pred: HashMap<u32, Vec<PageRecord>> = HashMap::new();
                for &(pred_id, record) in records {
                    by_pred.entry(pred_id).or_default().push(record);
                }

                let mut blocks: Vec<PredicateBlock> = by_pred
                    .into_iter()
                    .map(|(pred_id, mut recs)| {
                        recs.sort(); // sort by (object_val, subject_id)
                        PredicateBlock {
                            pred_id,
                            records: recs,
                        }
                    })
                    .collect();

                let meta = if has_meta {
                    page_resource_meta.get(&pm.page_id)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[])
                } else {
                    &[]
                };
                let page_bytes = write_page_file(&mut blocks, meta);
                let filename = format!("page_{:04}.dat", pm.page_id);
                fs::write(output.join("pages").join(&filename), &page_bytes)
                    .map_err(|e| {
                        PyValueError::new_err(format!("Failed to write {filename}: {e}"))
                    })?;
            }
        }

        // Write tile content files
        let tiles_dir = output.join("tiles");
        fs::create_dir_all(&tiles_dir)
            .map_err(|e| PyValueError::new_err(format!("Failed to create tiles dir: {e}")))?;

        for pm in &page_index.page_meta {
            if let Some(mut entries) = tile_content.remove(&pm.page_id) {
                entries.sort_by_key(|(sid, _)| *sid);
                let tile_bytes = write_tile_content_file(&entries);
                let filename = format!("tile_{:04}.dat", pm.page_id);
                fs::write(tiles_dir.join(&filename), &tile_bytes)
                    .map_err(|e| PyValueError::new_err(format!("Failed to write {filename}: {e}")))?;
            }
        }

        // Write RDF export (full N-Triples for oxigraph verification)
        let mut all_triples = Vec::new();
        for graph in self.graphs.values() {
            if let Ok(schema_triples) = graph_schema_to_triples(graph, &self.base_uri) {
                all_triples.extend(schema_triples);
            }
        }
        for resource_data in &self.resources {
            if let Some(graph) = self.graphs.get(&resource_data.graph_id) {
                if let Ok(triples) = resource_to_triples(
                    graph,
                    &resource_data.resource_id,
                    &resource_data.tiles,
                    &self.base_uri,
                ) {
                    all_triples.extend(triples);
                }
            }
        }
        let ntriples = triples_to_ntriples(&all_triples);
        fs::write(output.join("all.nt"), &ntriples)
            .map_err(|e| PyValueError::new_err(format!("Failed to write N-Triples: {e}")))?;

        Ok(())
    }
}

// =============================================================================
// Index Reader — exposes ros-madair-core binary parsers to Python
// =============================================================================

/// Read-side counterpart to IndexBuilder.
///
/// Loads dictionary, resource_map, and tile content files from a built index
/// directory using the same Rust parsing code as the WASM client.
#[pyclass]
pub struct IndexReader {
    base_uri: String,
    dict: Dictionary,
    resource_map: ResourceMap,
    index_dir: PathBuf,
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

        Ok(Self {
            base_uri: base_uri.to_string(),
            dict,
            resource_map,
            index_dir,
        })
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
}

#[pymodule]
fn ros_madair(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<IndexBuilder>()?;
    m.add_class::<IndexReader>()?;
    Ok(())
}
