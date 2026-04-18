// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Resource-to-page mapping for O(1) lookup by dictionary ID.
//!
//! ## Binary format (`resource_map.bin`)
//!
//! ```text
//! magic: "RMRM" (4 bytes)
//! entry_count: u32 LE (= dictionary.len())
//! entries: [u16 LE × entry_count]  — page_id per dict entry, 0xFFFF = not a resource
//! ```

use std::collections::HashMap;

use crate::uri::resource_prefix;
use crate::Dictionary;

const MAGIC: &[u8; 4] = b"RMRM";
const NOT_A_RESOURCE: u16 = 0xFFFF;

/// Maps dictionary IDs to page IDs for resource terms.
pub struct ResourceMap {
    entries: Vec<u16>, // dict_id → page_id (NOT_A_RESOURCE if not a resource)
}

impl ResourceMap {
    /// Build from a dictionary and a resource_id → page_id mapping.
    ///
    /// `resource_to_page` uses raw resource IDs (without URI prefix).
    /// `base_uri` is the prefix used to form resource URIs in the dictionary
    /// (e.g. `"https://example.org/"`), so that dict terms like
    /// `"{base_uri}resource/{id}"` are matched.
    pub fn build(
        dict: &Dictionary,
        resource_to_page: &HashMap<String, u32>,
        base_uri: &str,
    ) -> Self {
        let prefix = resource_prefix(base_uri);
        let count = dict.len();
        let mut entries = vec![NOT_A_RESOURCE; count];

        for (i, _) in (0..count).enumerate() {
            if let Some(term) = dict.resolve(i as u32) {
                if let Some(resource_id) = term.strip_prefix(&prefix) {
                    if let Some(&page_id) = resource_to_page.get(resource_id) {
                        if page_id < NOT_A_RESOURCE as u32 {
                            entries[i] = page_id as u16;
                        }
                    }
                }
            }
        }

        Self { entries }
    }

    /// Serialize to binary format.
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.entries.len();
        let mut buf = Vec::with_capacity(4 + 4 + count * 2);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(count as u32).to_le_bytes());
        for &page_id in &self.entries {
            buf.extend_from_slice(&page_id.to_le_bytes());
        }
        buf
    }

    /// Deserialize from binary format.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < 8 {
            return Err("ResourceMap data too short".into());
        }
        if &data[0..4] != MAGIC {
            return Err("Invalid ResourceMap magic".into());
        }
        let count =
            u32::from_le_bytes(data[4..8].try_into().map_err(|_| "Failed to read count")?)
                as usize;
        let expected = 8 + count * 2;
        if data.len() < expected {
            return Err(format!(
                "ResourceMap truncated: need {} bytes, got {}",
                expected,
                data.len()
            ));
        }

        let entries = (0..count)
            .map(|i| {
                let offset = 8 + i * 2;
                u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
            })
            .collect();

        Ok(Self { entries })
    }

    /// Get the page ID for a dictionary entry, or None if not a resource.
    pub fn page_for(&self, dict_id: u32) -> Option<u32> {
        self.entries.get(dict_id as usize).and_then(|&p| {
            if p == NOT_A_RESOURCE {
                None
            } else {
                Some(p as u32)
            }
        })
    }

    /// Check if a dictionary entry is a resource.
    pub fn is_resource(&self, dict_id: u32) -> bool {
        self.page_for(dict_id).is_some()
    }

    /// Number of entries (should match dictionary size).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let mut dict = Dictionary::new();
        dict.intern("https://example.org/resource/res-1");
        dict.intern("https://example.org/node/some_pred");
        dict.intern("https://example.org/resource/res-2");

        let mut r2p = HashMap::new();
        r2p.insert("res-1".to_string(), 0u32);
        r2p.insert("res-2".to_string(), 3u32);

        let rmap = ResourceMap::build(&dict, &r2p, "https://example.org/");
        assert_eq!(rmap.page_for(0), Some(0));
        assert_eq!(rmap.page_for(1), None); // predicate, not a resource
        assert_eq!(rmap.page_for(2), Some(3));

        let bytes = rmap.to_bytes();
        let rmap2 = ResourceMap::from_bytes(&bytes).unwrap();
        assert_eq!(rmap2.page_for(0), Some(0));
        assert_eq!(rmap2.page_for(1), None);
        assert_eq!(rmap2.page_for(2), Some(3));
        assert_eq!(rmap2.len(), 3);
    }

    #[test]
    fn test_empty() {
        let dict = Dictionary::new();
        let rmap = ResourceMap::build(&dict, &HashMap::new(), "https://example.org/");
        assert!(rmap.is_empty());
        let bytes = rmap.to_bytes();
        let rmap2 = ResourceMap::from_bytes(&bytes).unwrap();
        assert!(rmap2.is_empty());
    }

    #[test]
    fn test_out_of_bounds() {
        let mut dict = Dictionary::new();
        dict.intern("https://example.org/resource/r1");
        let rmap = ResourceMap::build(&dict, &HashMap::new(), "https://example.org/");
        assert_eq!(rmap.page_for(999), None);
    }
}
