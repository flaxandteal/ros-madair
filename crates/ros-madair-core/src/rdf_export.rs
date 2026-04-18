// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Convert alizarin-core graph/tile Rust types to RDF triples.
//!
//! Adapted from madder-core's rdf_export.rs to accept `&StaticGraph` and
//! `&[StaticTile]` directly instead of JSON strings, eliminating
//! serialization overhead.

use alizarin_core::graph::{StaticGraph, StaticTile};
use serde_json::Value;
use std::fmt;
use thiserror::Error;

use crate::datatype_class::{classify_datatype, DatatypeClass};
use crate::geo_convert;
use crate::uri::{collection_uri, concept_uri, graph_uri, node_uri, resource_uri, resource_prefix};
use crate::value_extract;

// Well-known namespace prefixes
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
const OWL_CLASS: &str = "http://www.w3.org/2002/07/owl#Class";
const OWL_DATATYPE_PROPERTY: &str = "http://www.w3.org/2002/07/owl#DatatypeProperty";
const OWL_OBJECT_PROPERTY: &str = "http://www.w3.org/2002/07/owl#ObjectProperty";
const GEO_AS_WKT: &str = "http://www.opengis.net/ont/geosparql#asWKT";
const GEO_WKT_LITERAL: &str = "http://www.opengis.net/ont/geosparql#wktLiteral";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";
const SKOS_PREF_LABEL: &str = "http://www.w3.org/2004/02/skos/core#prefLabel";
const SKOS_BROADER: &str = "http://www.w3.org/2004/02/skos/core#broader";
const SKOS_NARROWER: &str = "http://www.w3.org/2004/02/skos/core#narrower";
const SKOS_IN_SCHEME: &str = "http://www.w3.org/2004/02/skos/core#inScheme";
const SKOS_HAS_TOP_CONCEPT: &str = "http://www.w3.org/2004/02/skos/core#hasTopConcept";
const SKOS_CONCEPT: &str = "http://www.w3.org/2004/02/skos/core#Concept";
const SKOS_CONCEPT_SCHEME: &str = "http://www.w3.org/2004/02/skos/core#ConceptScheme";
const SKOS_ALT_LABEL: &str = "http://www.w3.org/2004/02/skos/core#altLabel";
const SKOS_SCOPE_NOTE: &str = "http://www.w3.org/2004/02/skos/core#scopeNote";

#[derive(Error, Debug)]
pub enum TripleError {
    #[error("JSON parse error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Missing required field: {0}")]
    MissingField(String),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("GeoJSON conversion error: {0}")]
    GeoError(String),
}

/// An RDF triple (subject, predicate, object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Triple {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
}

/// An RDF term — either a named node (URI) or a literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Term {
    NamedNode(String),
    Literal {
        value: String,
        datatype: Option<String>,
        language: Option<String>,
    },
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Term::NamedNode(uri) => write!(f, "<{}>", uri),
            Term::Literal {
                value,
                datatype,
                language,
            } => {
                let escaped = value
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r");
                if let Some(lang) = language {
                    write!(f, "\"{}\"@{}", escaped, lang)
                } else if let Some(dt) = datatype {
                    write!(f, "\"{}\"^^<{}>", escaped, dt)
                } else {
                    write!(f, "\"{}\"", escaped)
                }
            }
        }
    }
}

impl Triple {
    pub fn new(subject: Term, predicate: Term, object: Term) -> Self {
        Self {
            subject,
            predicate,
            object,
        }
    }

    /// Format as N-Triples line.
    pub fn to_ntriples(&self) -> String {
        format!("{} {} {} .", self.subject, self.predicate, self.object)
    }
}

fn uri(s: &str) -> Term {
    Term::NamedNode(s.to_string())
}

fn literal(value: &str, datatype: &str) -> Term {
    Term::Literal {
        value: value.to_string(),
        datatype: Some(datatype.to_string()),
        language: None,
    }
}

fn lang_literal(value: &str, lang: &str) -> Term {
    Term::Literal {
        value: value.to_string(),
        datatype: None,
        language: Some(lang.to_string()),
    }
}

fn string_literal(value: &str) -> Term {
    literal(value, XSD_STRING)
}

// ============================================================================
// Graph Schema → RDF (RDFS/OWL class + property declarations)
// ============================================================================

