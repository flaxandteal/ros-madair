// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chunk sink: content-hashed msgpack chunks + per-chunk concept summary
//! (summary.bin-as-data — P1: summary granularity = chunk, never row).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use alizarin_core::StaticTile;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::head::Interner;
use crate::EmitError;

pub(crate) const CHUNK_MAX_TILES: usize = 256;

/// Deterministic msgpack view of a tile: same named fields as
/// `StaticTile`, but `data` is a BTreeMap so key order (and hence the
/// chunk content hash) is run-stable. `StaticTile.data` is a HashMap —
/// serializing it directly makes every chunk hash random per process.
#[derive(Serialize)]
struct ChunkTile<'a> {
    data: BTreeMap<&'a str, &'a serde_json::Value>,
    nodegroup_id: &'a str,
    resourceinstance_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tileid: Option<&'a str>,
    parenttile_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sortorder: Option<i32>,
}

impl<'a> From<&'a StaticTile> for ChunkTile<'a> {
    fn from(tile: &'a StaticTile) -> Self {
        ChunkTile {
            data: tile
                .data
                .iter()
                .map(|(k, v)| (k.as_str(), v))
                .collect(),
            nodegroup_id: &tile.nodegroup_id,
            resourceinstance_id: &tile.resourceinstance_id,
            tileid: tile.tileid.as_deref(),
            parenttile_id: tile.parenttile_id.as_deref(),
            sortorder: tile.sortorder,
        }
    }
}

pub(crate) struct ChunkSink {
    chunks_dir: PathBuf,
    /// nodegroup uuid -> pending tiles for the next chunk (BTreeMap:
    /// deterministic grouping — HashMap order broke run-reproducible
    /// snapshot ids)
    pub(crate) pending_tiles: BTreeMap<String, Vec<StaticTile>>,
    /// nodegroup uuid -> (node int, concept int) pairs for pending tiles
    pub(crate) pending_concepts: BTreeMap<String, Vec<(i64, i64)>>,
    /// nodegroup uuid -> (node int, target int) pairs for pending tiles
    pub(crate) pending_links: BTreeMap<String, Vec<(i64, i64)>>,
    /// nodegroup uuid -> (rid, tile count) memberships for pending tiles
    pub(crate) pending_members: BTreeMap<String, Vec<(i64, usize)>>,
    /// content hash -> chunk id (content-addressed dedupe)
    chunk_by_hash: HashMap<String, i64>,
    /// (chunk id, hash) rows for the `chunks` table
    pub(crate) chunk_rows: Vec<(i64, String)>,
    /// (chunk, node, min_concept, max_concept, n) rows for `chunk_summary`
    pub(crate) summary_rows: Vec<(i64, i64, i64, i64, i64)>,
    /// (chunk, node, min_target, max_target, n) rows for
    /// `chunk_link_summary` (P1/P2: coarse chunk→target-range in the
    /// head; exact pairs resurface from the tiles client-side)
    pub(crate) link_summary_rows: Vec<(i64, i64, i64, i64, i64)>,
    /// (rid, nodegroup int, chunk id, tile count) rows for `fragment_dir`
    pub(crate) fragment_rows: Vec<(i64, i64, i64, i64)>,
}

impl ChunkSink {
    pub(crate) fn new(chunks_dir: PathBuf) -> Self {
        Self {
            chunks_dir,
            pending_tiles: BTreeMap::new(),
            pending_concepts: BTreeMap::new(),
            pending_links: BTreeMap::new(),
            pending_members: BTreeMap::new(),
            chunk_by_hash: HashMap::new(),
            chunk_rows: Vec::new(),
            summary_rows: Vec::new(),
            link_summary_rows: Vec::new(),
            fragment_rows: Vec::new(),
        }
    }

