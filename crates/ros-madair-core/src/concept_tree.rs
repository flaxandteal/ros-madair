// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Binary concept tree for browsing and label resolution.
//!
//! Replaces both `concept_labels.json` (label→value_id) and
//! `concept_hierarchy.json` (full SKOS tree for browsing) with a single
//! compact binary file `concept_tree.bin` (~3 MB vs 21 MB JSON).
//!
//! ## Binary format: `concept_tree.bin`
//!
//! ```text
//! Header (20 bytes):
//!   magic: "RMCT"           4 bytes
//!   version: u8 = 1         1 byte
//!   padding:                3 bytes
//!   collection_count: u32   4 bytes
//!   entry_count: u32        4 bytes
//!   strings_offset: u32     4 bytes  (byte offset to string table)
//!
//! Collection Table (44 bytes each, sorted by collection_id):
//!   collection_id: [u8; 36]   UUID string
//!   first_entry: u32          index into entries array
//!   entry_count: u32
//!
//! Entries (52 bytes each, grouped by collection, sorted by (depth, dfs_enter)):
//!   dfs_enter: u32
//!   dfs_leave: u32
//!   depth: u16
//!   label_len: u16
//!   value_id: [u8; 36]        UUID string
//!   label_offset: u32         byte offset into string table
//!
//! String Table:
//!   Concatenated UTF-8 label strings
//! ```
//!
//! One entry per (concept, language) pair — each SkosValue gets its own entry.
//! Entries sharing the same `(dfs_enter, dfs_leave, depth)` are the same
//! concept in different languages.
//!
//! ## Shared DFS walk
//!
//! [`build_concept_indexes`] does a single DFS walk over SKOS collections,
//! producing both a [`ConceptIntervalIndex`] (for the query engine) and a
//! [`ConceptTree`] (for browsing/label lookup). This guarantees consistent
//! DFS numbering between the two.

use std::collections::HashMap;

use crate::concept_intervals::{ConceptInterval, ConceptIntervalIndex};
use crate::dictionary::Dictionary;
use crate::uri::concept_prefix;

const MAGIC: &[u8; 4] = b"RMCT";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 20; // 4 magic + 1 version + 3 pad + 4 coll_count + 4 entry_count + 4 strings_offset
const COLLECTION_ENTRY_SIZE: usize = 44; // 36 UUID + 4 first_entry + 4 entry_count
const ENTRY_SIZE: usize = 52; // 4+4+2+2+36+4

/// A single concept entry in the tree (one per language per concept).
#[derive(Debug, Clone)]
pub struct ConceptEntry {
    pub dfs_enter: u32,
    pub dfs_leave: u32,
    pub depth: u16,
    pub value_id: String, // 36-char UUID
    pub label: String,
}

/// A collection header in the binary format.
#[derive(Debug, Clone)]
pub struct CollectionHeader {
    pub collection_id: String, // 36-char UUID
    pub first_entry: u32,
    pub entry_count: u32,
}

/// Information about a concept, returned by browsing APIs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConceptInfo {
    pub value_id: String,
    pub label: String,
    pub has_children: bool,
}

/// Binary concept tree supporting label resolution and hierarchy browsing.
#[derive(Debug, Clone)]
pub struct ConceptTree {
    collections: Vec<CollectionHeader>,
    entries: Vec<ConceptEntry>,
    /// (collection_id, lowercase_label) -> value_id (for label resolution).
    /// Labels are unique within a collection; scoping by collection avoids
    /// cross-collection collisions (e.g. "Church" as monument type vs townland).
    label_to_value_id: HashMap<(String, String), String>,
    /// value_id -> index into entries (for drill-down parent lookup).
    value_id_to_entry: HashMap<String, usize>,
    /// collection_id -> index into collections vec.
    collection_index: HashMap<String, usize>,
}

impl ConceptTree {
    /// Look up a concept value_id by collection and label (case-insensitive).
    pub fn lookup_label(&self, collection_id: &str, label: &str) -> Option<&str> {
        self.label_to_value_id
            .get(&(collection_id.to_string(), label.to_lowercase()))
            .map(|s| s.as_str())
    }

