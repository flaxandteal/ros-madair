// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Page-level summary quad index.
//!
//! A summary quad records that page S has edges with predicate P pointing to
//! objects in page O (or to quantized literal values), along with cardinality
//! counts. The summary index is loaded in full at init (~1-3MB for 1M resources)
//! and enables query planning with zero page fetches.
//!
//! ## Binary format
//!
//! Three sorted copies of the same quads (SPO, PSO, OPS) with a header:
//!
//! ```text
//! [header: 20 bytes]
//!   magic: [u8; 4]     = b"RMSQ"
//!   version: u8         = 1
//!   quad_count: u32 (LE)
//!   spo_offset: u32 (LE)   // always 20
//!   pso_offset: u32 (LE)
//!   ops_offset: u32 (LE)
//!
//! [SPO section: quad_count × 20 bytes, sorted by (page_s, pred, page_o)]
//! [PSO section: quad_count × 20 bytes, sorted by (pred, page_s, page_o)]
//! [OPS section: quad_count × 20 bytes, sorted by (page_o, pred, page_s)]
//! ```
//!
//! Each quad = 20 bytes: page_s(4) + pred(4) + page_o(4) + edge_count(4) + subject_count(4).

use std::collections::{HashMap, HashSet};

const MAGIC: &[u8; 4] = b"RMSQ";
const VERSION: u8 = 1;
const HEADER_SIZE: usize = 21;
const QUAD_SIZE: usize = 20;

/// A page-level summary quad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryQuad {
    /// Source page (page containing the subject resources).
    pub page_s: u32,
    /// Predicate dictionary ID.
    pub predicate: u32,
    /// Object page (page containing the object resource), or a quantized
    /// literal bucket for non-link predicates.
    pub page_o: u32,
    /// Number of resource-level edges summarized by this quad.
    pub edge_count: u32,
    /// Number of distinct subject resources in page_s with this (pred, page_o).
    pub subject_count: u32,
}

impl SummaryQuad {
    fn as_bytes(&self) -> [u8; QUAD_SIZE] {
        let mut buf = [0u8; QUAD_SIZE];
        buf[0..4].copy_from_slice(&self.page_s.to_le_bytes());
        buf[4..8].copy_from_slice(&self.predicate.to_le_bytes());
        buf[8..12].copy_from_slice(&self.page_o.to_le_bytes());
        buf[12..16].copy_from_slice(&self.edge_count.to_le_bytes());
        buf[16..20].copy_from_slice(&self.subject_count.to_le_bytes());
        buf
    }

    fn from_bytes(data: &[u8]) -> Self {
        Self {
            page_s: u32::from_le_bytes(data[0..4].try_into().unwrap()),
            predicate: u32::from_le_bytes(data[4..8].try_into().unwrap()),
            page_o: u32::from_le_bytes(data[8..12].try_into().unwrap()),
            edge_count: u32::from_le_bytes(data[12..16].try_into().unwrap()),
            subject_count: u32::from_le_bytes(data[16..20].try_into().unwrap()),
        }
    }
}

/// Accumulator for building summary quads from page records.
#[derive(Debug, Default)]
pub struct SummaryBuilder {
    /// (page_s, pred, page_o) → (edge_count, subject_set)
    accum: HashMap<(u32, u32, u32), (u32, HashSet<u32>)>,
}

impl SummaryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a record: subject in page_s has predicate pred with object in page_o.
    ///
    /// For link predicates (concept, resource-instance), page_o is the page
    /// containing the target resource/concept. For literal predicates
    /// (boolean, date, geo), page_o is the quantized value itself (used as
    /// a bucket key for summary-level range checks).
    pub fn add(
        &mut self,
        page_s: u32,
        pred: u32,
        page_o: u32,
        subject_id: u32,
    ) {
        let entry = self.accum.entry((page_s, pred, page_o)).or_default();
        entry.0 += 1;
        entry.1.insert(subject_id);
    }

    /// Build final summary quads.
    pub fn build(self) -> Vec<SummaryQuad> {
        self.accum
            .into_iter()
            .map(|((page_s, predicate, page_o), (edge_count, subjects))| SummaryQuad {
                page_s,
                predicate,
                page_o,
                edge_count,
                subject_count: subjects.len() as u32,
            })
            .collect()
    }
}

