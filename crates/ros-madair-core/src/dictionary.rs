// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! URI/literal ↔ integer ID dictionary encoding.
//!
//! Maps URIs and literals to compact integer IDs for efficient storage in
//! page files and summary quads. The dictionary is built at index time and
//! loaded once by the browser client.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bidirectional dictionary mapping terms (URIs/literals) to integer IDs.
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

    /// Serialize the dictionary to a compact binary format.
    ///
    /// Format:
    /// - 4 bytes: entry count (u32 LE)
    /// - For each entry:
    ///   - 4 bytes: string length (u32 LE)
    ///   - N bytes: UTF-8 string
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let count = self.id_to_term.len() as u32;
        buf.extend_from_slice(&count.to_le_bytes());

        for term in &self.id_to_term {
            let len = term.len() as u32;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(term.as_bytes());
        }

        buf
    }

    /// Deserialize from the compact binary format.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
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
}
