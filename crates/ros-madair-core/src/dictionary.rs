// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! URI/literal ↔ integer ID dictionary encoding.
//!
//! Maps URIs and literals to compact integer IDs for efficient storage in
//! page files and summary quads. The dictionary is built at index time and
//! loaded once by the browser client.
//!
//! Three formats are supported:
//! - **Legacy** (v0): sequential `[len, bytes]` pairs. Requires HashMap on load.
//! - **Indexed** (v1, magic `RMDC`): sorted index + offset table for O(log n)
//!   binary search directly on the raw bytes, no HashMap needed.
//! - **Prefix-compressed** (v2, magic `RMDC`): like v1 but with a prefix table
//!   that factors out common URI prefixes, reducing size by 60-80% for RDF data.
//!
//! # Deprecation schedule
//!
//! v1 backwards compatibility is retained for existing indices but **must be
//! removed before 0.1.0-alpha.16**. All consumers should rebuild indices with
//! the current writer (which emits v2) before upgrading past alpha.15.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Magic bytes for the indexed dictionary format.
const DICT_MAGIC: &[u8; 4] = b"RMDC";
/// Current format version (prefix-compressed).
const DICT_VERSION: u8 = 2;

/// Trait for types that can look up terms by string or resolve IDs to strings.
///
/// Implemented by both [`Dictionary`] (build-time, HashMap-backed) and
/// [`IndexedDictionary`] (client-time, binary-search on raw bytes).
pub trait DictLookup {
    /// Look up an ID by term string.
    fn lookup(&self, term: &str) -> Option<u32>;
    /// Resolve an ID to its term string.
    fn resolve(&self, id: u32) -> Option<&str>;
    /// Number of entries.
    fn len(&self) -> usize;
    /// Whether the dictionary is empty.
    fn is_empty(&self) -> bool { self.len() == 0 }
}