/// Convert a StaticGraph schema to RDF triples (RDFS/OWL declarations).
pub fn graph_schema_to_triples(
    graph: &StaticGraph,
    base_uri: &str,
) -> Result<Vec<Triple>, TripleError> {
    let class_uri = graph_uri(base_uri, &graph.graphid);
    let mut triples = Vec::new();

    // Declare the graph as an OWL class
    triples.push(Triple::new(uri(&class_uri), uri(RDF_TYPE), uri(OWL_CLASS)));

    // Add label from name
    let label = graph.name.get("en");
    if !label.is_empty() {
        triples.push(Triple::new(
            uri(&class_uri),
            uri(RDFS_LABEL),
            lang_literal(&label, "en"),
        ));
    }

    // Process nodes → property declarations
    for node in &graph.nodes {
        let alias = match &node.alias {
            Some(a) if !a.is_empty() => a.as_str(),
            _ => continue,
        };

        if classify_datatype(&node.datatype) == Some(DatatypeClass::Semantic) {
            continue;
        }

        let prop_uri = node_uri(base_uri, alias);

        let prop_type = if classify_datatype(&node.datatype)
            .is_some_and(|c| c.is_object_property())
        {
            OWL_OBJECT_PROPERTY
        } else {
            OWL_DATATYPE_PROPERTY
        };

        triples.push(Triple::new(uri(&prop_uri), uri(RDF_TYPE), uri(prop_type)));

        triples.push(Triple::new(
            uri(&prop_uri),
            uri(RDFS_LABEL),
            lang_literal(&node.name, "en"),
        ));

        triples.push(Triple::new(
            uri(&prop_uri),
            uri(RDFS_DOMAIN),
            uri(&class_uri),
        ));

        if let Some(range_uri) = datatype_to_range(&node.datatype, base_uri) {
            triples.push(Triple::new(
                uri(&prop_uri),
                uri(RDFS_RANGE),
                uri(&range_uri),
            ));
        }
    }

    Ok(triples)
}

// ============================================================================
// Resource Tiles → RDF (instance data)
// ============================================================================

/// Convert a single resource's tiles to RDF triples.
///
/// Accepts alizarin-core Rust types directly — no JSON parsing needed.
/// The graph must have had `build_indices()` called (for `get_node_by_id`).
pub fn resource_to_triples(
    graph: &StaticGraph,
    resource_id: &str,
    tiles: &[StaticTile],
    base_uri: &str,
) -> Result<Vec<Triple>, TripleError> {
    let subject = uri(&resource_uri(base_uri, resource_id));
    let mut triples = Vec::new();

    // rdf:type
    triples.push(Triple::new(
        subject.clone(),
        uri(RDF_TYPE),
        uri(&graph_uri(base_uri, &graph.graphid)),
    ));

    for tile in tiles {
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

            let dc = classify_datatype(&node.datatype);
            if dc == Some(DatatypeClass::Semantic) {
                continue;
            }

            let predicate = uri(&node_uri(base_uri, alias));

            match dc {
                Some(DatatypeClass::LocalizedString) => {
                    for (lang, text) in value_extract::extract_localized_strings(value) {
                        let obj = match lang {
                            Some(l) => lang_literal(&text, &l),
                            None => string_literal(&text),
                        };
                        triples.push(Triple::new(subject.clone(), predicate.clone(), obj));
                    }
                }
                Some(DatatypeClass::PlainString) => {
                    if let Some(s) = value.as_str() {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            string_literal(s),
                        ));
                    }
                }
                Some(DatatypeClass::Number) => {
                    emit_number_triple(&subject, &predicate, value, &mut triples);
                }
                Some(DatatypeClass::Date) => {
                    if let Some(s) = value.as_str() {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            literal(s, XSD_DATE),
                        ));
                    }
                }
                Some(DatatypeClass::Boolean) => {
                    if let Some(b) = value.as_bool() {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            literal(&b.to_string(), XSD_BOOLEAN),
                        ));
                    }
                }
                Some(DatatypeClass::Url) => {
                    if let Some(url_val) = value_extract::extract_url(value) {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            uri(&url_val),
                        ));
                    }
                }
                Some(DatatypeClass::Concept) => {
                    for cid in value_extract::extract_reference_ids(value) {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            uri(&concept_uri(base_uri, &cid)),
                        ));
                    }
                }
                Some(DatatypeClass::ResourceInstance) => {
                    for rid in value_extract::extract_reference_ids(value) {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            uri(&resource_uri(base_uri, &rid)),
                        ));
                    }
                }
                Some(DatatypeClass::GeoJson) => {
                    match geo_convert::geojson_to_wkt(&value.to_string()) {
                        Ok(wkt_str) => {
                            triples.push(Triple::new(
                                subject.clone(),
                                uri(GEO_AS_WKT),
                                literal(&wkt_str, GEO_WKT_LITERAL),
                            ));
                        }
                        Err(e) => {
                            eprintln!(
                                "Warning: GeoJSON conversion failed for resource {}: {}",
                                resource_id, e
                            );
                        }
                    }
                }
                Some(DatatypeClass::DomainValue) => {
                    if let Some(s) = value.as_str() {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            string_literal(s),
                        ));
                    } else if let Some(arr) = value.as_array() {
                        for v in arr {
                            if let Some(s) = v.as_str() {
                                triples.push(Triple::new(
                                    subject.clone(),
                                    predicate.clone(),
                                    string_literal(s),
                                ));
                            }
                        }
                    }
                }
                Some(DatatypeClass::Semantic) => unreachable!(),
                None => {
                    if let Some(s) = value.as_str() {
                        triples.push(Triple::new(
                            subject.clone(),
                            predicate.clone(),
                            string_literal(s),
                        ));
                    }
                }
            }
        }
    }

    Ok(triples)
}

