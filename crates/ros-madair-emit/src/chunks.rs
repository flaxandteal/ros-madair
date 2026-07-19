// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chunk sink: content-hashed msgpack chunks + per-chunk concept summary
//! (summary.bin-as-data — P1: summary granularity = chunk, never row).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use alizarin_core::StaticTile;
use ros_madair_format::ChunkTile;
use sha2::{Digest, Sha256};

use crate::head::Interner;
use crate::EmitError;

pub(crate) const CHUNK_MAX_TILES: usize = 256;

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
    /// nodegroup uuid -> (node int, quantized value) pairs for pending tiles (A8)
    pub(crate) pending_values: BTreeMap<String, Vec<(i64, i64)>>,
    /// nodegroup uuid -> (node int, [min_lng, min_lat, max_lng, max_lat]) per-
    /// resource bboxes for pending tiles (A8.2). Raw f64, not quantized: the
    /// head stores the coarse box; the client verifies exact intersection.
    pub(crate) pending_geo: BTreeMap<String, Vec<(i64, [f64; 4])>>,
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
    /// (chunk, node, min_qvalue, max_qvalue, n) rows for `chunk_value_summary`
    /// (A8: coarse chunk→ordered-value-range; the native reader uses value_tags)
    pub(crate) value_summary_rows: Vec<(i64, i64, i64, i64, i64)>,
    /// (chunk, node, min_lng, min_lat, max_lng, max_lat, n) rows for
    /// `chunk_geo_summary` (A8.2: the union bbox of every resource's bbox in the
    /// chunk — coarse chunk→region for the browser spatial prune; the native
    /// reader uses geo_bbox directly)
    pub(crate) geo_summary_rows: Vec<(i64, i64, f64, f64, f64, f64, i64)>,
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
            pending_values: BTreeMap::new(),
            pending_geo: BTreeMap::new(),
            pending_members: BTreeMap::new(),
            chunk_by_hash: HashMap::new(),
            chunk_rows: Vec::new(),
            summary_rows: Vec::new(),
            link_summary_rows: Vec::new(),
            value_summary_rows: Vec::new(),
            geo_summary_rows: Vec::new(),
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
        let values = self.pending_values.remove(ng).unwrap_or_default();
        let geo = self.pending_geo.remove(ng).unwrap_or_default();
        let members = self.pending_members.remove(ng).unwrap_or_default();
        if tiles.is_empty() {
            return Ok(());
        }
        // Serialize via ChunkTile so map key order (and the content
        // hash) is deterministic. Framed with the P17 version header — the hash
        // covers the header, so a format bump changes every chunk's identity.
        let chunk_tiles: Vec<ChunkTile> = tiles.iter().map(ChunkTile::from).collect();
        let bytes = ros_madair_format::encode_chunk(&chunk_tiles)?;
        let hash = hex(&Sha256::digest(&bytes));
        let chunk_id = if let Some(&id) = self.chunk_by_hash.get(&hash) {
            id
        } else {
            fs::write(self.chunks_dir.join(format!("{hash}.msgpack")), &bytes)?;
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
            // Per-chunk, per ordered-node: min/max/count over the quantized
            // values (A8). Coarse chunk→range for the browser range-prune; the
            // native reader queries value_tags directly.
            let mut vagg: BTreeMap<i64, (i64, i64, i64)> = BTreeMap::new();
            for (node, qvalue) in &values {
                let entry = vagg.entry(*node).or_insert((*qvalue, *qvalue, 0));
                entry.0 = entry.0.min(*qvalue);
                entry.1 = entry.1.max(*qvalue);
                entry.2 += 1;
            }
            for (node, (min_v, max_v, n)) in vagg {
                self.value_summary_rows.push((id, node, min_v, max_v, n));
            }
            // Per-chunk, per geo-node: the UNION bbox over every resource's bbox
            // in the chunk (A8.2). min corners take the min, max corners the max —
            // so the chunk box contains every resource box, and a query box that
            // misses the union misses every resource in it (a safe coarse prune).
            let mut gagg: BTreeMap<i64, ([f64; 4], i64)> = BTreeMap::new();
            for (node, b) in &geo {
                let entry = gagg.entry(*node).or_insert((*b, 0));
                entry.0[0] = entry.0[0].min(b[0]); // min_lng
                entry.0[1] = entry.0[1].min(b[1]); // min_lat
                entry.0[2] = entry.0[2].max(b[2]); // max_lng
                entry.0[3] = entry.0[3].max(b[3]); // max_lat
                entry.1 += 1;
            }
            for (node, (b, n)) in gagg {
                self.geo_summary_rows
                    .push((id, node, b[0], b[1], b[2], b[3], n));
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