    /// List top-level concepts (depth=0) in a collection.
    pub fn list_top_level(&self, collection_id: &str) -> Vec<ConceptInfo> {
        let coll_idx = match self.collection_index.get(collection_id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };
        let coll = &self.collections[coll_idx];
        let start = coll.first_entry as usize;
        let end = start + coll.entry_count as usize;

        self.collect_at_depth(&self.entries[start..end], 0)
    }

    /// List children of a concept (by value_id) within a collection.
    pub fn list_children(&self, collection_id: &str, parent_value_id: &str) -> Vec<ConceptInfo> {
        let coll_idx = match self.collection_index.get(collection_id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };
        let coll = &self.collections[coll_idx];
        let coll_start = coll.first_entry as usize;
        let coll_end = coll_start + coll.entry_count as usize;

        // Find the parent entry to get its DFS interval and depth
        let parent_idx = match self.value_id_to_entry.get(parent_value_id) {
            Some(&idx) => idx,
            None => return Vec::new(),
        };
        // Verify parent is in this collection
        if parent_idx < coll_start || parent_idx >= coll_end {
            return Vec::new();
        }

        let parent = &self.entries[parent_idx];
        let child_depth = parent.depth + 1;

        // Scan entries in [coll_start..coll_end] for entries at child_depth
        // whose dfs_enter is within [parent.dfs_enter, parent.dfs_leave].
        // Because entries are sorted by (depth, dfs_enter), we can scan
        // the child_depth region.
        let slice = &self.entries[coll_start..coll_end];
        let children: Vec<&ConceptEntry> = slice
            .iter()
            .filter(|e| {
                e.depth == child_depth
                    && e.dfs_enter >= parent.dfs_enter
                    && e.dfs_enter <= parent.dfs_leave
            })
            .collect();

        self.dedup_entries(&children)
    }

    /// Return all collection IDs.
    pub fn collection_ids(&self) -> Vec<&str> {
        self.collections
            .iter()
            .map(|c| c.collection_id.as_str())
            .collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    // ---------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------

    /// Collect entries at a specific depth, deduplicating by dfs_enter
    /// (multiple language entries share the same dfs_enter).
    fn collect_at_depth(&self, entries: &[ConceptEntry], depth: u16) -> Vec<ConceptInfo> {
        let at_depth: Vec<&ConceptEntry> = entries
            .iter()
            .filter(|e| e.depth == depth)
            .collect();
        self.dedup_entries(&at_depth)
    }

    /// Deduplicate entries by dfs_enter, picking the first label encountered.
    fn dedup_entries(&self, entries: &[&ConceptEntry]) -> Vec<ConceptInfo> {
        let mut seen = HashMap::new();
        let mut result = Vec::new();
        for entry in entries {
            if seen.contains_key(&entry.dfs_enter) {
                continue;
            }
            seen.insert(entry.dfs_enter, true);
            result.push(ConceptInfo {
                value_id: entry.value_id.clone(),
                label: entry.label.clone(),
                has_children: entry.dfs_enter != entry.dfs_leave,
            });
        }
        result
    }

    // ---------------------------------------------------------------
    // Binary serialization
    // ---------------------------------------------------------------

    /// Serialize to binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let coll_count = self.collections.len();
        let entry_count = self.entries.len();

        // Calculate string table: concatenated labels
        let mut string_table = Vec::new();
        let mut label_offsets: Vec<u32> = Vec::with_capacity(entry_count);
        for entry in &self.entries {
            label_offsets.push(string_table.len() as u32);
            string_table.extend_from_slice(entry.label.as_bytes());
        }

        let strings_offset = HEADER_SIZE
            + coll_count * COLLECTION_ENTRY_SIZE
            + entry_count * ENTRY_SIZE;

        let total = strings_offset + string_table.len();
        let mut buf = Vec::with_capacity(total);

        // Header
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        buf.extend_from_slice(&[0u8; 3]); // padding
        buf.extend_from_slice(&(coll_count as u32).to_le_bytes());
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());
        buf.extend_from_slice(&(strings_offset as u32).to_le_bytes());

        // Collection table
        for coll in &self.collections {
            let mut id_bytes = [0u8; 36];
            let src = coll.collection_id.as_bytes();
            let len = src.len().min(36);
            id_bytes[..len].copy_from_slice(&src[..len]);
            buf.extend_from_slice(&id_bytes);
            buf.extend_from_slice(&coll.first_entry.to_le_bytes());
            buf.extend_from_slice(&coll.entry_count.to_le_bytes());
        }

