// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! [`TileSource`] implementations backed by Rós Madair tile index files.
//!
//! Two variants:
//! - [`InMemoryTileSource`]: all tile file bytes pre-loaded (WASM / testing)
//! - [`DiskTileSource`]: reads tile files from a directory on demand (native / Python)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use alizarin_core::graph::StaticTile;
use alizarin_core::tile_source::{TileSource, TileSourceError};

use crate::tile_content_file::parse_tile_content_header;
use crate::uri::resource_uri;
use crate::{Dictionary, ResourceMap};

/// Extract tiles for a subject from raw tile file bytes.
fn extract_tiles_from_bytes(
    file_bytes: &[u8],
    dict_id: u32,
    resource_id: &str,
    nodegroup_id: Option<&str>,
) -> Result<Vec<StaticTile>, TileSourceError> {
    let header = parse_tile_content_header(file_bytes)
        .map_err(|e| TileSourceError::LoadError(e))?;

    let entry = header.entry_for_subject(dict_id).ok_or_else(|| {
        TileSourceError::ResourceNotFound {
            resource_id: resource_id.to_string(),
        }
    })?;

    let start = entry.blob_offset as usize;
    let end = start + entry.blob_size as usize;
    if end > file_bytes.len() {
        return Err(TileSourceError::LoadError(
            "Tile blob extends beyond file".into(),
        ));
    }

    let mut tiles: Vec<StaticTile> = rmp_serde::from_slice(&file_bytes[start..end])
        .map_err(|e| TileSourceError::LoadError(format!("Msgpack error: {}", e)))?;

    if let Some(ng_id) = nodegroup_id {
        tiles.retain(|t| t.nodegroup_id == ng_id);
    }

    Ok(tiles)
}

/// Resolve a resource_id to (dict_id, page_id) via dictionary + resource_map.
fn resolve_resource(
    dictionary: &Dictionary,
    resource_map: &ResourceMap,
    base_uri: &str,
    resource_id: &str,
) -> Result<(u32, u32), TileSourceError> {
    let uri = resource_uri(base_uri, resource_id);
    let dict_id = dictionary.lookup(&uri).ok_or_else(|| {
        TileSourceError::ResourceNotFound {
            resource_id: resource_id.to_string(),
        }
    })?;
    let page_id = resource_map.page_for(dict_id).ok_or_else(|| {
        TileSourceError::ResourceNotFound {
            resource_id: resource_id.to_string(),
        }
    })?;
    Ok((dict_id, page_id))
}

// ---------------------------------------------------------------------------
// InMemoryTileSource
// ---------------------------------------------------------------------------

/// In-memory tile source with pre-loaded tile file bytes.
///
/// All tile data is held in memory, so `load_tiles` is purely computational
/// (header parse + msgpack deserialize). Suitable for WASM (where tile files
/// have been fetched upfront) and testing.
pub struct InMemoryTileSource {
    base_uri: String,
    dictionary: Dictionary,
    resource_map: ResourceMap,
    /// page_id → raw tile file bytes
    tile_files: HashMap<u32, Vec<u8>>,
}

impl InMemoryTileSource {
    pub fn new(
        base_uri: String,
        dictionary: Dictionary,
        resource_map: ResourceMap,
        tile_files: HashMap<u32, Vec<u8>>,
    ) -> Self {
        Self {
            base_uri,
            dictionary,
            resource_map,
            tile_files,
        }
    }
}

impl TileSource for InMemoryTileSource {
    fn load_tiles(
        &self,
        resource_id: &str,
        nodegroup_id: Option<&str>,
    ) -> Result<Vec<StaticTile>, TileSourceError> {
        let (dict_id, page_id) =
            resolve_resource(&self.dictionary, &self.resource_map, &self.base_uri, resource_id)?;

        let file_bytes = self.tile_files.get(&page_id).ok_or_else(|| {
            TileSourceError::LoadError(format!("No tile file for page {}", page_id))
        })?;

        extract_tiles_from_bytes(file_bytes, dict_id, resource_id, nodegroup_id)
    }
}

// ---------------------------------------------------------------------------
// DiskTileSource
// ---------------------------------------------------------------------------

/// Disk-backed tile source that reads tile files on demand.
///
/// Each `load_tiles` call reads the relevant `tile_XXXX.dat` from the tiles
/// directory. Suitable for native builds and Python extensions where the
/// index lives on local disk.
pub struct DiskTileSource {
    base_uri: String,
    dictionary: Dictionary,
    resource_map: ResourceMap,
    tiles_dir: PathBuf,
}