    /// Chunking: group one resource's tiles per nodegroup (BTreeMap:
    /// flush trigger order must be deterministic), record fragment
    /// memberships, and flush any bucket that reaches capacity.
    pub(crate) fn add_resource_tiles(
        &mut self,
        rid: i64,
        tiles: Vec<StaticTile>,
        interner: &mut Interner,
    ) -> Result<(), EmitError> {
        let mut by_ng: BTreeMap<String, Vec<StaticTile>> = BTreeMap::new();
        for tile in tiles {
            by_ng
                .entry(tile.nodegroup_id.clone())
                .or_default()
                .push(tile);
        }
        for (ng, ng_tiles) in by_ng {
            let count = ng_tiles.len();
            let bucket = self.pending_tiles.entry(ng.clone()).or_default();
            bucket.extend(ng_tiles);
            let full = bucket.len() >= CHUNK_MAX_TILES;
            self.pending_members
                .entry(ng.clone())
                .or_default()
                .push((rid, count));
            if full {
                self.flush(&ng, interner)?;
            }
        }
        Ok(())
    }

    /// Flush remaining chunks.
    pub(crate) fn flush_remaining(&mut self, interner: &mut Interner) -> Result<(), EmitError> {
        let mut ngs: Vec<String> = self.pending_tiles.keys().cloned().collect();
        ngs.sort();
        for ng in ngs {
            self.flush(&ng, interner)?;
        }
        Ok(())
    }

    fn flush(&mut self, ng: &str, interner: &mut Interner) -> Result<(), EmitError> {
        let tiles = self.pending_tiles.remove(ng).unwrap_or_default();
        let concepts = self.pending_concepts.remove(ng).unwrap_or_default();
        let links = self.pending_links.remove(ng).unwrap_or_default();
        let members = self.pending_members.remove(ng).unwrap_or_default();
        if tiles.is_empty() {
            return Ok(());
        }
        // Serialize via ChunkTile so map key order (and the content
        // hash) is deterministic.
        let chunk_tiles: Vec<ChunkTile> = tiles.iter().map(ChunkTile::from).collect();
        let bytes = rmp_serde::to_vec_named(&chunk_tiles)?;
        let hash = hex(&Sha256::digest(&bytes));
        let chunk_id = if let Some(&id) = self.chunk_by_hash.get(&hash) {
            id
        } else {
            fs::write(
                self.chunks_dir.join(format!("{hash}.msgpack")),
                &bytes,
            )?;
            let id = self.chunk_by_hash.len() as i64 + 1;
            self.chunk_by_hash.insert(hash.clone(), id);
            self.chunk_rows.push((id, hash));
            // Per-chunk, per concept-bearing node: min/max/count over the
            // interned concept ints of the tiles in this chunk (P1/P2/P15:
            // plan coarse against the summary, verify exact on hydration).
            // DFS-ordered interning makes these ranges subtree-tight
            // (P18-corollary).
            let mut agg: BTreeMap<i64, (i64, i64, i64)> = BTreeMap::new();
            for (node, concept) in &concepts {
                let entry = agg.entry(*node).or_insert((*concept, *concept, 0));
                entry.0 = entry.0.min(*concept);
                entry.1 = entry.1.max(*concept);
                entry.2 += 1;
            }
            for (node, (min_c, max_c, n)) in agg {
                self.summary_rows.push((id, node, min_c, max_c, n));
            }
            // Per-chunk, per link-bearing node: min/max/count over the
            // interned target ints. This is the ONLY head record of
            // links (P1: summary granularity = chunk, never row; exact
            // pairs live in the tiles).
            let mut lagg: BTreeMap<i64, (i64, i64, i64)> = BTreeMap::new();
            for (node, target) in &links {
                let entry = lagg.entry(*node).or_insert((*target, *target, 0));
                entry.0 = entry.0.min(*target);
                entry.1 = entry.1.max(*target);
                entry.2 += 1;
            }
            for (node, (min_t, max_t, n)) in lagg {
                self.link_summary_rows.push((id, node, min_t, max_t, n));
            }
            id
        };
        let ng_int = interner.intern(ng);
        for (rid, count) in members {
            self.fragment_rows
                .push((rid, ng_int, chunk_id, count as i64));
        }
        Ok(())
    }
}

pub(crate) fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
