// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! # RosMadair Core
//!
//! Core library for the RosMadair page-based static SPARQL query engine.
//!
//! Converts alizarin-style graph/tile data into a two-level index:
//! - **Summary quads** (~1-3MB, loaded at init): page-level routing index
//! - **Page files** (~10-200KB each, fetched on demand): fixed-width quantized
//!   records sorted by (predicate, object_val, subject_id)
//!
//! ## Modules
//!
//! - [`rdf_export`] — alizarin StaticGraph/StaticTile → RDF triples
//! - [`dictionary`] — URI/literal ↔ u32 bidirectional encoding
//! - [`quantize`] — fixed-width value quantization (8 bytes per record)
//! - [`page_assignment`] — Hilbert-based resource → page mapping
//! - [`page_file`] — per-page binary file format with predicate-partitioned records
//! - [`summary_quads`] — page-level quad index for query planning
//! - [`tile_content_file`] — per-page tile content file for full-fidelity tile serving
//! - [`hilbert`] — 3D Hilbert curve for page sub-sorting
//! - [`geo_convert`] — GeoJSON → WKT + centroid extraction
//! - [`datatype_class`] — shared Arches datatype classification
//! - [`uri`] — shared URI construction helpers
//! - [`value_extract`] — shared JSON value extraction

pub mod datatype_class;
pub mod dictionary;
pub mod geo_convert;
pub mod hilbert;
pub mod page_assignment;
pub mod page_file;
pub mod quantize;
pub mod rdf_export;
pub mod resource_map;
pub mod summary_quads;
pub mod tile_content_file;
pub mod tile_source_impl;
pub mod uri;
pub mod value_extract;

pub use dictionary::Dictionary;
pub use geo_convert::{extract_centroid, geojson_to_wkt};
pub use page_assignment::{assign_pages, PageAssignment, PageConfig, PageIndex, PageMeta};
pub use page_file::{
    binary_search_object, full_header_size, parse_page_header, parse_records,
    parse_resource_meta, range_search_object, serialize_resource_meta, write_page_file,
    PageHeader, PredicateBlock, PredicateEntry, ResourceMeta,
};
pub use quantize::{
    dequantize_date, dequantize_point, quantize_bbox, quantize_bool, quantize_date,
    quantize_dictionary_id, quantize_point, quantize_tile_value, quantize_type_for_datatype,
    hilbert_ranges_for_geo_bbox, PageRecord, QuantizeType,
};
pub use rdf_export::{
    collection_to_triples, graph_schema_to_triples, resource_to_triples, triples_to_ntriples,
    Term, Triple, TripleError,
};
pub use resource_map::ResourceMap;
pub use summary_quads::{
    serialize_summary, SummaryBuilder, SummaryIndex, SummaryQuad,
};
pub use tile_content_file::{
    parse_tile_content_header, tile_full_header_size, write_tile_content_file,
    TileContentEntry, TileContentHeader,
};
pub use tile_source_impl::{DiskTileSource, GrowableTileSource, InMemoryTileSource};
