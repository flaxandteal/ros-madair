// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! DFS interval encoding for concept hierarchies.
//!
//! Assigns each concept in a SKOS collection a DFS enter/leave interval
//! via an Euler tour. This encoding enables hierarchical concept queries
//! as contiguous range scans on sorted page records.
//!
//! ## Encoding scheme
//!
//! ```text
//! Ecclesiastical Building (enter=100, leave=150)
//! +-- Church (enter=101, leave=120)
//! |   +-- Cathedral (enter=102, leave=102)
//! |   +-- Meetinghouse (enter=103, leave=103)
//! +-- Monastery (enter=121, leave=130)
//! +-- Synagogue (enter=131, leave=131)
//! ```
//!
//! Query "Church" -> range scan [101, 120] -> Church + Cathedral + Meetinghouse
//!
//! ## Namespace separation
//!
//! Bit 31 (`DFS_OFFSET = 0x8000_0000`) separates concept DFS values from
//! resource page IDs in the summary index:
//! - `page_o < 0x8000_0000` -> resource page ID
//! - `page_o >= 0x8000_0000` -> concept DFS enter value
//!
//! ## Binary format: `concept_intervals.bin`
//!
//! ```text
//! magic: "RMCI" (4 bytes)
//! version: u8 = 1
//! padding: 3 bytes
//! entry_count: u32 LE
//!
//! // Forward index: sorted by dict_id
//! (dict_id: u32, dfs_enter: u32, dfs_leave: u32) x entry_count  [12 bytes each]
//!
//! // Reverse index: sorted by dfs_enter
//! (dfs_enter: u32, dict_id: u32) x entry_count  [8 bytes each]
//! ```

const MAGIC: &[u8; 4] = b"RMCI";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 12; // 4 magic + 1 version + 3 padding + 4 entry_count

/// Bit flag to distinguish concept DFS values from page IDs in summary quads.
pub const DFS_OFFSET: u32 = 0x8000_0000;

/// A single concept's DFS interval, keyed by its dictionary ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConceptInterval {
    pub dict_id: u32,
    pub dfs_enter: u32,
    pub dfs_leave: u32,
}

/// Index mapping concept dictionary IDs to DFS intervals and back.
#[derive(Debug, Clone)]
pub struct ConceptIntervalIndex {
    /// Sorted by dict_id for forward lookup.
    by_dict_id: Vec<ConceptInterval>,
    /// (dfs_enter, dict_id) sorted by dfs_enter for reverse lookup.
    by_dfs_enter: Vec<(u32, u32)>,
}

impl ConceptIntervalIndex {
    /// Build from a pre-computed list of intervals.
    pub fn from_intervals(mut intervals: Vec<ConceptInterval>) -> Self {
        // Deduplicate by dict_id (same value_id might appear if collections overlap)
        intervals.sort_by_key(|ci| ci.dict_id);
        intervals.dedup_by_key(|ci| ci.dict_id);

        let mut by_dfs_enter: Vec<(u32, u32)> = intervals
            .iter()
            .map(|ci| (ci.dfs_enter, ci.dict_id))
            .collect();
        by_dfs_enter.sort_by_key(|&(enter, _)| enter);

        ConceptIntervalIndex {
            by_dict_id: intervals,
            by_dfs_enter,
        }
    }

    /// Forward lookup: dict_id -> (dfs_enter, dfs_leave).
    pub fn lookup(&self, dict_id: u32) -> Option<(u32, u32)> {
        self.by_dict_id
            .binary_search_by_key(&dict_id, |ci| ci.dict_id)
            .ok()
            .map(|idx| {
                let ci = &self.by_dict_id[idx];
                (ci.dfs_enter, ci.dfs_leave)
            })
    }

    /// Reverse lookup: dfs_enter -> dict_id.
    pub fn reverse_lookup(&self, dfs_enter: u32) -> Option<u32> {
        self.by_dfs_enter
            .binary_search_by_key(&dfs_enter, |&(e, _)| e)
            .ok()
            .map(|idx| self.by_dfs_enter[idx].1)
    }

    /// Encode a DFS enter value for use as `page_o` in summary quads.
    pub fn encode_for_summary(dfs_enter: u32) -> u32 {
        dfs_enter | DFS_OFFSET
    }

    /// Check whether a `page_o` value represents a concept DFS entry.
    pub fn is_concept_page_o(page_o: u32) -> bool {
        page_o & DFS_OFFSET != 0
    }

    /// Decode a concept `page_o` back to its raw DFS enter value.
    pub fn decode_from_summary(page_o: u32) -> u32 {
        page_o & !DFS_OFFSET
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.by_dict_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_dict_id.is_empty()
    }

    /// Serialize to binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let entry_count = self.by_dict_id.len();
        let forward_size = entry_count * 12;
        let reverse_size = entry_count * 8;
        let total = HEADER_SIZE + forward_size + reverse_size;

        let mut buf = Vec::with_capacity(total);

        // Header
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        buf.extend_from_slice(&[0u8; 3]); // padding
        buf.extend_from_slice(&(entry_count as u32).to_le_bytes());

        // Forward index (sorted by dict_id)
        for ci in &self.by_dict_id {
            buf.extend_from_slice(&ci.dict_id.to_le_bytes());
            buf.extend_from_slice(&ci.dfs_enter.to_le_bytes());
            buf.extend_from_slice(&ci.dfs_leave.to_le_bytes());
        }

        // Reverse index (sorted by dfs_enter)
        for &(dfs_enter, dict_id) in &self.by_dfs_enter {
            buf.extend_from_slice(&dfs_enter.to_le_bytes());
            buf.extend_from_slice(&dict_id.to_le_bytes());
        }