// ============================================================================
// Collection → RDF (SKOS concept scheme)
// ============================================================================

/// Convert an RDM collection JSON to SKOS triples.
pub fn collection_to_triples(
    collection_id: &str,
    concepts_json: &str,
    base_uri: &str,
) -> Result<Vec<Triple>, TripleError> {
    let concepts: Vec<Value> = serde_json::from_str(concepts_json)?;
    let scheme_uri = collection_uri(base_uri, collection_id);
    let mut triples = Vec::new();

    triples.push(Triple::new(
        uri(&scheme_uri),
        uri(RDF_TYPE),
        uri(SKOS_CONCEPT_SCHEME),
    ));

    for concept in &concepts {
        let concept_id = concept
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TripleError::MissingField("concept id".into()))?;

        let c_uri = concept_uri(base_uri, concept_id);

        triples.push(Triple::new(uri(&c_uri), uri(RDF_TYPE), uri(SKOS_CONCEPT)));
        triples.push(Triple::new(
            uri(&c_uri),
            uri(SKOS_IN_SCHEME),
            uri(&scheme_uri),
        ));

        // prefLabel
        let pref_label_key = if concept.get("prefLabels").is_some() {
            "prefLabels"
        } else {
            "prefLabel"
        };
        if let Some(labels) = concept.get(pref_label_key) {
            for (lang, text) in value_extract::extract_localized_strings(labels) {
                let obj = match lang {
                    Some(l) => lang_literal(&text, &l),
                    None => string_literal(&text),
                };
                triples.push(Triple::new(uri(&c_uri), uri(SKOS_PREF_LABEL), obj));
            }
        }

        // altLabels
        if let Some(alt_labels) = concept.get("altLabels").and_then(|v| v.as_object()) {
            for (lang, labels_val) in alt_labels {
                if let Some(arr) = labels_val.as_array() {
                    for label in arr {
                        if let Some(s) = label.as_str() {
                            triples.push(Triple::new(
                                uri(&c_uri),
                                uri(SKOS_ALT_LABEL),
                                lang_literal(s, lang),
                            ));
                        }
                    }
                }
            }
        }

        // scopeNote
        if let Some(scope_notes) = concept.get("scopeNote").and_then(|v| v.as_object()) {
            for (lang, note) in scope_notes {
                if let Some(s) = note.as_str() {
                    triples.push(Triple::new(
                        uri(&c_uri),
                        uri(SKOS_SCOPE_NOTE),
                        lang_literal(s, lang),
                    ));
                }
            }
        }

        // broader
        if let Some(broader) = concept.get("broader").and_then(|v| v.as_array()) {
            for parent in broader {
                if let Some(pid) = parent.as_str() {
                    triples.push(Triple::new(
                        uri(&c_uri),
                        uri(SKOS_BROADER),
                        uri(&concept_uri(base_uri, pid)),
                    ));
                }
            }
        }

        // narrower
        if let Some(narrower) = concept.get("narrower").and_then(|v| v.as_array()) {
            for child in narrower {
                if let Some(cid) = child.as_str() {
                    triples.push(Triple::new(
                        uri(&c_uri),
                        uri(SKOS_NARROWER),
                        uri(&concept_uri(base_uri, cid)),
                    ));
                }
            }
        }

        // hasTopConcept — if no broader
        let has_broader = concept
            .get("broader")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        if !has_broader {
            triples.push(Triple::new(
                uri(&scheme_uri),
                uri(SKOS_HAS_TOP_CONCEPT),
                uri(&c_uri),
            ));
        }
    }

    Ok(triples)
}

// ============================================================================
// Helpers
// ============================================================================