        // Entries
        for (i, entry) in self.entries.iter().enumerate() {
            buf.extend_from_slice(&entry.dfs_enter.to_le_bytes());
            buf.extend_from_slice(&entry.dfs_leave.to_le_bytes());
            buf.extend_from_slice(&entry.depth.to_le_bytes());
            buf.extend_from_slice(&(entry.label.len() as u16).to_le_bytes());
            let mut vid_bytes = [0u8; 36];
            let src = entry.value_id.as_bytes();
            let len = src.len().min(36);
            vid_bytes[..len].copy_from_slice(&src[..len]);
            buf.extend_from_slice(&vid_bytes);
            buf.extend_from_slice(&label_offsets[i].to_le_bytes());
        }

        // String table
        buf.extend_from_slice(&string_table);

        buf
    }

    /// Deserialize from binary format.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < HEADER_SIZE {
            return Err("Concept tree data too short".into());
        }
        if &data[0..4] != MAGIC {
            return Err("Invalid concept tree magic".into());
        }
        if data[4] != VERSION {
            return Err(format!("Unsupported concept tree version: {}", data[4]));
        }

        let coll_count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let entry_count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        let strings_offset = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;

        let coll_start = HEADER_SIZE;
        let entries_start = coll_start + coll_count * COLLECTION_ENTRY_SIZE;
        let expected_strings = entries_start + entry_count * ENTRY_SIZE;
        if expected_strings != strings_offset {
            return Err(format!(
                "String table offset mismatch: expected {}, got {}",
                expected_strings, strings_offset
            ));
        }
        if data.len() < strings_offset {
            return Err("Concept tree data truncated before string table".into());
        }

        // Parse collections
        let mut collections = Vec::with_capacity(coll_count);
        for i in 0..coll_count {
            let base = coll_start + i * COLLECTION_ENTRY_SIZE;
            let id_bytes = &data[base..base + 36];
            let collection_id = std::str::from_utf8(id_bytes)
                .map_err(|e| format!("Invalid collection_id UTF-8: {e}"))?
                .trim_end_matches('\0')
                .to_string();
            let first_entry =
                u32::from_le_bytes(data[base + 36..base + 40].try_into().unwrap());
            let entry_count_val =
                u32::from_le_bytes(data[base + 40..base + 44].try_into().unwrap());
            collections.push(CollectionHeader {
                collection_id,
                first_entry,
                entry_count: entry_count_val,
            });
        }

        // Parse entries
        let mut entries = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let base = entries_start + i * ENTRY_SIZE;
            let dfs_enter = u32::from_le_bytes(data[base..base + 4].try_into().unwrap());
            let dfs_leave = u32::from_le_bytes(data[base + 4..base + 8].try_into().unwrap());
            let depth = u16::from_le_bytes(data[base + 8..base + 10].try_into().unwrap());
            let label_len = u16::from_le_bytes(data[base + 10..base + 12].try_into().unwrap());
            let vid_bytes = &data[base + 12..base + 48];
            let value_id = std::str::from_utf8(vid_bytes)
                .map_err(|e| format!("Invalid value_id UTF-8: {e}"))?
                .trim_end_matches('\0')
                .to_string();
            let label_offset =
                u32::from_le_bytes(data[base + 48..base + 52].try_into().unwrap()) as usize;

            let label_start = strings_offset + label_offset;
            let label_end = label_start + label_len as usize;
            if label_end > data.len() {
                return Err(format!(
                    "Label extends beyond data: {}..{} > {}",
                    label_start, label_end, data.len()
                ));
            }
            let label = std::str::from_utf8(&data[label_start..label_end])
                .map_err(|e| format!("Invalid label UTF-8: {e}"))?
                .to_string();

            entries.push(ConceptEntry {
                dfs_enter,
                dfs_leave,
                depth,
                value_id,
                label,
            });
        }

        let tree = Self::from_parts(collections, entries);

        Ok(tree)
    }

    /// Build runtime indexes from parsed collections and entries.
    fn from_parts(collections: Vec<CollectionHeader>, entries: Vec<ConceptEntry>) -> Self {
        let mut label_to_value_id = HashMap::new();
        let mut value_id_to_entry = HashMap::new();
        let mut collection_index = HashMap::new();

        for (i, coll) in collections.iter().enumerate() {
            collection_index.insert(coll.collection_id.clone(), i);
        }

        // Build (collection_id, label) -> value_id map, scoped per collection.
        for coll in &collections {
            let start = coll.first_entry as usize;
            let end = start + coll.entry_count as usize;
            for i in start..end {
                let entry = &entries[i];
                if !entry.label.is_empty() && !entry.value_id.is_empty() {
                    label_to_value_id
                        .entry((coll.collection_id.clone(), entry.label.to_lowercase()))
                        .or_insert_with(|| entry.value_id.clone());
                }
                if !entry.value_id.is_empty() {
                    value_id_to_entry
                        .entry(entry.value_id.clone())
                        .or_insert(i);
                }
            }
        }

        Self {
            collections,
            entries,
            label_to_value_id,
            value_id_to_entry,
            collection_index,
        }
    }
}