/// Bidirectional dictionary mapping terms (URIs/literals) to integer IDs.
///
/// Used at build time for interning terms. At query time, prefer
/// [`IndexedDictionary`] which operates directly on the binary blob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dictionary {
    /// term string → integer ID
    term_to_id: HashMap<String, u32>,
    /// integer ID → term string (indexed by position)
    id_to_term: Vec<String>,
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or insert a term, returning its integer ID.
    pub fn intern(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.term_to_id.get(term) {
            return id;
        }
        let id = self.id_to_term.len() as u32;
        self.id_to_term.push(term.to_string());
        self.term_to_id.insert(term.to_string(), id);
        id
    }

    /// Look up a term by ID.
    pub fn resolve(&self, id: u32) -> Option<&str> {
        self.id_to_term.get(id as usize).map(|s| s.as_str())
    }

    /// Look up an ID by term.
    pub fn lookup(&self, term: &str) -> Option<u32> {
        self.term_to_id.get(term).copied()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.id_to_term.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_term.is_empty()
    }

    /// Serialize to the current best format (v2, prefix-compressed).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_bytes_indexed()
    }

    /// Serialize to the prefix-compressed indexed format (v2).
    ///
    /// Format (RMDC v2):
    /// ```text
    /// [0..4]      magic "RMDC"
    /// [4]         version (2)
    /// [5..9]      count N (u32 LE)
    /// [9]         prefix_count P (u8)
    /// [10..]      prefix_table: P × (len: u16 LE, UTF-8 bytes)
    /// [..]        id_entries: N × (prefix_id: u8, suffix_offset: u32 LE) — 5 bytes each, ID order
    /// [..]        sorted_index: N × u32 LE — IDs in term-sorted order
    /// [..]        suffix_blob: packed suffix strings in ID order
    /// ```
    pub fn to_bytes_indexed(&self) -> Vec<u8> {
        let n = self.id_to_term.len();
        if n == 0 {
            let mut buf = Vec::with_capacity(10);
            buf.extend_from_slice(DICT_MAGIC);
            buf.push(DICT_VERSION);
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.push(0u8); // prefix_count = 0
            return buf;
        }

        // Detect common prefixes: split each term at last '#' or '/'
        let prefixes = detect_prefixes(&self.id_to_term);

        // Assign each term its best prefix
        let mut term_prefix_ids: Vec<u8> = Vec::with_capacity(n);
        let mut suffix_blob = Vec::new();
        let mut suffix_offsets: Vec<u32> = Vec::with_capacity(n);

        for term in &self.id_to_term {
            let (pid, suffix) = best_prefix(term, &prefixes);
            term_prefix_ids.push(pid);
            suffix_offsets.push(suffix_blob.len() as u32);
            suffix_blob.extend_from_slice(suffix.as_bytes());
        }

        // Build sorted index: IDs sorted by full term
        let mut sorted_ids: Vec<u32> = (0..n as u32).collect();
        sorted_ids.sort_by(|&a, &b| {
            self.id_to_term[a as usize].cmp(&self.id_to_term[b as usize])
        });

        // Write binary
        let mut buf = Vec::new();

        // Header
        buf.extend_from_slice(DICT_MAGIC);
        buf.push(DICT_VERSION);
        buf.extend_from_slice(&(n as u32).to_le_bytes());
        buf.push(prefixes.len() as u8);

        // Prefix table
        for prefix in &prefixes {
            buf.extend_from_slice(&(prefix.len() as u16).to_le_bytes());
            buf.extend_from_slice(prefix.as_bytes());
        }

        // ID entries: (prefix_id: u8, suffix_offset: u32) × N
        for i in 0..n {
            buf.push(term_prefix_ids[i]);
            buf.extend_from_slice(&suffix_offsets[i].to_le_bytes());
        }

        // Sorted index: N × u32
        for &id in &sorted_ids {
            buf.extend_from_slice(&id.to_le_bytes());
        }

        // Suffix blob
        buf.extend_from_slice(&suffix_blob);

        buf
    }

    /// Deserialize from any supported format (v0 legacy, v1 indexed, v2 prefix).
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() >= 5 && &data[0..4] == DICT_MAGIC {
            return Self::from_bytes_indexed(data);
        }
        Self::from_bytes_legacy(data)
    }

    /// Deserialize from the legacy sequential format.
    fn from_bytes_legacy(data: &[u8]) -> Result<Self, String> {
        if data.len() < 4 {
            return Err("Dictionary data too short".into());
        }

        let count =
            u32::from_le_bytes(data[0..4].try_into().map_err(|_| "Failed to read count")?)
                as usize;

        let mut offset = 4;
        let mut dict = Dictionary::new();

        for _ in 0..count {
            if offset + 4 > data.len() {
                return Err("Unexpected end of dictionary data".into());
            }
            let len = u32::from_le_bytes(
                data[offset..offset + 4]
                    .try_into()
                    .map_err(|_| "Failed to read string length")?,
            ) as usize;
            offset += 4;

            if offset + len > data.len() {
                return Err("Unexpected end of dictionary string data".into());
            }
            let term = std::str::from_utf8(&data[offset..offset + len])
                .map_err(|e| format!("Invalid UTF-8 in dictionary: {}", e))?;
            dict.intern(term);
            offset += len;
        }

        Ok(dict)
    }

    /// Deserialize from an indexed/prefix format into a full Dictionary.
    fn from_bytes_indexed(data: &[u8]) -> Result<Self, String> {
        let indexed = IndexedDictionary::from_bytes(data.to_vec())?;
        let mut dict = Dictionary::new();
        for id in 0..indexed.len() as u32 {
            if let Some(term) = indexed.resolve(id) {
                dict.intern(term);
            }
        }
        Ok(dict)
    }
}

impl DictLookup for Dictionary {
    fn lookup(&self, term: &str) -> Option<u32> {
        self.lookup(term)
    }
    fn resolve(&self, id: u32) -> Option<&str> {
        self.resolve(id)
    }
    fn len(&self) -> usize {
        self.len()
    }
}

/// Zero-copy dictionary that operates directly on the serialized bytes.
///
/// Supports O(1) `resolve(id)` and O(log n) `lookup(term)` without
/// building a HashMap. Handles both v1 (plain) and v2 (prefix-compressed).
#[derive(Clone)]
pub struct IndexedDictionary {
    /// Raw binary data (owned).
    data: Vec<u8>,
    /// Format version (1 or 2).
    version: u8,
    /// Number of entries.
    count: u32,
    /// Parsed prefix strings (v2 only; empty for v1).
    prefixes: Vec<String>,
    /// Byte offset where id_entries / id_offsets start.
    id_entries_start: usize,
    /// Byte offset where sorted index starts.
    sorted_index_start: usize,
    /// Byte offset where the suffix/string blob starts.
    blob_start: usize,
    /// Scratch buffer for prefix+suffix concatenation during lookups.
    /// Using a Cell/RefCell would be needed in non-WASM; for simplicity
    /// we reconstruct per-call (small cost vs HashMap).
    _phantom: (),
}

