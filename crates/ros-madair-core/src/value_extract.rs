// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Shared JSON value extraction for Arches tile data.
//!
//! Arches stores tile values in varied JSON shapes depending on the datatype
//! and whether the value is single or multi-valued. These helpers normalise
//! all shapes into uniform Rust types.

use serde_json::Value;

/// Extract reference IDs (concept, resource-instance, domain-value) from a tile value.
///
/// Handles all JSON shapes found in Arches:
/// - plain string: `"uuid"`
/// - array of strings: `["uuid1", "uuid2"]`
/// - array of objects: `[{"resourceId": "uuid"}, {"id": "uuid"}, {"value": "uuid"}]`
/// - single object: `{"resourceId": "uuid"}` / `{"id": "uuid"}` / `{"value": "uuid"}`
pub fn extract_reference_ids(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) if !s.is_empty() => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| extract_id_from_object(v))
            })
            .collect(),
        Value::Object(_) => extract_id_from_object(value).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Extract a URL from a tile value.
///
/// Handles plain string `"https://..."` and object `{"url": "https://..."}`.
pub fn extract_url(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map.get("url").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

/// Extract localized strings from a tile value.
///
/// Returns `(language, text)` pairs. `language` is `None` for plain strings.
///
/// Handles:
/// - plain string: `"text"` → `[(None, "text")]`
/// - language map: `{"en": "text", "ga": "téacs"}` → `[(Some("en"), "text"), ...]`
/// - language map with direction: `{"en": {"value": "text", "direction": "ltr"}}` → same
pub fn extract_localized_strings(value: &Value) -> Vec<(Option<String>, String)> {
    match value {
        Value::String(s) => vec![(None, s.clone())],
        Value::Object(map) => {
            let mut result = Vec::new();
            for (lang, lang_val) in map {
                let text = match lang_val {
                    Value::String(s) => s.clone(),
                    Value::Object(inner) => match inner.get("value").and_then(|v| v.as_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    },
                    Value::Null => continue,
                    _ => continue,
                };
                if !text.is_empty() {
                    result.push((Some(lang.clone()), text));
                }
            }
            result
        }
        _ => Vec::new(),
    }
}

/// Try to extract an ID from a JSON object, checking common Arches key names.
fn extract_id_from_object(value: &Value) -> Option<String> {
    value
        .get("resourceId")
        .or_else(|| value.get("id"))
        .or_else(|| value.get("value"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- extract_reference_ids --

    #[test]
    fn ref_ids_plain_string() {
        assert_eq!(extract_reference_ids(&json!("abc-123")), vec!["abc-123"]);
    }

    #[test]
    fn ref_ids_empty_string() {
        assert!(extract_reference_ids(&json!("")).is_empty());
    }

    #[test]
    fn ref_ids_array_of_strings() {
        assert_eq!(
            extract_reference_ids(&json!(["a", "b"])),
            vec!["a", "b"],
        );
    }

    #[test]
    fn ref_ids_array_of_objects() {
        let val = json!([
            {"resourceId": "r1"},
            {"id": "r2"},
            {"value": "r3"},
        ]);
        assert_eq!(extract_reference_ids(&val), vec!["r1", "r2", "r3"]);
    }

    #[test]
    fn ref_ids_single_object() {
        assert_eq!(
            extract_reference_ids(&json!({"resourceId": "x"})),
            vec!["x"],
        );
    }

    #[test]
    fn ref_ids_null() {
        assert!(extract_reference_ids(&Value::Null).is_empty());
    }

    #[test]
    fn ref_ids_mixed_array() {
        let val = json!(["plain-id", {"id": "obj-id"}]);
        assert_eq!(extract_reference_ids(&val), vec!["plain-id", "obj-id"]);
    }

    // -- extract_url --

    #[test]
    fn url_plain_string() {
        assert_eq!(extract_url(&json!("https://x.org")), Some("https://x.org".into()));
    }

    #[test]
    fn url_object() {
        assert_eq!(
            extract_url(&json!({"url": "https://y.org", "label": "Y"})),
            Some("https://y.org".into()),
        );
    }

    #[test]
    fn url_null() {
        assert_eq!(extract_url(&Value::Null), None);
    }

    // -- extract_localized_strings --

    #[test]
    fn localized_plain_string() {
        assert_eq!(
            extract_localized_strings(&json!("hello")),
            vec![(None, "hello".into())],
        );
    }

    #[test]
    fn localized_language_map() {
        let val = json!({"en": "hello", "ga": "dia duit"});
        let mut result = extract_localized_strings(&val);
        result.sort_by_key(|(lang, _)| lang.clone());
        assert_eq!(result, vec![
            (Some("en".into()), "hello".into()),
            (Some("ga".into()), "dia duit".into()),
        ]);
    }

    #[test]
    fn localized_direction_objects() {
        let val = json!({"en": {"value": "hello", "direction": "ltr"}});
        assert_eq!(
            extract_localized_strings(&val),
            vec![(Some("en".into()), "hello".into())],
        );
    }

    #[test]
    fn localized_skips_null_and_empty() {
        let val = json!({"en": null, "ga": ""});
        assert!(extract_localized_strings(&val).is_empty());
    }
}