// ===================================================================
// Shared DFS builder
// ===================================================================

/// Intermediate data collected during the shared DFS walk.
struct DfsWalkState {
    counter: u32,
    intervals: Vec<ConceptInterval>,
    /// (collection_index, ConceptEntry) pairs — grouped later.
    entries: Vec<(usize, ConceptEntry)>,
}

/// Build both [`ConceptIntervalIndex`] and [`ConceptTree`] from SKOS
/// collections in a single DFS walk.
///
/// This guarantees consistent DFS numbering between the query engine's
/// interval index and the browsing tree.
pub fn build_concept_indexes(
    collections: &[alizarin_core::skos::SkosCollection],
    dict: &Dictionary,
    base_uri: &str,
) -> (ConceptIntervalIndex, ConceptTree) {
    let prefix = concept_prefix(base_uri);
    let mut state = DfsWalkState {
        counter: 0,
        intervals: Vec::new(),
        entries: Vec::new(),
    };

    let mut collection_headers: Vec<CollectionHeader> = Vec::new();

    for (coll_idx, collection) in collections.iter().enumerate() {
        let coll_id = collection.id.clone();
        let entries_before = state.entries.len();

        for concept in collection.concepts.values() {
            dfs_walk_shared(concept, &prefix, dict, coll_idx, 0, &mut state);
        }

        // Sort this collection's entries by (depth, dfs_enter)
        let entries_after = state.entries.len();
        state.entries[entries_before..entries_after]
            .sort_by(|a, b| {
                a.1.depth.cmp(&b.1.depth)
                    .then(a.1.dfs_enter.cmp(&b.1.dfs_enter))
            });

        collection_headers.push(CollectionHeader {
            collection_id: coll_id,
            first_entry: 0, // filled in below
            entry_count: (entries_after - entries_before) as u32,
        });
    }

    // Flatten entries, assigning first_entry for each collection.
    // Group by collection_index in order.
    let mut final_entries: Vec<ConceptEntry> = Vec::with_capacity(state.entries.len());
    let mut entry_offset = 0u32;
    for (coll_idx, header) in collection_headers.iter_mut().enumerate() {
        header.first_entry = entry_offset;
        for (ci, entry) in &state.entries {
            if *ci == coll_idx {
                final_entries.push(entry.clone());
                entry_offset += 1;
            }
        }
    }

    // Sort collection headers by collection_id
    // (and keep final_entries consistent by remapping)
    let mut coll_order: Vec<usize> = (0..collection_headers.len()).collect();
    coll_order.sort_by(|&a, &b| {
        collection_headers[a].collection_id.cmp(&collection_headers[b].collection_id)
    });

    let sorted_headers: Vec<CollectionHeader> = coll_order
        .iter()
        .map(|&i| collection_headers[i].clone())
        .collect();

    let interval_index = ConceptIntervalIndex::from_intervals(state.intervals);
    let tree = ConceptTree::from_parts(sorted_headers, final_entries);

    (interval_index, tree)
}