/// Serialize summary quads to binary: three sorted copies + header.
pub fn serialize_summary(quads: &[SummaryQuad]) -> Vec<u8> {
    let quad_count = quads.len();
    let section_size = quad_count * QUAD_SIZE;
    let total_size = HEADER_SIZE + 3 * section_size;

    let mut buf = Vec::with_capacity(total_size);

    // Header
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.extend_from_slice(&(quad_count as u32).to_le_bytes());
    let spo_offset = HEADER_SIZE as u32;
    let pso_offset = spo_offset + section_size as u32;
    let ops_offset = pso_offset + section_size as u32;
    buf.extend_from_slice(&spo_offset.to_le_bytes());
    buf.extend_from_slice(&pso_offset.to_le_bytes());
    buf.extend_from_slice(&ops_offset.to_le_bytes());

    // SPO sort
    let mut spo = quads.to_vec();
    spo.sort_by_key(|q| (q.page_s, q.predicate, q.page_o));
    for q in &spo {
        buf.extend_from_slice(&q.as_bytes());
    }

    // PSO sort
    let mut pso = quads.to_vec();
    pso.sort_by_key(|q| (q.predicate, q.page_s, q.page_o));
    for q in &pso {
        buf.extend_from_slice(&q.as_bytes());
    }

    // OPS sort
    let mut ops = quads.to_vec();
    ops.sort_by_key(|q| (q.page_o, q.predicate, q.page_s));
    for q in &ops {
        buf.extend_from_slice(&q.as_bytes());
    }

    buf
}

/// Deserialized summary index with three sorted views.
#[derive(Clone)]
pub struct SummaryIndex {
    spo: Vec<SummaryQuad>,
    pso: Vec<SummaryQuad>,
    ops: Vec<SummaryQuad>,
}