fn datatype_to_range(datatype: &str, base_uri: &str) -> Option<String> {
    match classify_datatype(datatype)? {
        DatatypeClass::LocalizedString | DatatypeClass::PlainString | DatatypeClass::Url => {
            Some(XSD_STRING.to_string())
        }
        DatatypeClass::Number => Some(XSD_DECIMAL.to_string()),
        DatatypeClass::Date => Some(XSD_DATE.to_string()),
        DatatypeClass::Boolean => Some(XSD_BOOLEAN.to_string()),
        DatatypeClass::GeoJson => Some(GEO_WKT_LITERAL.to_string()),
        DatatypeClass::Concept => Some(SKOS_CONCEPT.to_string()),
        DatatypeClass::ResourceInstance => Some(resource_prefix(base_uri)),
        DatatypeClass::DomainValue | DatatypeClass::Semantic => None,
    }
}

fn emit_number_triple(
    subject: &Term,
    predicate: &Term,
    value: &Value,
    triples: &mut Vec<Triple>,
) {
    match value {
        Value::Number(n) => {
            let (val_str, dt) = if n.is_i64() || n.is_u64() {
                (n.to_string(), XSD_INTEGER)
            } else {
                (n.to_string(), XSD_DECIMAL)
            };
            triples.push(Triple::new(
                subject.clone(),
                predicate.clone(),
                literal(&val_str, dt),
            ));
        }
        Value::String(s) => {
            triples.push(Triple::new(
                subject.clone(),
                predicate.clone(),
                literal(s, XSD_DECIMAL),
            ));
        }
        _ => {}
    }
}

/// Write a collection of triples as N-Triples to a string.
pub fn triples_to_ntriples(triples: &[Triple]) -> String {
    triples
        .iter()
        .map(|t| t.to_ntriples())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alizarin_core::graph::{StaticGraph, StaticTile};
    use std::collections::HashMap;

    fn make_test_graph() -> StaticGraph {
        let json = r#"{
            "graphid": "graph-001",
            "name": {"en": "Heritage Place"},
            "nodes": [
                {"nodeid": "node-1", "alias": "name_value", "datatype": "string", "name": "Name", "graph_id": "graph-001", "nodegroup_id": "ng-1", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": false, "issearchable": true, "istopnode": false},
                {"nodeid": "node-2", "alias": "year_built", "datatype": "number", "name": "Year Built", "graph_id": "graph-001", "nodegroup_id": "ng-2", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": false, "issearchable": true, "istopnode": false}
            ],
            "edges": [],
            "root": {"nodeid": "root", "name": "Heritage Place", "datatype": "semantic", "graph_id": "graph-001", "is_collector": false, "isrequired": false, "exportable": true, "config": {}, "hascustomalias": false, "issearchable": true, "istopnode": true},
            "nodegroups": []
        }"#;
        let mut graph: StaticGraph = serde_json::from_str(json).unwrap();
        graph.build_indices();
        graph
    }

    fn make_test_tiles() -> Vec<StaticTile> {
        let mut data1 = HashMap::new();
        data1.insert(
            "node-1".to_string(),
            serde_json::json!({"en": "Belfast Castle"}),
        );

        let mut data2 = HashMap::new();
        data2.insert("node-2".to_string(), serde_json::json!(1870));

        vec![
            StaticTile {
                data: data1,
                nodegroup_id: "ng-1".to_string(),
                resourceinstance_id: "res-001".to_string(),
                tileid: None,
                parenttile_id: None,
                provisionaledits: None,
                sortorder: None,
            },
            StaticTile {
                data: data2,
                nodegroup_id: "ng-2".to_string(),
                resourceinstance_id: "res-001".to_string(),
                tileid: None,
                parenttile_id: None,
                provisionaledits: None,
                sortorder: None,
            },
        ]
    }

    #[test]
    fn test_resource_to_triples_basic() {
        let graph = make_test_graph();
        let tiles = make_test_tiles();
        let base = "https://example.org/";

        let triples = resource_to_triples(&graph, "res-001", &tiles, base).unwrap();
        assert!(triples.len() >= 3, "Expected at least 3 triples, got {}", triples.len());

        // Check rdf:type
        assert!(triples.iter().any(
            |t| matches!(&t.predicate, Term::NamedNode(u) if u == RDF_TYPE)
                && matches!(&t.object, Term::NamedNode(u) if u == "https://example.org/graph/graph-001")
        ));

        // Check name value
        assert!(triples.iter().any(
            |t| matches!(&t.predicate, Term::NamedNode(u) if u.ends_with("node/name_value"))
        ));

        // Check number value
        assert!(triples.iter().any(
            |t| matches!(&t.predicate, Term::NamedNode(u) if u.ends_with("node/year_built"))
        ));
    }

    #[test]
    fn test_ntriples_output() {
        let triple = Triple::new(
            uri("http://example.org/s"),
            uri("http://example.org/p"),
            lang_literal("hello \"world\"", "en"),
        );
        assert_eq!(
            triple.to_ntriples(),
            "<http://example.org/s> <http://example.org/p> \"hello \\\"world\\\"\"@en ."
        );
    }
}
