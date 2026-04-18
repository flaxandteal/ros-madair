// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Shared URI construction for RDF namespace terms.
//!
//! All Arches-derived URIs follow the pattern `{base_uri}{type}/{id}`.
//! These helpers centralise the formatting so that callers don't
//! repeat `format!("{}resource/{}", ...)` everywhere.

/// Construct a resource instance URI.
pub fn resource_uri(base_uri: &str, resource_id: &str) -> String {
    format!("{}resource/{}", base_uri, resource_id)
}

/// Construct a concept URI.
pub fn concept_uri(base_uri: &str, concept_id: &str) -> String {
    format!("{}concept/{}", base_uri, concept_id)
}

/// Construct a node (property) URI from an alias.
pub fn node_uri(base_uri: &str, alias: &str) -> String {
    format!("{}node/{}", base_uri, alias)
}

/// Construct a graph (class) URI.
pub fn graph_uri(base_uri: &str, graph_id: &str) -> String {
    format!("{}graph/{}", base_uri, graph_id)
}

/// Construct a collection (concept scheme) URI.
pub fn collection_uri(base_uri: &str, collection_id: &str) -> String {
    format!("{}collection/{}", base_uri, collection_id)
}

/// The `{base_uri}resource/` prefix, for `strip_prefix` operations.
pub fn resource_prefix(base_uri: &str) -> String {
    format!("{}resource/", base_uri)
}

/// The `{base_uri}concept/` prefix, for `strip_prefix` operations.
pub fn concept_prefix(base_uri: &str) -> String {
    format!("{}concept/", base_uri)
}

/// Return the appropriate URI prefix for interning dictionary values of
/// the given datatype. Uses [`classify_datatype`] to avoid duplicating
/// the string-to-class mapping.
pub fn prefix_for_datatype(base_uri: &str, datatype: &str) -> String {
    use crate::datatype_class::{classify_datatype, DatatypeClass};
    match classify_datatype(datatype) {
        Some(DatatypeClass::ResourceInstance) => resource_prefix(base_uri),
        Some(DatatypeClass::Concept) => concept_prefix(base_uri),
        _ => format!("{}value/", base_uri),
    }
}