impl SummaryIndex {
    /// Deserialize from binary.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < HEADER_SIZE {
            return Err("Summary data too short".into());
        }
        if &data[0..4] != MAGIC {
            return Err("Invalid summary magic".into());
        }
        if data[4] != VERSION {
            return Err(format!("Unsupported summary version: {}", data[4]));
        }

        let quad_count =
            u32::from_le_bytes(data[5..9].try_into().unwrap()) as usize;
        let spo_offset =
            u32::from_le_bytes(data[9..13].try_into().unwrap()) as usize;
        let pso_offset =
            u32::from_le_bytes(data[13..17].try_into().unwrap()) as usize;
        let ops_offset =
            u32::from_le_bytes(data[17..21].try_into().unwrap()) as usize;

        let section_size = quad_count * QUAD_SIZE;
        let expected = ops_offset + section_size;
        if data.len() < expected {
            return Err(format!(
                "Summary data truncated: need {} bytes, got {}",
                expected,
                data.len()
            ));
        }

        let parse_section = |offset: usize| -> Vec<SummaryQuad> {
            (0..quad_count)
                .map(|i| {
                    let base = offset + i * QUAD_SIZE;
                    SummaryQuad::from_bytes(&data[base..base + QUAD_SIZE])
                })
                .collect()
        };

        Ok(Self {
            spo: parse_section(spo_offset),
            pso: parse_section(pso_offset),
            ops: parse_section(ops_offset),
        })
    }

    /// Lookup by subject_page only → all quads from that page.
    pub fn lookup_s(&self, subj_page: u32) -> &[SummaryQuad] {
        let start = self.spo.partition_point(|q| q.page_s < subj_page);
        let end = self.spo[start..]
            .partition_point(|q| q.page_s <= subj_page)
            + start;
        &self.spo[start..end]
    }

    /// Lookup by (subject_page, predicate) → matching quads.
    pub fn lookup_sp(&self, subj_page: u32, pred: u32) -> &[SummaryQuad] {
        let start = self
            .spo
            .partition_point(|q| (q.page_s, q.predicate) < (subj_page, pred));
        let end = self.spo[start..]
            .partition_point(|q| (q.page_s, q.predicate) <= (subj_page, pred))
            + start;
        &self.spo[start..end]
    }

    /// Lookup by (predicate, subject_page) → matching quads.
    pub fn lookup_ps(&self, pred: u32, subj_page: u32) -> &[SummaryQuad] {
        let start = self
            .pso
            .partition_point(|q| (q.predicate, q.page_s) < (pred, subj_page));
        let end = self.pso[start..]
            .partition_point(|q| (q.predicate, q.page_s) <= (pred, subj_page))
            + start;
        &self.pso[start..end]
    }

    /// Lookup by predicate only → all quads with that predicate.
    pub fn lookup_p(&self, pred: u32) -> &[SummaryQuad] {
        let start = self.pso.partition_point(|q| q.predicate < pred);
        let end = self.pso[start..]
            .partition_point(|q| q.predicate <= pred)
            + start;
        &self.pso[start..end]
    }

    /// Lookup by (object_page, predicate) → matching quads.
    pub fn lookup_op(&self, obj_page: u32, pred: u32) -> &[SummaryQuad] {
        let start = self
            .ops
            .partition_point(|q| (q.page_o, q.predicate) < (obj_page, pred));
        let end = self.ops[start..]
            .partition_point(|q| (q.page_o, q.predicate) <= (obj_page, pred))
            + start;
        &self.ops[start..end]
    }

    /// Lookup by object_page only → all quads with that object page.
    pub fn lookup_o(&self, obj_page: u32) -> &[SummaryQuad] {
        let start = self.ops.partition_point(|q| q.page_o < obj_page);
        let end = self.ops[start..]
            .partition_point(|q| q.page_o <= obj_page)
            + start;
        &self.ops[start..end]
    }

    /// Count total edges matching (predicate, object_page) across all subject pages.
    /// Answerable from summary alone — zero page loads.
    pub fn count_where(&self, pred: u32, obj_page: u32) -> u64 {
        self.lookup_op(obj_page, pred)
            .iter()
            .map(|q| q.edge_count as u64)
            .sum()
    }

    /// All subject pages that have edges with (predicate, object_page).
    pub fn subject_pages_for(&self, pred: u32, obj_page: u32) -> Vec<u32> {
        self.lookup_op(obj_page, pred)
            .iter()
            .map(|q| q.page_s)
            .collect()
    }

    /// All object pages reachable from subject_page via predicate.
    pub fn object_pages_for(&self, subj_page: u32, pred: u32) -> Vec<u32> {
        self.lookup_sp(subj_page, pred)
            .iter()
            .map(|q| q.page_o)
            .collect()
    }

    /// Range lookup on OPS index: all quads where `lo <= page_o <= hi` and
    /// `predicate == pred`.
    ///
    /// Used for DFS interval concept queries: `lo` and `hi` are
    /// `dfs_enter | DFS_OFFSET` and `dfs_leave | DFS_OFFSET`.
    pub fn lookup_op_range(&self, lo: u32, hi: u32, pred: u32) -> Vec<&SummaryQuad> {
        let start = self.ops.partition_point(|q| q.page_o < lo);
        let end = self.ops[start..]
            .partition_point(|q| q.page_o <= hi)
            + start;
        self.ops[start..end]
            .iter()
            .filter(|q| q.predicate == pred)
            .collect()
    }

    /// Total quads in the index.
    pub fn len(&self) -> usize {
        self.spo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_quads() -> Vec<SummaryQuad> {
        vec![
            SummaryQuad { page_s: 0, predicate: 10, page_o: 1, edge_count: 5, subject_count: 3 },
            SummaryQuad { page_s: 0, predicate: 10, page_o: 2, edge_count: 2, subject_count: 2 },
            SummaryQuad { page_s: 0, predicate: 20, page_o: 3, edge_count: 10, subject_count: 8 },
            SummaryQuad { page_s: 1, predicate: 10, page_o: 0, edge_count: 3, subject_count: 3 },
            SummaryQuad { page_s: 2, predicate: 20, page_o: 3, edge_count: 7, subject_count: 5 },
        ]
    }

    #[test]
    fn test_serialize_roundtrip() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();
        assert_eq!(idx.len(), 5);
    }

    #[test]
    fn test_lookup_sp() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();

        let results = idx.lookup_sp(0, 10);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|q| q.page_s == 0 && q.predicate == 10));
    }

    #[test]
    fn test_lookup_op() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();

        let results = idx.lookup_op(3, 20);
        assert_eq!(results.len(), 2); // page_s=0 and page_s=2 both point to page_o=3 via pred=20
    }

    #[test]
    fn test_count_where() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();

        assert_eq!(idx.count_where(20, 3), 17); // 10 + 7
        assert_eq!(idx.count_where(10, 1), 5);
        assert_eq!(idx.count_where(99, 99), 0);
    }

    #[test]
    fn test_subject_pages_for() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();

        let mut pages = idx.subject_pages_for(20, 3);
        pages.sort();
        assert_eq!(pages, vec![0, 2]);
    }

    #[test]
    fn test_object_pages_for() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();

        let mut pages = idx.object_pages_for(0, 10);
        pages.sort();
        assert_eq!(pages, vec![1, 2]);
    }

    #[test]
    fn test_lookup_p() {
        let quads = sample_quads();
        let bytes = serialize_summary(&quads);
        let idx = SummaryIndex::from_bytes(&bytes).unwrap();

        let results = idx.lookup_p(10);
        assert_eq!(results.len(), 3); // three quads with predicate=10
    }

    #[test]
    fn test_builder() {
        let mut builder = SummaryBuilder::new();
        builder.add(0, 10, 1, 100);
        builder.add(0, 10, 1, 101);
        builder.add(0, 10, 1, 100); // duplicate subject
        builder.add(0, 10, 2, 102);

        let quads = builder.build();
        assert_eq!(quads.len(), 2); // two distinct (page_s, pred, page_o) combos

        let q1 = quads.iter().find(|q| q.page_o == 1).unwrap();
        assert_eq!(q1.edge_count, 3);
        assert_eq!(q1.subject_count, 2); // subjects 100 and 101
    }
}
