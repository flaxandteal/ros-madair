// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Per-page tile content file for full-fidelity tile serving via Range requests.
//!
//! Each tile content file stores the complete (MessagePack-encoded) tile data
//! for every resource assigned to that page.  The format parallels
//! [`crate::page_file`] — one file per page, binary header + body — but serves
//! a different purpose: lossless tile retrieval rather than quantized querying.
//!
//! ## File layout
//!
//! ```text
//! [header]
//!   magic: [u8; 4]           = b"RMTL"
//!   version: u8              = 1
//!   entry_count: u32 (LE)
//!   entries: [(subject_id: u32, blob_offset: u32, blob_size: u32)]  // 12 bytes each
//!
//! [body]
//!   blob_0: [u8; N0]   — MessagePack-encoded Vec<StaticTile> for resource 0
//!   blob_1: [u8; N1]
//!   ...
//! ```
//!
//! Header size = 9 + 12 × entry_count.  Entries are sorted by `subject_id`
//! for binary search.
//!
//! The client fetches the header via a Range request, binary-searches for the
//! target `subject_id`, then fetches the corresponding blob with a second
//! Range request.

pub const TILE_MAGIC: &[u8; 4] = b"RMTL";
pub const TILE_VERSION: u8 = 1;

/// Minimum bytes needed to determine the full header size (magic + version + entry_count).
pub const TILE_MIN_HEADER_PROBE: usize = 9;

const ENTRY_SIZE: usize = 12;

/// A single entry in the tile content header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileContentEntry {
    /// Dictionary ID of the resource (subject).
    pub subject_id: u32,
    /// Byte offset from start of file where this resource's blob begins.
    pub blob_offset: u32,
    /// Size in bytes of this resource's blob.
    pub blob_size: u32,
}

/// Parsed tile content file header.
#[derive(Debug, Clone)]
pub struct TileContentHeader {
    pub entries: Vec<TileContentEntry>,
}

impl TileContentHeader {
    /// Total header size in bytes: 9 + 12 × entry_count.
    pub fn header_size(&self) -> usize {
        9 + ENTRY_SIZE * self.entries.len()
    }

    /// Find the entry for a given subject_id via binary search.
    pub fn entry_for_subject(&self, subject_id: u32) -> Option<&TileContentEntry> {
        self.entries
            .binary_search_by_key(&subject_id, |e| e.subject_id)
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// Calculate the full header size from the first [`TILE_MIN_HEADER_PROBE`] bytes.
pub fn tile_full_header_size(probe: &[u8]) -> Result<usize, String> {
    if probe.len() < TILE_MIN_HEADER_PROBE {
        return Err("Tile probe too short".into());
    }
    if &probe[0..4] != TILE_MAGIC {
        return Err(format!(
            "Invalid tile file magic: expected {:?}, got {:?}",
            TILE_MAGIC,
            &probe[0..4]
        ));
    }
    if probe[4] != TILE_VERSION {
        return Err(format!(
            "Unsupported tile file version: {} (expected {})",
            probe[4], TILE_VERSION
        ));
    }
    let entry_count = u32::from_le_bytes(
        probe[5..9]
            .try_into()
            .map_err(|_| "Failed to read entry_count")?,
    ) as usize;
    Ok(9 + ENTRY_SIZE * entry_count)
}

/// Parse a tile content header from bytes.
pub fn parse_tile_content_header(data: &[u8]) -> Result<TileContentHeader, String> {
    if data.len() < TILE_MIN_HEADER_PROBE {
        return Err("Tile file too short for header".into());
    }
    if &data[0..4] != TILE_MAGIC {
        return Err("Invalid tile file magic".into());
    }
    if data[4] != TILE_VERSION {
        return Err(format!(
            "Unsupported tile file version: {} (expected {})",
            data[4], TILE_VERSION
        ));
    }
    let entry_count = u32::from_le_bytes(
        data[5..9]
            .try_into()
            .map_err(|_| "Failed to read entry_count")?,
    ) as usize;

    let needed = 9 + ENTRY_SIZE * entry_count;
    if data.len() < needed {
        return Err(format!(
            "Tile header truncated: need {} bytes, got {}",
            needed,
            data.len()
        ));
    }

    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let base = 9 + ENTRY_SIZE * i;
        let subject_id = u32::from_le_bytes(data[base..base + 4].try_into().unwrap());
        let blob_offset = u32::from_le_bytes(data[base + 4..base + 8].try_into().unwrap());
        let blob_size = u32::from_le_bytes(data[base + 8..base + 12].try_into().unwrap());
        entries.push(TileContentEntry {
            subject_id,
            blob_offset,
            blob_size,
        });
    }

    Ok(TileContentHeader { entries })
}

/// Write a complete tile content file.
///
/// `entries` must be `(subject_id, msgpack_blob)` pairs, **already sorted by
/// subject_id**.  The caller is responsible for sorting.
pub fn write_tile_content_file(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let entry_count = entries.len();
    let header_size = 9 + ENTRY_SIZE * entry_count;

    // Calculate blob offsets
    let mut blob_offset = header_size as u32;
    let mut header_entries = Vec::with_capacity(entry_count);
    for (subject_id, blob) in entries {
        header_entries.push(TileContentEntry {
            subject_id: *subject_id,
            blob_offset,
            blob_size: blob.len() as u32,
        });
        blob_offset += blob.len() as u32;
    }

    let total_size = blob_offset as usize;
    let mut buf = Vec::with_capacity(total_size);

    // Write header
    buf.extend_from_slice(TILE_MAGIC);
    buf.push(TILE_VERSION);
    buf.extend_from_slice(&(entry_count as u32).to_le_bytes());

    for entry in &header_entries {
        buf.extend_from_slice(&entry.subject_id.to_le_bytes());
        buf.extend_from_slice(&entry.blob_offset.to_le_bytes());
        buf.extend_from_slice(&entry.blob_size.to_le_bytes());
    }

    // Write blobs
    for (_subject_id, blob) in entries {
        buf.extend_from_slice(blob);
    }

    debug_assert_eq!(buf.len(), total_size);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let data = write_tile_content_file(&[]);
        assert_eq!(data.len(), 9); // header only
        let header = parse_tile_content_header(&data).unwrap();
        assert!(header.entries.is_empty());
        assert_eq!(header.header_size(), 9);
    }