/// Shared DFS walk over a concept and its children.
fn dfs_walk_shared(
    concept: &alizarin_core::skos::SkosConcept,
    prefix: &str,
    dict: &Dictionary,
    coll_idx: usize,
    depth: u16,
    state: &mut DfsWalkState,
) {
    let enter = state.counter;
    state.counter += 1;

    // Recurse into children
    if let Some(children) = &concept.children {
        for child in children {
            dfs_walk_shared(child, prefix, dict, coll_idx, depth + 1, state);
        }
    }

    let leave = state.counter.saturating_sub(1);

    // Emit interval entries (for ConceptIntervalIndex) and tree entries
    // (for ConceptTree) for each language's value_id.
    for (lang, value) in &concept.pref_labels {
        if value.id.is_empty() {
            continue;
        }

        // Interval index entry (needs dict_id)
        let uri = format!("{prefix}{}", value.id);
        if let Some(dict_id) = dict.lookup(&uri) {
            state.intervals.push(ConceptInterval {
                dict_id,
                dfs_enter: enter,
                dfs_leave: leave,
            });
        }

        // Extract label text. In Arches-format SKOS, SkosValue.value can be
        // a JSON string like `{"id": "...", "value": "Church"}` rather than
        // a plain label. Try to extract the inner "value" field if present.
        let label = extract_label_text(&value.value);

        // Tree entry (label + browsing)
        state.entries.push((
            coll_idx,
            ConceptEntry {
                dfs_enter: enter,
                dfs_leave: leave,
                depth,
                value_id: value.id.clone(),
                label,
            },
        ));

        let _ = lang; // used as iteration key only
    }
}