impl IndexedDictionary {
    /// Parse an indexed dictionary from raw bytes. Supports v1 and v2.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, String> {
        if data.len() < 9 {
            return Err("IndexedDictionary: data too short".into());
        }
        if &data[0..4] != DICT_MAGIC {
            return Err("IndexedDictionary: bad magic".into());
        }
        let version = data[4];
        match version {
            // TODO(alpha.16): remove v1 support — all indices must be rebuilt as v2
            1 => Self::parse_v1(data),
            2 => Self::parse_v2(data),
            _ => Err(format!("IndexedDictionary: unsupported version {}", version)),
        }
    }

    /// Parse v1 indexed format (no prefix compression).
    ///
    /// DEPRECATION: v1 backwards compatibility must be removed before alpha.16.
    /// All indices should be rebuilt with v2 by then.
    fn parse_v1(data: Vec<u8>) -> Result<Self, String> {
        let count = u32::from_le_bytes(
            data[5..9].try_into().map_err(|_| "Failed to read count")?,
        );
        let n = count as usize;
        let id_entries_start = 9;
        let sorted_index_start = id_entries_start + n * 4;
        let blob_start = sorted_index_start + n * 8;

        if blob_start > data.len() {
            return Err("IndexedDictionary v1: data truncated".into());
        }

        Ok(Self {
            data,
            version: 1,
            count,
            prefixes: Vec::new(),
            id_entries_start,
            sorted_index_start,
            blob_start,
            _phantom: (),
        })
    }

    fn parse_v2(data: Vec<u8>) -> Result<Self, String> {
        let count = u32::from_le_bytes(
            data[5..9].try_into().map_err(|_| "Failed to read count")?,
        );
        let n = count as usize;
        let prefix_count = data[9] as usize;

        // Parse prefix table
        let mut offset = 10;
        let mut prefixes = Vec::with_capacity(prefix_count);
        for _ in 0..prefix_count {
            if offset + 2 > data.len() {
                return Err("IndexedDictionary v2: prefix table truncated".into());
            }
            let len = u16::from_le_bytes(
                data[offset..offset + 2].try_into().map_err(|_| "prefix len")?,
            ) as usize;
            offset += 2;
            if offset + len > data.len() {
                return Err("IndexedDictionary v2: prefix string truncated".into());
            }
            let s = std::str::from_utf8(&data[offset..offset + len])
                .map_err(|e| format!("Invalid UTF-8 in prefix: {}", e))?;
            prefixes.push(s.to_string());
            offset += len;
        }

        let id_entries_start = offset;
        let sorted_index_start = id_entries_start + n * 5; // 5 bytes per entry
        let blob_start = sorted_index_start + n * 4; // 4 bytes per sorted entry

        if blob_start > data.len() {
            return Err("IndexedDictionary v2: data truncated".into());
        }

        Ok(Self {
            data,
            version: 2,
            count,
            prefixes,
            id_entries_start,
            sorted_index_start,
            blob_start,
            _phantom: (),
        })
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Resolve an ID to its term string. O(1).
    ///
    /// For v2, reconstructs prefix+suffix into a new String and leaks it
    /// into a &str. This is acceptable because resolve() is called
    /// infrequently (result display), and the IndexedDictionary lives for
    /// the lifetime of the app.
    pub fn resolve(&self, id: u32) -> Option<&str> {
        if id >= self.count {
            return None;
        }
        match self.version {
            1 => self.resolve_v1(id),
            2 => self.resolve_v2(id),
            _ => None,
        }
    }

    fn resolve_v1(&self, id: u32) -> Option<&str> {
        let off_pos = self.id_entries_start + (id as usize) * 4;
        let blob_offset = u32::from_le_bytes(
            self.data[off_pos..off_pos + 4].try_into().ok()?,
        ) as usize;

        let end = if (id + 1) < self.count {
            let next_pos = off_pos + 4;
            u32::from_le_bytes(
                self.data[next_pos..next_pos + 4].try_into().ok()?,
            ) as usize
        } else {
            self.data.len() - self.blob_start
        };

        let abs_start = self.blob_start + blob_offset;
        let abs_end = self.blob_start + end;
        std::str::from_utf8(&self.data[abs_start..abs_end]).ok()
    }

    fn resolve_v2(&self, id: u32) -> Option<&str> {
        let (prefix_id, suffix) = self.id_entry_v2(id)?;
        if self.prefixes.is_empty() || prefix_id == 255 {
            // No prefix — suffix is the full term, return directly from blob
            return Some(suffix);
        }
        let prefix = self.prefixes.get(prefix_id as usize)?;
        if prefix.is_empty() {
            return Some(suffix);
        }
        // Reconstruct: allocate and leak for lifetime
        // This is intentional — IndexedDictionary lives for app lifetime
        let full = format!("{}{}", prefix, suffix);
        Some(Box::leak(full.into_boxed_str()))
    }

    /// Get (prefix_id, suffix_str) for a v2 entry.
    fn id_entry_v2(&self, id: u32) -> Option<(u8, &str)> {
        let entry_pos = self.id_entries_start + (id as usize) * 5;
        let prefix_id = self.data[entry_pos];
        let suffix_offset = u32::from_le_bytes(
            self.data[entry_pos + 1..entry_pos + 5].try_into().ok()?,
        ) as usize;

        // Find suffix end
        let suffix_end = if (id + 1) < self.count {
            let next_pos = self.id_entries_start + ((id + 1) as usize) * 5;
            u32::from_le_bytes(
                self.data[next_pos + 1..next_pos + 5].try_into().ok()?,
            ) as usize
        } else {
            self.data.len() - self.blob_start
        };

        let abs_start = self.blob_start + suffix_offset;
        let abs_end = self.blob_start + suffix_end;
        let suffix = std::str::from_utf8(&self.data[abs_start..abs_end]).ok()?;
        Some((prefix_id, suffix))
    }

    /// Look up a term's ID by binary search. O(log n).
    pub fn lookup(&self, term: &str) -> Option<u32> {
        if self.count == 0 {
            return None;
        }
        match self.version {
            1 => self.lookup_v1(term),
            2 => self.lookup_v2(term),
            _ => None,
        }
    }

    fn lookup_v1(&self, term: &str) -> Option<u32> {
        let n = self.count as usize;
        let mut lo = 0usize;
        let mut hi = n;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry_pos = self.sorted_index_start + mid * 8;
            let blob_offset = u32::from_le_bytes(
                self.data[entry_pos..entry_pos + 4].try_into().ok()?,
            ) as usize;
            let id = u32::from_le_bytes(
                self.data[entry_pos + 4..entry_pos + 8].try_into().ok()?,
            );

            let mid_term = self.term_at_v1(blob_offset, id)?;
            match mid_term.cmp(term) {
                std::cmp::Ordering::Equal => return Some(id),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    fn term_at_v1(&self, blob_offset: usize, id: u32) -> Option<&str> {
        let abs_start = self.blob_start + blob_offset;
        let end = if (id + 1) < self.count {
            let next_off_pos = self.id_entries_start + ((id + 1) as usize) * 4;
            u32::from_le_bytes(
                self.data[next_off_pos..next_off_pos + 4].try_into().ok()?,
            ) as usize
        } else {
            self.data.len() - self.blob_start
        };
        let abs_end = self.blob_start + end;
        std::str::from_utf8(&self.data[abs_start..abs_end]).ok()
    }

    fn lookup_v2(&self, term: &str) -> Option<u32> {
        let n = self.count as usize;
        let mut lo = 0usize;
        let mut hi = n;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let sorted_pos = self.sorted_index_start + mid * 4;
            let id = u32::from_le_bytes(
                self.data[sorted_pos..sorted_pos + 4].try_into().ok()?,
            );

            let cmp = self.compare_term_v2(id, term)?;
            match cmp {
                std::cmp::Ordering::Equal => return Some(id),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    /// Compare the reconstructed term for `id` against `target` without allocating.
    fn compare_term_v2(&self, id: u32, target: &str) -> Option<std::cmp::Ordering> {
        let (prefix_id, suffix) = self.id_entry_v2(id)?;
        let prefix = if prefix_id == 255 || self.prefixes.is_empty() {
            ""
        } else {
            self.prefixes.get(prefix_id as usize).map(|s| s.as_str()).unwrap_or("")
        };

        // Compare prefix portion first
        let target_bytes = target.as_bytes();
        let prefix_bytes = prefix.as_bytes();
        let suffix_bytes = suffix.as_bytes();

        // Conceptually comparing (prefix + suffix) vs target
        let prefix_len = prefix_bytes.len();
        let suffix_len = suffix_bytes.len();
        let total_len = prefix_len + suffix_len;
        let target_len = target_bytes.len();

        // Compare byte by byte: first prefix portion, then suffix
        let min_len = total_len.min(target_len);
        for i in 0..min_len {
            let our_byte = if i < prefix_len {
                prefix_bytes[i]
            } else {
                suffix_bytes[i - prefix_len]
            };
            let their_byte = target_bytes[i];
            match our_byte.cmp(&their_byte) {
                std::cmp::Ordering::Equal => continue,
                other => return Some(other),
            }
        }
        Some(total_len.cmp(&target_len))
    }
}

impl DictLookup for IndexedDictionary {
    fn lookup(&self, term: &str) -> Option<u32> {
        self.lookup(term)
    }
    fn resolve(&self, id: u32) -> Option<&str> {
        self.resolve(id)
    }
    fn len(&self) -> usize {
        self.len()
    }
}

// --- Prefix detection ---

/// Detect common prefixes for a set of terms.
/// Returns up to 254 prefixes (prefix_id 255 reserved for "no prefix").
fn detect_prefixes(terms: &[String]) -> Vec<String> {
    // Count frequency of URI prefixes (up to last '#' or '/')
    let mut prefix_counts: HashMap<&str, usize> = HashMap::new();
    for term in terms {
        if let Some(cut) = term.rfind(['#', '/']) {
            let prefix = &term[..=cut]; // include the delimiter
            *prefix_counts.entry(prefix).or_default() += 1;
        }
    }

    // Score by bytes saved: (count - 1) * prefix_len - overhead
    // overhead = 2 (len u16) + prefix_len (table entry) ≈ prefix_len + 2
    // net savings = (count - 1) * prefix_len - prefix_len - 2
    //            = prefix_len * (count - 2) - 2
    let mut scored: Vec<(&str, i64)> = prefix_counts
        .iter()
        .map(|(&prefix, &count)| {
            let savings = (prefix.len() as i64) * (count as i64 - 2) - 2;
            (prefix, savings)
        })
        .filter(|(_, s)| *s > 0)
        .collect();

    scored.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
    scored.truncate(254); // max 254 prefixes (255 = no prefix)

    scored.into_iter().map(|(p, _)| p.to_string()).collect()
}

/// Find the best matching prefix for a term. Returns (prefix_id, suffix).
/// If no prefix matches, returns (255, full_term).
fn best_prefix<'a>(term: &'a str, prefixes: &[String]) -> (u8, &'a str) {
    // Find longest matching prefix
    let mut best_id = 255u8;
    let mut best_len = 0usize;

    for (i, prefix) in prefixes.iter().enumerate() {
        if term.starts_with(prefix.as_str()) && prefix.len() > best_len {
            best_id = i as u8;
            best_len = prefix.len();
        }
    }

    if best_id == 255 {
        (255, term)
    } else {
        (best_id, &term[best_len..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intern_and_resolve() {
        let mut dict = Dictionary::new();
        let id1 = dict.intern("http://example.org/foo");
        let id2 = dict.intern("http://example.org/bar");
        let id1_again = dict.intern("http://example.org/foo");

        assert_eq!(id1, id1_again);
        assert_ne!(id1, id2);
        assert_eq!(dict.resolve(id1), Some("http://example.org/foo"));
        assert_eq!(dict.resolve(id2), Some("http://example.org/bar"));
        assert_eq!(dict.lookup("http://example.org/foo"), Some(id1));
        assert_eq!(dict.len(), 2);
    }

    #[test]
    fn test_binary_roundtrip() {
        let mut dict = Dictionary::new();
        dict.intern("http://example.org/foo");
        dict.intern("http://example.org/bar");
        dict.intern("hello world");

        let bytes = dict.to_bytes();
        let dict2 = Dictionary::from_bytes(&bytes).unwrap();

        assert_eq!(dict2.len(), 3);
        assert_eq!(dict2.resolve(0), Some("http://example.org/foo"));
        assert_eq!(dict2.resolve(1), Some("http://example.org/bar"));
        assert_eq!(dict2.resolve(2), Some("hello world"));
        assert_eq!(dict2.lookup("http://example.org/foo"), Some(0));
    }

    #[test]
    fn test_empty_dictionary() {
        let dict = Dictionary::new();
        let bytes = dict.to_bytes();
        let dict2 = Dictionary::from_bytes(&bytes).unwrap();
        assert_eq!(dict2.len(), 0);
        assert!(dict2.is_empty());
    }

    #[test]
    fn test_v2_prefix_compression() {
        let mut dict = Dictionary::new();
        // Many terms sharing a long prefix
        for i in 0..100 {
            dict.intern(&format!("http://example.org/ontology/goidelic#{}", i));
        }
        dict.intern("http://www.w3.org/ns/lemon/ontolex#LexicalEntry");
        dict.intern("http://www.w3.org/ns/lemon/ontolex#Form");
        dict.intern("plain-literal");

        let v2_bytes = dict.to_bytes_indexed();
        assert_eq!(&v2_bytes[0..4], b"RMDC");
        assert_eq!(v2_bytes[4], 2); // version 2

        // v2 should be significantly smaller than naive storage
        let naive_size: usize = dict.id_to_term.iter().map(|t| 4 + t.len()).sum::<usize>() + 4;
        assert!(
            v2_bytes.len() < naive_size,
            "v2 ({}) should be smaller than naive ({})",
            v2_bytes.len(),
            naive_size
        );

        // Roundtrip via IndexedDictionary
        let indexed = IndexedDictionary::from_bytes(v2_bytes).unwrap();
        assert_eq!(indexed.len(), 103);

        // resolve works
        assert_eq!(indexed.resolve(0), Some("http://example.org/ontology/goidelic#0"));
        assert_eq!(indexed.resolve(50), Some("http://example.org/ontology/goidelic#50"));
        assert_eq!(indexed.resolve(100), Some("http://www.w3.org/ns/lemon/ontolex#LexicalEntry"));
        assert_eq!(indexed.resolve(102), Some("plain-literal"));

        // lookup works
        assert_eq!(indexed.lookup("http://example.org/ontology/goidelic#0"), Some(0));
        assert_eq!(indexed.lookup("http://example.org/ontology/goidelic#99"), Some(99));
        assert_eq!(indexed.lookup("http://www.w3.org/ns/lemon/ontolex#Form"), Some(101));
        assert_eq!(indexed.lookup("plain-literal"), Some(102));
        assert_eq!(indexed.lookup("nonexistent"), None);
    }

    #[test]
    fn test_v2_no_prefixes_for_short_terms() {
        let mut dict = Dictionary::new();
        dict.intern("a");
        dict.intern("b");
        dict.intern("c");

        let bytes = dict.to_bytes_indexed();
        let indexed = IndexedDictionary::from_bytes(bytes).unwrap();

        assert_eq!(indexed.resolve(0), Some("a"));
        assert_eq!(indexed.resolve(1), Some("b"));
        assert_eq!(indexed.resolve(2), Some("c"));
        assert_eq!(indexed.lookup("b"), Some(1));
        assert_eq!(indexed.lookup("d"), None);
    }

    #[test]
    fn test_v2_roundtrip_to_dictionary() {
        let mut dict = Dictionary::new();
        dict.intern("http://example.org/foo");
        dict.intern("http://example.org/bar");
        dict.intern("hello world");

        let bytes = dict.to_bytes_indexed();
        let dict2 = Dictionary::from_bytes(&bytes).unwrap();
        assert_eq!(dict2.len(), 3);
        assert_eq!(dict2.resolve(0), Some("http://example.org/foo"));
        assert_eq!(dict2.resolve(1), Some("http://example.org/bar"));
        assert_eq!(dict2.resolve(2), Some("hello world"));
        assert_eq!(dict2.lookup("http://example.org/bar"), Some(1));
    }

    #[test]
    fn test_v2_empty() {
        let dict = Dictionary::new();
        let bytes = dict.to_bytes_indexed();
        let indexed = IndexedDictionary::from_bytes(bytes).unwrap();
        assert_eq!(indexed.len(), 0);
        assert!(indexed.is_empty());
        assert_eq!(indexed.resolve(0), None);
        assert_eq!(indexed.lookup("anything"), None);
    }

    #[test]
    fn test_prefix_detection() {
        let terms: Vec<String> = (0..50)
            .map(|i| format!("http://example.org/resource/{}", i))
            .chain(std::iter::once("standalone".to_string()))
            .collect();
        let prefixes = detect_prefixes(&terms);
        assert!(!prefixes.is_empty());
        assert!(prefixes[0].contains("http://example.org/resource/"));
    }
}
