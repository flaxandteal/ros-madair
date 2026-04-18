// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Per-page binary file format with predicate-partitioned fixed-width records.
//!
//! Each page file contains all indexed records for resources assigned to that
//! page. Records are 8 bytes each: `(object_val: u32, subject_id: u32)`,
//! partitioned by predicate and sorted by `(object_val, subject_id)` within
//! each partition.
//!
//! ## File layout (v3)
//!
//! ```text
//! [header]
//!   magic: [u8; 4]                = b"RMPG"
//!   version: u8                    = 3
//!   predicate_count: u16 (LE)
//!   resource_meta_offset: u32 (LE)
//!   resource_meta_size: u32 (LE)
//!   entries: [(pred_id: u32, offset: u32, record_count: u32)]  // 12 bytes each
//!
//! [body]
//!   block_0: [PageRecord; N0]   // 8 bytes × N0, sorted by (object_val, subject_id)
//!   block_1: [PageRecord; N1]
//!   ...
//!
//! [resource metadata]
//!   count: u32 (LE)
//!   entries: [dict_id: u32, name: len-prefixed string, slug: ..., model: ...]
//! ```
//!
//! Header size = 15 + 12 × predicate_count.
//! Client fetches header first (~100-500 bytes), then binary-searches the
//! relevant predicate block(s) via range requests.

use crate::quantize::PageRecord;

const MAGIC: &[u8; 4] = b"RMPG";
const VERSION: u8 = 3;
const RECORD_SIZE: u32 = 8;

/// A predicate block entry in the page header.
#[derive(Debug, Clone, Copy)]
pub struct PredicateEntry {
    /// Dictionary ID of the predicate.
    pub pred_id: u32,
    /// Byte offset from start of file where this block begins.
    pub offset: u32,
    /// Number of 8-byte records in this block.
    pub record_count: u32,
}

/// Resource metadata embedded in v3 page files.
#[derive(Debug, Clone)]
pub struct ResourceMeta {
    pub dict_id: u32,
    pub name: String,
    pub slug: String,
    pub model: String,
}

/// Parsed page file header.
#[derive(Debug, Clone)]
pub struct PageHeader {
    pub entries: Vec<PredicateEntry>,
    /// (offset, size) of the resource metadata section.
    pub resource_meta_range: Option<(u32, u32)>,
}

impl PageHeader {
    /// Total header size in bytes: 15 + 12 × predicate_count.
    pub fn header_size(&self) -> u32 {
        15 + 12 * self.entries.len() as u32
    }

    /// Find the entry for a given predicate ID.
    pub fn entry_for_predicate(&self, pred_id: u32) -> Option<&PredicateEntry> {
        self.entries.iter().find(|e| e.pred_id == pred_id)
    }

    /// Byte range (start, end exclusive) for a predicate's record block.
    pub fn predicate_byte_range(&self, pred_id: u32) -> Option<(u32, u32)> {
        self.entry_for_predicate(pred_id).map(|e| {
            (e.offset, e.offset + e.record_count * RECORD_SIZE)
        })
    }
}

/// A predicate block with its records, ready to write.
pub struct PredicateBlock {
    pub pred_id: u32,
    pub records: Vec<PageRecord>,
}