        buf
    }

    /// Deserialize from binary format.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < HEADER_SIZE {
            return Err("Concept interval data too short".into());
        }
        if &data[0..4] != MAGIC {
            return Err("Invalid concept interval magic".into());
        }
        if data[4] != VERSION {
            return Err(format!("Unsupported concept interval version: {}", data[4]));
        }

        let entry_count =
            u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

        let forward_size = entry_count * 12;
        let reverse_size = entry_count * 8;
        let expected = HEADER_SIZE + forward_size + reverse_size;
        if data.len() < expected {
            return Err(format!(
                "Concept interval data truncated: need {} bytes, got {}",
                expected,
                data.len()
            ));
        }

        let forward_start = HEADER_SIZE;
        let mut by_dict_id = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let base = forward_start + i * 12;
            let dict_id = u32::from_le_bytes(data[base..base + 4].try_into().unwrap());
            let dfs_enter = u32::from_le_bytes(data[base + 4..base + 8].try_into().unwrap());
            let dfs_leave = u32::from_le_bytes(data[base + 8..base + 12].try_into().unwrap());
            by_dict_id.push(ConceptInterval {
                dict_id,
                dfs_enter,
                dfs_leave,
            });
        }

        let reverse_start = forward_start + forward_size;
        let mut by_dfs_enter = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            let base = reverse_start + i * 8;
            let dfs_enter = u32::from_le_bytes(data[base..base + 4].try_into().unwrap());
            let dict_id = u32::from_le_bytes(data[base + 4..base + 8].try_into().unwrap());
            by_dfs_enter.push((dfs_enter, dict_id));
        }

        Ok(Self {
            by_dict_id,
            by_dfs_enter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_index() -> ConceptIntervalIndex {
        // Simulate a tree:
        //   Ecclesiastical (enter=0, leave=5)
        //     Church (enter=1, leave=3)
        //       Cathedral (enter=2, leave=2)
        //       Meetinghouse (enter=3, leave=3)
        //     Monastery (enter=4, leave=4)
        //     Synagogue (enter=5, leave=5)
        let intervals = vec![
            ConceptInterval { dict_id: 10, dfs_enter: 0, dfs_leave: 5 },  // Ecclesiastical
            ConceptInterval { dict_id: 11, dfs_enter: 1, dfs_leave: 3 },  // Church
            ConceptInterval { dict_id: 12, dfs_enter: 2, dfs_leave: 2 },  // Cathedral
            ConceptInterval { dict_id: 13, dfs_enter: 3, dfs_leave: 3 },  // Meetinghouse
            ConceptInterval { dict_id: 14, dfs_enter: 4, dfs_leave: 4 },  // Monastery
            ConceptInterval { dict_id: 15, dfs_enter: 5, dfs_leave: 5 },  // Synagogue
        ];
        ConceptIntervalIndex::from_intervals(intervals)
    }

    #[test]
    fn test_forward_lookup() {
        let idx = make_test_index();
        assert_eq!(idx.lookup(10), Some((0, 5)));
        assert_eq!(idx.lookup(11), Some((1, 3)));
        assert_eq!(idx.lookup(12), Some((2, 2)));
        assert_eq!(idx.lookup(99), None);
    }

    #[test]
    fn test_reverse_lookup() {
        let idx = make_test_index();
        assert_eq!(idx.reverse_lookup(0), Some(10));
        assert_eq!(idx.reverse_lookup(1), Some(11));
        assert_eq!(idx.reverse_lookup(2), Some(12));
        assert_eq!(idx.reverse_lookup(99), None);
    }

    #[test]
    fn test_encode_decode_summary() {
        let encoded = ConceptIntervalIndex::encode_for_summary(42);
        assert!(ConceptIntervalIndex::is_concept_page_o(encoded));
        assert_eq!(ConceptIntervalIndex::decode_from_summary(encoded), 42);

        assert!(!ConceptIntervalIndex::is_concept_page_o(42));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let idx = make_test_index();
        let bytes = idx.to_bytes();
        let idx2 = ConceptIntervalIndex::from_bytes(&bytes).unwrap();

        assert_eq!(idx.len(), idx2.len());
        for ci in &idx.by_dict_id {
            assert_eq!(idx2.lookup(ci.dict_id), Some((ci.dfs_enter, ci.dfs_leave)));
        }
        for &(enter, dict_id) in &idx.by_dfs_enter {
            assert_eq!(idx2.reverse_lookup(enter), Some(dict_id));
        }
    }

    #[test]
    fn test_empty_index() {
        let idx = ConceptIntervalIndex::from_intervals(Vec::new());
        assert!(idx.is_empty());
        assert_eq!(idx.lookup(0), None);
        assert_eq!(idx.reverse_lookup(0), None);

        let bytes = idx.to_bytes();
        let idx2 = ConceptIntervalIndex::from_bytes(&bytes).unwrap();
        assert!(idx2.is_empty());
    }

    #[test]
    fn test_dedup_by_dict_id() {
        let intervals = vec![
            ConceptInterval { dict_id: 10, dfs_enter: 0, dfs_leave: 5 },
            ConceptInterval { dict_id: 10, dfs_enter: 0, dfs_leave: 5 },  // duplicate
            ConceptInterval { dict_id: 11, dfs_enter: 1, dfs_leave: 3 },
        ];
        let idx = ConceptIntervalIndex::from_intervals(intervals);
        assert_eq!(idx.len(), 2);
    }
}