impl DiskTileSource {
    pub fn new(
        base_uri: String,
        dictionary: Dictionary,
        resource_map: ResourceMap,
        tiles_dir: PathBuf,
    ) -> Self {
        Self {
            base_uri,
            dictionary,
            resource_map,
            tiles_dir,
        }
    }
}

impl TileSource for DiskTileSource {
    fn load_tiles(
        &self,
        resource_id: &str,
        nodegroup_id: Option<&str>,
    ) -> Result<Vec<StaticTile>, TileSourceError> {
        let (dict_id, page_id) =
            resolve_resource(&self.dictionary, &self.resource_map, &self.base_uri, resource_id)?;

        let tile_path = self.tiles_dir.join(format!("tile_{:04}.dat", page_id));
        let file_bytes = std::fs::read(&tile_path).map_err(|e| {
            TileSourceError::LoadError(format!("Failed to read {}: {}", tile_path.display(), e))
        })?;

        extract_tiles_from_bytes(&file_bytes, dict_id, resource_id, nodegroup_id)
    }
}

// ---------------------------------------------------------------------------
// GrowableTileSource
// ---------------------------------------------------------------------------

/// Tile source that accumulates tile files at runtime.
///
/// Designed for the combined WASM binary: the [`Dictionary`] and
/// [`ResourceMap`] are cloned from [`SparqlStore`] at connect-time, and tile
/// files are inserted as they are fetched from the CDN.  The [`RwLock`] on
/// `tile_files` satisfies the `Send + Sync` bound required by [`TileSource`];
/// WASM is single-threaded so there is never actual contention.
pub struct GrowableTileSource {
    base_uri: String,
    dictionary: Dictionary,
    resource_map: ResourceMap,
    tile_files: RwLock<HashMap<u32, Vec<u8>>>,
}

impl GrowableTileSource {
    pub fn new(base_uri: String, dictionary: Dictionary, resource_map: ResourceMap) -> Self {
        Self {
            base_uri,
            dictionary,
            resource_map,
            tile_files: RwLock::new(HashMap::new()),
        }
    }

    /// Insert (or replace) a tile file for the given page.
    pub fn insert_tile_file(&self, page_id: u32, bytes: Vec<u8>) {
        self.tile_files
            .write()
            .expect("GrowableTileSource lock poisoned")
            .insert(page_id, bytes);
    }

    /// Check whether a tile file has already been fetched for `page_id`.
    pub fn has_tile_file(&self, page_id: u32) -> bool {
        self.tile_files
            .read()
            .expect("GrowableTileSource lock poisoned")
            .contains_key(&page_id)
    }

    /// Borrow the dictionary.
    pub fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// Borrow the resource map.
    pub fn resource_map(&self) -> &ResourceMap {
        &self.resource_map
    }

    /// The base URI used for resource URI construction.
    pub fn base_uri(&self) -> &str {
        &self.base_uri
    }
}