/// Write a complete page file from predicate blocks.
///
/// Records within each block must already be sorted by (object_val, subject_id).
/// Pass `&[]` for `resource_meta` when no metadata is available.
pub fn write_page_file(blocks: &mut [PredicateBlock], resource_meta: &[ResourceMeta]) -> Vec<u8> {
    // Sort blocks by pred_id for consistent ordering
    blocks.sort_by_key(|b| b.pred_id);

    let pred_count = blocks.len() as u16;
    let header_size = 15 + 12 * blocks.len();

    // Calculate offsets for predicate blocks
    let mut offset = header_size as u32;
    let mut entries: Vec<PredicateEntry> = Vec::with_capacity(blocks.len());
    for block in blocks.iter() {
        entries.push(PredicateEntry {
            pred_id: block.pred_id,
            offset,
            record_count: block.records.len() as u32,
        });
        offset += block.records.len() as u32 * RECORD_SIZE;
    }

    // Serialize resource metadata
    let meta_bytes = serialize_resource_meta(resource_meta);
    let resource_meta_offset = offset;
    let resource_meta_size = meta_bytes.len() as u32;

    // Write header
    let total_size = offset as usize + resource_meta_size as usize;
    let mut buf = Vec::with_capacity(total_size);
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.extend_from_slice(&pred_count.to_le_bytes());
    buf.extend_from_slice(&resource_meta_offset.to_le_bytes());
    buf.extend_from_slice(&resource_meta_size.to_le_bytes());

    for entry in &entries {
        buf.extend_from_slice(&entry.pred_id.to_le_bytes());
        buf.extend_from_slice(&entry.offset.to_le_bytes());
        buf.extend_from_slice(&entry.record_count.to_le_bytes());
    }

    // Write predicate blocks
    for block in blocks.iter() {
        for rec in &block.records {
            buf.extend_from_slice(&rec.to_bytes());
        }
    }

    // Write resource metadata section
    buf.extend_from_slice(&meta_bytes);

    buf
}

/// Parse a page file header from bytes.
pub fn parse_page_header(data: &[u8]) -> Result<PageHeader, String> {
    if data.len() < 15 {
        return Err("Page file too short for header".into());
    }
    if &data[0..4] != MAGIC {
        return Err("Invalid page file magic".into());
    }
    let version = data[4];
    if version != VERSION {
        return Err(format!("Unsupported page file version: {} (expected {})", version, VERSION));
    }
    let pred_count = u16::from_le_bytes(
        data[5..7].try_into().map_err(|_| "Failed to read predicate count")?,
    ) as usize;

    let meta_offset = u32::from_le_bytes(data[7..11].try_into().unwrap());
    let meta_size = u32::from_le_bytes(data[11..15].try_into().unwrap());
    let resource_meta_range = if meta_size > 0 {
        Some((meta_offset, meta_size))
    } else {
        None
    };

    let needed = 15 + 12 * pred_count;
    if data.len() < needed {
        return Err(format!(
            "Page header truncated: need {} bytes, got {}",
            needed,
            data.len()
        ));
    }

    let mut entries = Vec::with_capacity(pred_count);
    for i in 0..pred_count {
        let base = 15 + 12 * i;
        let pred_id = u32::from_le_bytes(data[base..base + 4].try_into().unwrap());
        let offset = u32::from_le_bytes(data[base + 4..base + 8].try_into().unwrap());
        let record_count = u32::from_le_bytes(data[base + 8..base + 12].try_into().unwrap());
        entries.push(PredicateEntry {
            pred_id,
            offset,
            record_count,
        });
    }

    Ok(PageHeader { entries, resource_meta_range })
}

/// Parse records from a predicate block's raw bytes.
pub fn parse_records(block_bytes: &[u8]) -> Vec<PageRecord> {
    block_bytes
        .chunks_exact(RECORD_SIZE as usize)
        .map(|chunk| {
            let bytes: &[u8; 8] = chunk.try_into().unwrap();
            PageRecord::from_bytes(bytes)
        })
        .collect()
}

/// Binary search within a sorted record block for records matching a specific
/// object_val. Returns the range of matching record indices.
pub fn binary_search_object(records: &[PageRecord], target: u32) -> (usize, usize) {
    // Lower bound
    let lo = records.partition_point(|r| r.object_val < target);
    // Upper bound
    let hi = records[lo..].partition_point(|r| r.object_val <= target) + lo;
    (lo, hi)
}

/// Binary search for records within an object_val range [lo_val, hi_val] inclusive.
pub fn range_search_object(records: &[PageRecord], lo_val: u32, hi_val: u32) -> (usize, usize) {
    let lo = records.partition_point(|r| r.object_val < lo_val);
    let hi = records[lo..].partition_point(|r| r.object_val <= hi_val) + lo;
    (lo, hi)
}