    #[test]
    fn test_roundtrip_single() {
        let blob = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let entries = vec![(42u32, blob.clone())];
        let data = write_tile_content_file(&entries);

        let header = parse_tile_content_header(&data).unwrap();
        assert_eq!(header.entries.len(), 1);
        assert_eq!(header.header_size(), 9 + 12);

        let entry = header.entry_for_subject(42).unwrap();
        assert_eq!(entry.subject_id, 42);
        assert_eq!(entry.blob_size, 4);

        let extracted = &data[entry.blob_offset as usize..(entry.blob_offset + entry.blob_size) as usize];
        assert_eq!(extracted, &blob);
    }

    #[test]
    fn test_roundtrip_multiple() {
        let entries = vec![
            (10u32, vec![1, 2, 3]),
            (20u32, vec![4, 5]),
            (30u32, vec![6, 7, 8, 9]),
        ];
        let data = write_tile_content_file(&entries);

        let header = parse_tile_content_header(&data).unwrap();
        assert_eq!(header.entries.len(), 3);
        assert_eq!(header.header_size(), 9 + 36);

        // Binary search finds each entry
        for (subject_id, blob) in &entries {
            let entry = header.entry_for_subject(*subject_id).unwrap();
            let extracted =
                &data[entry.blob_offset as usize..(entry.blob_offset + entry.blob_size) as usize];
            assert_eq!(extracted, blob.as_slice());
        }

        // Missing subject_id returns None
        assert!(header.entry_for_subject(15).is_none());
        assert!(header.entry_for_subject(99).is_none());
    }

    #[test]
    fn test_header_probe() {
        let entries = vec![(1u32, vec![0xAA]), (2u32, vec![0xBB, 0xCC])];
        let data = write_tile_content_file(&entries);

        let size = tile_full_header_size(&data[..TILE_MIN_HEADER_PROBE]).unwrap();
        assert_eq!(size, 9 + 24); // 2 entries × 12 bytes each
        assert_eq!(size, header_size_for_count(2));
    }

    #[test]
    fn test_invalid_magic() {
        let data = vec![0, 0, 0, 0, 1, 0, 0, 0, 0];
        assert!(parse_tile_content_header(&data).is_err());
        assert!(tile_full_header_size(&data).is_err());
    }

    #[test]
    fn test_truncated_header() {
        // Valid magic/version/count but not enough entry bytes
        let mut data = Vec::new();
        data.extend_from_slice(TILE_MAGIC);
        data.push(TILE_VERSION);
        data.extend_from_slice(&2u32.to_le_bytes()); // claims 2 entries
        // but only provide 5 bytes of entry data (need 24)
        data.extend_from_slice(&[0; 5]);
        assert!(parse_tile_content_header(&data).is_err());
    }

    fn header_size_for_count(n: usize) -> usize {
        9 + 12 * n
    }
}