impl TileSource for GrowableTileSource {
    fn load_tiles(
        &self,
        resource_id: &str,
        nodegroup_id: Option<&str>,
    ) -> Result<Vec<StaticTile>, TileSourceError> {
        let (dict_id, page_id) =
            resolve_resource(&self.dictionary, &self.resource_map, &self.base_uri, resource_id)?;

        let files = self
            .tile_files
            .read()
            .expect("GrowableTileSource lock poisoned");

        let file_bytes = match files.get(&page_id) {
            Some(b) => b,
            None => {
                // Page not fetched yet — fall through to JS tile_loader.
                return Err(TileSourceError::ResourceNotFound {
                    resource_id: resource_id.to_string(),
                });
            }
        };

        extract_tiles_from_bytes(file_bytes, dict_id, resource_id, nodegroup_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_content_file::write_tile_content_file;
    use crate::uri::resource_uri;
    use std::collections::HashMap as StdHashMap;

    fn build_test_fixtures() -> (Dictionary, ResourceMap, HashMap<u32, Vec<u8>>, String) {
        let base_uri = "https://example.org/";
        let mut dict = Dictionary::new();

        // Intern two resources
        let id_001 = dict.intern(&resource_uri(base_uri, "hp-001"));
        let id_002 = dict.intern(&resource_uri(base_uri, "hp-002"));

        // Build a resource map: both on page 0
        let mut resource_to_page = StdHashMap::new();
        resource_to_page.insert("hp-001".to_string(), 0u32);
        resource_to_page.insert("hp-002".to_string(), 0u32);
        let resource_map = ResourceMap::build(&dict, &resource_to_page, base_uri);

        // Build tiles for each resource
        let tiles_001 = vec![StaticTile {
            data: {
                let mut d = StdHashMap::new();
                d.insert("n-name".to_string(), serde_json::json!({"en": "Belfast Castle"}));
                d
            },
            nodegroup_id: "ng-name".to_string(),
            resourceinstance_id: "hp-001".to_string(),
            tileid: None,
            parenttile_id: None,
            provisionaledits: None,
            sortorder: None,
        }];

        let tiles_002 = vec![
            StaticTile {
                data: {
                    let mut d = StdHashMap::new();
                    d.insert("n-name".to_string(), serde_json::json!({"en": "Carrickfergus Castle"}));
                    d
                },
                nodegroup_id: "ng-name".to_string(),
                resourceinstance_id: "hp-002".to_string(),
                tileid: None,
                parenttile_id: None,
                provisionaledits: None,
                sortorder: None,
            },
            StaticTile {
                data: {
                    let mut d = StdHashMap::new();
                    d.insert("n-type".to_string(), serde_json::json!("castle"));
                    d
                },
                nodegroup_id: "ng-type".to_string(),
                resourceinstance_id: "hp-002".to_string(),
                tileid: None,
                parenttile_id: None,
                provisionaledits: None,
                sortorder: None,
            },
        ];

        // Serialize and write tile content file
        let blob_001 = rmp_serde::to_vec_named(&tiles_001).unwrap();
        let blob_002 = rmp_serde::to_vec_named(&tiles_002).unwrap();

        let mut entries: Vec<(u32, Vec<u8>)> = vec![(id_001, blob_001), (id_002, blob_002)];
        entries.sort_by_key(|(sid, _)| *sid);

        let file_bytes = write_tile_content_file(&entries);
        let mut tile_files = HashMap::new();
        tile_files.insert(0u32, file_bytes);

        (dict, resource_map, tile_files, base_uri.to_string())
    }

    #[test]
    fn test_in_memory_load_all_tiles() {
        let (dict, rmap, tile_files, base_uri) = build_test_fixtures();
        let source = InMemoryTileSource::new(base_uri, dict, rmap, tile_files);

        let tiles = source.load_tiles("hp-001", None).unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].nodegroup_id, "ng-name");

        let tiles = source.load_tiles("hp-002", None).unwrap();
        assert_eq!(tiles.len(), 2);
    }

    #[test]
    fn test_in_memory_filter_by_nodegroup() {
        let (dict, rmap, tile_files, base_uri) = build_test_fixtures();
        let source = InMemoryTileSource::new(base_uri, dict, rmap, tile_files);

        let tiles = source.load_tiles("hp-002", Some("ng-name")).unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].nodegroup_id, "ng-name");

        let tiles = source.load_tiles("hp-002", Some("ng-type")).unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].nodegroup_id, "ng-type");

        let tiles = source.load_tiles("hp-002", Some("ng-nonexistent")).unwrap();
        assert_eq!(tiles.len(), 0);
    }

    #[test]
    fn test_in_memory_resource_not_found() {
        let (dict, rmap, tile_files, base_uri) = build_test_fixtures();
        let source = InMemoryTileSource::new(base_uri, dict, rmap, tile_files);

        let result = source.load_tiles("hp-999", None);
        assert!(matches!(
            result,
            Err(TileSourceError::ResourceNotFound { .. })
        ));
    }

    #[test]
    fn test_disk_tile_source() {
        let (dict, rmap, tile_files, base_uri) = build_test_fixtures();

        let tmp_dir = std::env::temp_dir().join("ros_madair_test_tiles");
        std::fs::create_dir_all(&tmp_dir).unwrap();

        for (page_id, bytes) in &tile_files {
            std::fs::write(tmp_dir.join(format!("tile_{:04}.dat", page_id)), bytes).unwrap();
        }

        let source = DiskTileSource::new(base_uri, dict, rmap, tmp_dir.clone());

        let tiles = source.load_tiles("hp-001", None).unwrap();
        assert_eq!(tiles.len(), 1);

        let tiles = source.load_tiles("hp-002", Some("ng-type")).unwrap();
        assert_eq!(tiles.len(), 1);

        let result = source.load_tiles("hp-999", None);
        assert!(matches!(
            result,
            Err(TileSourceError::ResourceNotFound { .. })
        ));

        // Cleanup
        std::fs::remove_dir_all(&tmp_dir).ok();
    }
}