/// Minimum header bytes needed to determine the full header size.
/// Read this many bytes first, then read the rest if needed.
pub const MIN_HEADER_PROBE: usize = 15;

/// Calculate full header size from the first [`MIN_HEADER_PROBE`] bytes.
pub fn full_header_size(probe: &[u8]) -> Result<usize, String> {
    if probe.len() < MIN_HEADER_PROBE {
        return Err("Probe too short".into());
    }
    if &probe[0..4] != MAGIC {
        return Err("Invalid magic".into());
    }
    let pred_count = u16::from_le_bytes(
        probe[5..7].try_into().map_err(|_| "Failed to read pred count")?,
    ) as usize;
    Ok(15 + 12 * pred_count)
}

/// Serialize resource metadata to bytes.
pub fn serialize_resource_meta(meta: &[ResourceMeta]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    for rm in meta {
        buf.extend_from_slice(&rm.dict_id.to_le_bytes());
        let name_bytes = rm.name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        let slug_bytes = rm.slug.as_bytes();
        buf.extend_from_slice(&(slug_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(slug_bytes);
        let model_bytes = rm.model.as_bytes();
        buf.extend_from_slice(&(model_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(model_bytes);
    }
    buf
}

/// Parse resource metadata from bytes.
pub fn parse_resource_meta(data: &[u8]) -> Result<Vec<ResourceMeta>, String> {
    if data.len() < 4 {
        return Err("Resource metadata too short".into());
    }
    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let mut pos = 4;
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        if pos + 4 > data.len() {
            return Err("Resource metadata truncated (dict_id)".into());
        }
        let dict_id = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let name = read_length_prefixed_string(data, &mut pos)?;
        let slug = read_length_prefixed_string(data, &mut pos)?;
        let model = read_length_prefixed_string(data, &mut pos)?;

        result.push(ResourceMeta { dict_id, name, slug, model });
    }
    Ok(result)
}

fn read_length_prefixed_string(data: &[u8], pos: &mut usize) -> Result<String, String> {
    if *pos + 2 > data.len() {
        return Err("Resource metadata truncated (string length)".into());
    }
    let len = u16::from_le_bytes(data[*pos..*pos + 2].try_into().unwrap()) as usize;
    *pos += 2;
    if *pos + len > data.len() {
        return Err("Resource metadata truncated (string data)".into());
    }
    let s = std::str::from_utf8(&data[*pos..*pos + len])
        .map_err(|e| format!("Invalid UTF-8 in resource metadata: {}", e))?
        .to_string();
    *pos += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_and_parse_roundtrip() {
        let mut blocks = vec![
            PredicateBlock {
                pred_id: 10,
                records: vec![
                    PageRecord { subject_id: 1, object_val: 100 },
                    PageRecord { subject_id: 2, object_val: 200 },
                    PageRecord { subject_id: 3, object_val: 200 },
                ],
            },
            PredicateBlock {
                pred_id: 5,
                records: vec![
                    PageRecord { subject_id: 1, object_val: 50 },
                ],
            },
        ];

        let bytes = write_page_file(&mut blocks, &[]);
        let header = parse_page_header(&bytes).unwrap();

        assert_eq!(header.entries.len(), 2);
        // Blocks sorted by pred_id
        assert_eq!(header.entries[0].pred_id, 5);
        assert_eq!(header.entries[0].record_count, 1);
        assert_eq!(header.entries[1].pred_id, 10);
        assert_eq!(header.entries[1].record_count, 3);

        // Parse block for pred_id=10
        let (start, end) = header.predicate_byte_range(10).unwrap();
        let records = parse_records(&bytes[start as usize..end as usize]);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].object_val, 100);
        assert_eq!(records[1].object_val, 200);
    }

    #[test]
    fn test_write_and_parse_with_meta_roundtrip() {
        let mut blocks = vec![
            PredicateBlock {
                pred_id: 10,
                records: vec![
                    PageRecord { subject_id: 1, object_val: 100 },
                    PageRecord { subject_id: 2, object_val: 200 },
                ],
            },
        ];

        let meta = vec![
            ResourceMeta {
                dict_id: 42,
                name: "Test Resource".to_string(),
                slug: "test-resource".to_string(),
                model: "Heritage Place".to_string(),
            },
            ResourceMeta {
                dict_id: 99,
                name: "Another".to_string(),
                slug: "another".to_string(),
                model: "Monument".to_string(),
            },
        ];

        let bytes = write_page_file(&mut blocks, &meta);
        let header = parse_page_header(&bytes).unwrap();

        assert_eq!(header.entries.len(), 1);
        assert_eq!(header.entries[0].pred_id, 10);
        assert_eq!(header.entries[0].record_count, 2);

        // Verify predicate block still works
        let (start, end) = header.predicate_byte_range(10).unwrap();
        let records = parse_records(&bytes[start as usize..end as usize]);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].object_val, 100);

        // Verify resource metadata
        let (meta_offset, meta_size) = header.resource_meta_range.unwrap();
        let meta_data = &bytes[meta_offset as usize..(meta_offset + meta_size) as usize];
        let parsed_meta = parse_resource_meta(meta_data).unwrap();
        assert_eq!(parsed_meta.len(), 2);
        assert_eq!(parsed_meta[0].dict_id, 42);
        assert_eq!(parsed_meta[0].name, "Test Resource");
        assert_eq!(parsed_meta[0].slug, "test-resource");
        assert_eq!(parsed_meta[0].model, "Heritage Place");
        assert_eq!(parsed_meta[1].dict_id, 99);
        assert_eq!(parsed_meta[1].name, "Another");
    }

    #[test]
    fn test_v3_empty_metadata() {
        let mut blocks = vec![
            PredicateBlock {
                pred_id: 1,
                records: vec![PageRecord { subject_id: 0, object_val: 0 }],
            },
        ];

        let bytes = write_page_file(&mut blocks, &[]);
        let header = parse_page_header(&bytes).unwrap();

        // Empty metadata section still has 4 bytes (count=0), so range is present
        let (meta_offset, meta_size) = header.resource_meta_range.unwrap();
        let meta_data = &bytes[meta_offset as usize..(meta_offset + meta_size) as usize];
        let parsed = parse_resource_meta(meta_data).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_resource_meta_roundtrip() {
        let meta = vec![
            ResourceMeta {
                dict_id: 1,
                name: "Héllo Wörld".to_string(),
                slug: "hello-world".to_string(),
                model: "Test".to_string(),
            },
        ];
        let bytes = serialize_resource_meta(&meta);
        let parsed = parse_resource_meta(&bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].dict_id, 1);
        assert_eq!(parsed[0].name, "Héllo Wörld");
        assert_eq!(parsed[0].slug, "hello-world");
        assert_eq!(parsed[0].model, "Test");
    }

    #[test]
    fn test_binary_search() {
        let records = vec![
            PageRecord { subject_id: 1, object_val: 10 },
            PageRecord { subject_id: 2, object_val: 20 },
            PageRecord { subject_id: 3, object_val: 20 },
            PageRecord { subject_id: 4, object_val: 30 },
        ];

        let (lo, hi) = binary_search_object(&records, 20);
        assert_eq!((lo, hi), (1, 3));

        let (lo, hi) = binary_search_object(&records, 15);
        assert_eq!((lo, hi), (1, 1)); // empty range

        let (lo, hi) = range_search_object(&records, 15, 25);
        assert_eq!((lo, hi), (1, 3)); // includes the 20s
    }

    #[test]
    fn test_header_probe() {
        let mut blocks = vec![
            PredicateBlock {
                pred_id: 1,
                records: vec![PageRecord { subject_id: 0, object_val: 0 }],
            },
            PredicateBlock {
                pred_id: 2,
                records: vec![],
            },
        ];
        let bytes = write_page_file(&mut blocks, &[]);
        let size = full_header_size(&bytes[..MIN_HEADER_PROBE]).unwrap();
        assert_eq!(size, 15 + 12 * 2);
    }
}