/// Extract the plain label text from a SkosValue.value field.
///
/// Arches-format SKOS sometimes stores values as JSON strings like
/// `{"id": "...", "value": "Church"}` instead of plain `"Church"`.
/// This function tries to extract the inner "value" field, falling
/// back to the raw string if it's not JSON.
fn extract_label_text(raw: &str) -> String {
    if raw.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(inner) = parsed.get("value").and_then(|v| v.as_str()) {
                return inner.to_string();
            }
        }
    }
    raw.to_string()
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_tree() -> ConceptTree {
        // Simulate:
        //   Collection "col-1":
        //     Ecclesiastical (enter=0, leave=5, depth=0)
        //       Church (enter=1, leave=3, depth=1)
        //         Cathedral (enter=2, leave=2, depth=2)
        //         Meetinghouse (enter=3, leave=3, depth=2)
        //       Monastery (enter=4, leave=4, depth=1)
        //       Synagogue (enter=5, leave=5, depth=1)
        let entries = vec![
            // depth=0 first (sorted by depth, dfs_enter)
            ConceptEntry {
                dfs_enter: 0, dfs_leave: 5, depth: 0,
                value_id: "vid-ecclesiastical".into(), label: "Ecclesiastical Building".into(),
            },
            // depth=1
            ConceptEntry {
                dfs_enter: 1, dfs_leave: 3, depth: 1,
                value_id: "vid-church".into(), label: "Church".into(),
            },
            ConceptEntry {
                dfs_enter: 4, dfs_leave: 4, depth: 1,
                value_id: "vid-monastery".into(), label: "Monastery".into(),
            },
            ConceptEntry {
                dfs_enter: 5, dfs_leave: 5, depth: 1,
                value_id: "vid-synagogue".into(), label: "Synagogue".into(),
            },
            // depth=2
            ConceptEntry {
                dfs_enter: 2, dfs_leave: 2, depth: 2,
                value_id: "vid-cathedral".into(), label: "Cathedral".into(),
            },
            ConceptEntry {
                dfs_enter: 3, dfs_leave: 3, depth: 2,
                value_id: "vid-meetinghouse".into(), label: "Meetinghouse".into(),
            },
        ];

        let collections = vec![CollectionHeader {
            collection_id: "col-1".into(),
            first_entry: 0,
            entry_count: entries.len() as u32,
        }];

        ConceptTree::from_parts(collections, entries)
    }

    #[test]
    fn test_roundtrip() {
        let tree = make_test_tree();
        let bytes = tree.to_bytes();
        let tree2 = ConceptTree::from_bytes(&bytes).unwrap();

        assert_eq!(tree2.collections.len(), 1);
        assert_eq!(tree2.entries.len(), tree.entries.len());

        // Verify all entries survived
        for (a, b) in tree.entries.iter().zip(tree2.entries.iter()) {
            assert_eq!(a.dfs_enter, b.dfs_enter);
            assert_eq!(a.dfs_leave, b.dfs_leave);
            assert_eq!(a.depth, b.depth);
            assert_eq!(a.value_id, b.value_id);
            assert_eq!(a.label, b.label);
        }
    }

    #[test]
    fn test_label_lookup() {
        let tree = make_test_tree();
        assert_eq!(tree.lookup_label("col-1", "Church"), Some("vid-church"));
        assert_eq!(tree.lookup_label("col-1", "church"), Some("vid-church")); // case-insensitive
        assert_eq!(tree.lookup_label("col-1", "CATHEDRAL"), Some("vid-cathedral"));
        assert_eq!(tree.lookup_label("col-1", "Nonexistent"), None);
        assert_eq!(tree.lookup_label("col-999", "Church"), None); // wrong collection
    }

    #[test]
    fn test_list_top_level() {
        let tree = make_test_tree();
        let top = tree.list_top_level("col-1");
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].value_id, "vid-ecclesiastical");
        assert!(top[0].has_children); // enter=0, leave=5
    }

    #[test]
    fn test_list_children() {
        let tree = make_test_tree();

        // Children of Ecclesiastical (depth=0 -> depth=1)
        let children = tree.list_children("col-1", "vid-ecclesiastical");
        assert_eq!(children.len(), 3);
        let labels: Vec<&str> = children.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"Church"));
        assert!(labels.contains(&"Monastery"));
        assert!(labels.contains(&"Synagogue"));

        // Church has children (enter=1, leave=3)
        let church = children.iter().find(|c| c.label == "Church").unwrap();
        assert!(church.has_children);

        // Monastery has no children (enter=4, leave=4)
        let monastery = children.iter().find(|c| c.label == "Monastery").unwrap();
        assert!(!monastery.has_children);

        // Children of Church (depth=1 -> depth=2)
        let grandchildren = tree.list_children("col-1", "vid-church");
        assert_eq!(grandchildren.len(), 2);
        let labels: Vec<&str> = grandchildren.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"Cathedral"));
        assert!(labels.contains(&"Meetinghouse"));
    }

    #[test]
    fn test_list_children_leaf() {
        let tree = make_test_tree();
        let children = tree.list_children("col-1", "vid-cathedral");
        assert!(children.is_empty());
    }

    #[test]
    fn test_unknown_collection() {
        let tree = make_test_tree();
        assert!(tree.list_top_level("nonexistent").is_empty());
        assert!(tree.list_children("nonexistent", "vid-church").is_empty());
    }

    #[test]
    fn test_collection_ids() {
        let tree = make_test_tree();
        let ids = tree.collection_ids();
        assert_eq!(ids, vec!["col-1"]);
    }

    #[test]
    fn test_empty_tree() {
        let tree = ConceptTree::from_parts(Vec::new(), Vec::new());
        assert!(tree.is_empty());
        assert!(tree.list_top_level("x").is_empty());
        assert_eq!(tree.lookup_label("x", "x"), None);
        assert!(tree.collection_ids().is_empty());

        let bytes = tree.to_bytes();
        let tree2 = ConceptTree::from_bytes(&bytes).unwrap();
        assert!(tree2.is_empty());
    }

    #[test]
    fn test_multi_language_dedup() {
        // Same concept in two languages — should dedup by dfs_enter
        let entries = vec![
            ConceptEntry {
                dfs_enter: 0, dfs_leave: 0, depth: 0,
                value_id: "vid-a-en".into(), label: "Church".into(),
            },
            ConceptEntry {
                dfs_enter: 0, dfs_leave: 0, depth: 0,
                value_id: "vid-a-fr".into(), label: "Église".into(),
            },
        ];
        let collections = vec![CollectionHeader {
            collection_id: "col-1".into(),
            first_entry: 0,
            entry_count: 2,
        }];
        let tree = ConceptTree::from_parts(collections, entries);

        let top = tree.list_top_level("col-1");
        assert_eq!(top.len(), 1); // deduped

        // Both labels should be findable
        assert!(tree.lookup_label("col-1", "Church").is_some());
        assert!(tree.lookup_label("col-1", "Église").is_some());
    }
}
