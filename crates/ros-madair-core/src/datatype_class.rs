// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Shared Arches datatype classification.
//!
//! Arches uses string datatype identifiers (e.g. `"concept"`, `"string"`,
//! `"geojson-feature-collection"`). This module provides a single source
//! of truth for classifying them, replacing scattered match blocks.

/// High-level classification of an Arches datatype.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatatypeClass {
    LocalizedString,
    PlainString,
    Number,
    Date,
    Boolean,
    Url,
    Concept,
    ResourceInstance,
    DomainValue,
    GeoJson,
    Semantic,
}

/// Classify an Arches datatype string into a `DatatypeClass`.
///
/// Returns `None` for unknown datatypes.
pub fn classify_datatype(datatype: &str) -> Option<DatatypeClass> {
    match datatype {
        "string" => Some(DatatypeClass::LocalizedString),
        "non-localized-string" => Some(DatatypeClass::PlainString),
        "number" => Some(DatatypeClass::Number),
        "date" | "edtf" => Some(DatatypeClass::Date),
        "boolean" => Some(DatatypeClass::Boolean),
        "url" => Some(DatatypeClass::Url),
        "concept" | "concept-value" | "concept-list" => Some(DatatypeClass::Concept),
        "resource-instance" | "resource-instance-list" => Some(DatatypeClass::ResourceInstance),
        "domain-value" | "domain-value-list" => Some(DatatypeClass::DomainValue),
        "geojson-feature-collection" => Some(DatatypeClass::GeoJson),
        "semantic" => Some(DatatypeClass::Semantic),
        _ => None,
    }
}

impl DatatypeClass {
    /// Whether this datatype represents an OWL ObjectProperty (links to another entity)
    /// as opposed to a DatatypeProperty (literal value).
    pub fn is_object_property(&self) -> bool {
        matches!(self, DatatypeClass::Concept | DatatypeClass::ResourceInstance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_all_known_types() {
        let cases = [
            ("string", DatatypeClass::LocalizedString),
            ("non-localized-string", DatatypeClass::PlainString),
            ("number", DatatypeClass::Number),
            ("date", DatatypeClass::Date),
            ("edtf", DatatypeClass::Date),
            ("boolean", DatatypeClass::Boolean),
            ("url", DatatypeClass::Url),
            ("concept", DatatypeClass::Concept),
            ("concept-value", DatatypeClass::Concept),
            ("concept-list", DatatypeClass::Concept),
            ("resource-instance", DatatypeClass::ResourceInstance),
            ("resource-instance-list", DatatypeClass::ResourceInstance),
            ("domain-value", DatatypeClass::DomainValue),
            ("domain-value-list", DatatypeClass::DomainValue),
            ("geojson-feature-collection", DatatypeClass::GeoJson),
            ("semantic", DatatypeClass::Semantic),
        ];
        for (input, expected) in cases {
            assert_eq!(classify_datatype(input), Some(expected), "failed for {input}");
        }
    }

    #[test]
    fn classify_unknown_returns_none() {
        assert_eq!(classify_datatype("file-list"), None);
        assert_eq!(classify_datatype(""), None);
    }

    #[test]
    fn object_property_flags() {
        assert!(DatatypeClass::Concept.is_object_property());
        assert!(DatatypeClass::ResourceInstance.is_object_property());
        assert!(!DatatypeClass::Number.is_object_property());
        assert!(!DatatypeClass::DomainValue.is_object_property());
        assert!(!DatatypeClass::GeoJson.is_object_property());
    }
}
