// SPDX-License-Identifier: AGPL-3.0-or-later
//! Concept closure (first-class artifact): SKOS loading/normalization,
//! the `Closure`/`ClosureEntry` types, and the DFS walk that builds the
//! closure from parsed collections.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use alizarin_core::skos::{SkosCollection, SkosConcept};
use alizarin_core::{load_collections_from_dir, PrebuildLoader};
use serde::Serialize;

use crate::EmitError;

#[derive(Serialize, Default)]
pub struct Closure {
    /// concept id -> entry
    pub concepts: BTreeMap<String, ClosureEntry>,
    /// pref-label value id -> concept id (tiles store value ids)
    pub value_map: BTreeMap<String, String>,
}

#[derive(Serialize)]
pub struct ClosureEntry {
    pub label: String,
    pub collection_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// ancestors including self, root-first
    pub ancestors: Vec<String>,
}

pub(crate) fn label_of(concept: &SkosConcept) -> String {
    concept
        .pref_labels
        .get("en")
        .or_else(|| {
            // Deterministic fallback: lowest language key (the labels
            // map is a HashMap; .values().next() would be random and
            // leak into closure.json and the snapshot id).
            concept
                .pref_labels
                .iter()
                .min_by(|a, b| a.0.cmp(b.0))
                .map(|(_, v)| v)
        })
        .map(|v| v.value.clone())
        .unwrap_or_else(|| concept.id.clone())
}

/// Normalize parsed collections for determinism: the SKOS XML parser
/// assembles schemes and children via HashSet/HashMap, so collection
/// order (within one XML file) and sibling order (beyond sort_order
/// ties) are run-random. Sort collections by id and every children Vec
/// by (sort_order, id) recursively — this fixes closure.json, the
/// concept DFS interning order, and hence the snapshot id.
fn normalize_collections(collections: &mut Vec<SkosCollection>) {
    collections.sort_by(|a, b| a.id.cmp(&b.id));
    fn sort_children(concept: &mut SkosConcept) {
        if let Some(children) = concept.children.as_mut() {
            children.sort_by(|a, b| {
                a.sort_order
                    .unwrap_or(i32::MAX)
                    .cmp(&b.sort_order.unwrap_or(i32::MAX))
                    .then_with(|| a.id.cmp(&b.id))
            });
            for child in children.iter_mut() {
                sort_children(child);
            }
        }
    }
    for coll in collections {
        for concept in coll.concepts.values_mut() {
            sort_children(concept);
        }
        for concept in coll.all_concepts.values_mut() {
            sort_children(concept);
        }
    }
}

/// Load SKOS collections from the data dir and normalize them for
/// determinism. Prebuild layouts keep vocabularies under
/// reference_data/{concepts,collections,controlled_lists,staging}
/// (XML concept schemes + JSON collections) — PrebuildLoader handles
/// the subdirectory scan and XML-over-JSON dedup. The flat
/// vocabularies/ dir remains supported for the Clódóir layout.
pub(crate) fn load_collections(
    data_dir: &Path,
    base_uri: &str,
) -> Result<Vec<SkosCollection>, EmitError> {
    let mut collections: Vec<SkosCollection> = if data_dir.join("reference_data").is_dir() {
        PrebuildLoader::new(data_dir)
            .and_then(|loader| loader.load_collections(base_uri))
            .map_err(|e| format!("loading reference_data SKOS: {e}"))?
    } else {
        Vec::new()
    };
    let vocab_dir = data_dir.join("vocabularies");
    if vocab_dir.is_dir() {
        let extra = load_collections_from_dir(vocab_dir.to_str().unwrap_or(""), base_uri)
            .map_err(|e| format!("loading vocabularies SKOS: {e}"))?;
        let seen: HashSet<String> = collections.iter().map(|c| c.id.clone()).collect();
        collections.extend(extra.into_iter().filter(|c| !seen.contains(&c.id)));
    }
    normalize_collections(&mut collections);
    Ok(collections)
}

fn walk_concepts(
    closure: &mut Closure,
    collection_id: &str,
    concept: &SkosConcept,
    mut ancestors: Vec<String>,
) {
    ancestors.push(concept.id.clone());
    closure.concepts.insert(
        concept.id.clone(),
        ClosureEntry {
            label: label_of(concept),
            collection_id: collection_id.to_string(),
            parent: ancestors.len().checked_sub(2).map(|i| ancestors[i].clone()),
            ancestors: ancestors.clone(),
        },
    );
    // Map every pref-label value id (what concept tiles store) to the concept.
    for value in concept.pref_labels.values() {
        if !value.id.is_empty() {
            closure
                .value_map
                .insert(value.id.clone(), concept.id.clone());
        }
    }
    for child in concept.children.iter().flatten() {
        walk_concepts(closure, collection_id, child, ancestors.clone());
    }
}

pub fn build_closure(collections: &[SkosCollection]) -> Closure {
    let mut closure = Closure::default();
    for coll in collections {
        // The concept maps are HashMaps — sort top-level by id so the
        // walk (and hence which ancestor chain wins for poly-hierarchy
        // concepts, last-insert-wins) is deterministic, mirroring
        // preintern_concepts (P16; otherwise closure.json and the
        // snapshot id vary run to run).
        let mut top: Vec<&SkosConcept> = if !coll.concepts.is_empty() {
            coll.concepts.values().collect()
        } else {
            coll.all_concepts.values().collect()
        };
        top.sort_by(|a, b| a.id.cmp(&b.id));
        for concept in top {
            walk_concepts(&mut closure, &coll.id, concept, Vec::new());
        }
        // Standalone value ids declared at collection level.
        for (vid, val) in &coll.values {
            if !closure.value_map.contains_key(vid) && closure.concepts.contains_key(&val.id) {
                closure.value_map.insert(vid.clone(), val.id.clone());
            }
        }
    }
    closure
}
