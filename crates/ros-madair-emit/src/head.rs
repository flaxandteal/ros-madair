// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quantized head ("RM-the-schema in SQLite"): schema DDL, dictionary
//! interning, spine/concept_tags/vocab/chunk summary/fragment_dir
//! population, rollups, indexes and VACUUM.

use std::collections::HashMap;

use alizarin_core::skos::{SkosCollection, SkosConcept};
use alizarin_core::{
    datatype_index_spec, ExtensionTypeRegistry, IndexClass, StaticGraph,
    StaticResource, StaticTile,
};
use rusqlite::{Connection, Transaction};

use crate::chunks::ChunkSink;
use crate::closure::Closure;
use crate::EmitError;

// ---------------------------------------------------------------------------
// Dictionary interning (dictionary.bin minus the parser — P0-corollary)
// ---------------------------------------------------------------------------

/// Interns every UUID/URI the head references (resource ids, node ids,
/// nodegroup ids, concept ids, link targets) to a dense i64. Built in
/// memory during emit; bulk-inserted into `dict` at the end.
#[derive(Default)]
pub(crate) struct Interner {
    map: HashMap<String, i64>,
}

impl Interner {
    pub(crate) fn intern(&mut self, term: &str) -> i64 {
        if let Some(&id) = self.map.get(term) {
            return id;
        }
        let id = self.map.len() as i64 + 1;
        self.map.insert(term.to_string(), id);
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// Pre-intern concept ids in closure DFS order (P18-corollary: interning
/// order is locality order). Run against a FRESH interner so concept
/// term_ids are the dense prefix 1..=N and every subtree's ids are
/// contiguous. Returns vocab rows (concept term_id, dfs_enter, dfs_leave)
/// where dfs_enter = own term_id and dfs_leave = max descendant term_id —
/// hierarchy membership is then `concept BETWEEN dfs_enter AND dfs_leave`
/// (P10 DFS intervals; this is what lets the exact concept_tags table
/// answer ancestor queries with no expanded table, and what makes the
/// chunk_summary min/max ranges prune at all).
fn preintern_concepts_dfs(
    interner: &mut Interner,
    concept: &SkosConcept,
    closure: &Closure,
    rows: &mut Vec<VocabRow>,
) {
    // Poly-hierarchy / repeated subtrees: first DFS occurrence wins;
    // a repeat visit would fracture the interval, so skip it.
    if interner.map.contains_key(&concept.id) {
        return;
    }
    let enter = interner.intern(&concept.id);
    for child in concept.children.iter().flatten() {
        preintern_concepts_dfs(interner, child, closure, rows);
    }
    // Only concept ids have been interned so far, so the current max
    // term_id is the last id assigned within this subtree.
    let leave = interner.len() as i64;
    // A2: carry the concept's label so display resolves as a SQL join
    // (`concept_tags → vocab.label`), self-contained in the head. Take the
    // label from the CLOSURE, not `label_of(concept)` on this node: a concept
    // can appear twice in the tree — a shallow membership ref with no
    // pref_labels, and its full definition — and DFS-first here would pick the
    // shallow one (a UUID). `build_closure` already resolved the real label
    // (last-write-wins over both occurrences), so this makes vocab.label
    // byte-identical to what closure.json carried.
    let label = closure
        .concepts
        .get(&concept.id)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| crate::closure::label_of(concept));
    rows.push(VocabRow {
        concept: enter,
        dfs_enter: enter,
        dfs_leave: leave,
        label,
    });
}

/// One row of the head's `vocab` table: a concept's DFS interval plus its
/// display label (A2). The label makes concept display a self-contained SQL
/// join instead of a `closure.json` sidecar lookup.
pub(crate) struct VocabRow {
    pub concept: i64,
    pub dfs_enter: i64,
    pub dfs_leave: i64,
    pub label: String,
}

/// Pre-intern concepts in closure DFS order BEFORE anything else is
/// interned, so concept term_ids are dense, contiguous per subtree
/// (P18-corollary: interning order is locality order). Traversal
/// mirrors build_closure.
pub(crate) fn preintern_concepts(
    interner: &mut Interner,
    collections: &[SkosCollection],
    closure: &Closure,
) -> Vec<VocabRow> {
    let mut vocab_rows: Vec<VocabRow> = Vec::new();
    for coll in collections {
        // The concept maps are HashMaps — sort top-level by id so term
        // ids (and hence the snapshot id) are deterministic (P16).
        // Children are Vecs and keep parse order.
        let mut top: Vec<&SkosConcept> = if !coll.concepts.is_empty() {
            coll.concepts.values().collect()
        } else {
            coll.all_concepts.values().collect()
        };
        top.sort_by(|a, b| a.id.cmp(&b.id));
        for concept in top {
            preintern_concepts_dfs(interner, concept, closure, &mut vocab_rows);
        }
    }
    vocab_rows
}

// ---------------------------------------------------------------------------
// Index routing
// ---------------------------------------------------------------------------

/// Quantize a raw ordered-scalar value into the head's signed sortable key,
/// dispatching on datatype (A8). The `IndexClass::Ordered` class is opaque about
/// meaning — this is where the datatype decides the encoding.
///
/// A8.1: date/edtf → days-from-civil (`alizarin_core::quantize`). This is for
/// SCALAR ordered values only; geometry (A8.2) is NOT quantized to a single key —
/// it indexes a bounding box (`geo::extract_bbox` → geo_bbox) and never reaches
/// here. `None` (unparseable) → the value is not head-indexed.
fn quantize_ordered(datatype: &str, raw: &str) -> Option<i64> {
    match datatype {
        "date" | "edtf" => alizarin_core::quantize::quantize_date(raw),
        _ => None,
    }
}

/// A node's config as a JSON object (the wire shape `datatype_index_spec`
/// and extension handlers expect), or `None` when the node has no config.
/// This is what lets a handler (e.g. the CLM reference handler) resolve its
/// own collection — the emitter never inspects config keys directly.
fn node_config_value(config: &HashMap<String, serde_json::Value>) -> Option<serde_json::Value> {
    if config.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(
        config.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    ))
}

// ---------------------------------------------------------------------------
// Schema DDL
// ---------------------------------------------------------------------------

/// Create the head schema. No text in the head: the only TEXT columns
/// are the interning dictionary itself, chunk content hashes, and
/// display_name.
pub(crate) fn create_schema(conn: &Connection) -> Result<(), EmitError> {
    conn.execute_batch(
        "PRAGMA page_size=8192; PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
         CREATE TABLE dict (term_id INTEGER PRIMARY KEY,
             term TEXT UNIQUE NOT NULL);
         -- P10 DFS intervals: dfs_enter = the concept's own term_id
         -- (concepts are pre-interned in closure DFS order), dfs_leave =
         -- max descendant term_id. Hierarchy membership is
         --   concept BETWEEN dfs_enter AND dfs_leave
         -- against the exact concept_tags table — no expanded table.
         -- `label` (A2): the concept's display text, so concept display is a
         -- self-contained join (concept_tags → vocab.label) — no closure.json
         -- sidecar. A bounded controlled vocabulary, like dict.term already is.
         CREATE TABLE vocab (concept INTEGER PRIMARY KEY,
             dfs_enter INTEGER NOT NULL, dfs_leave INTEGER NOT NULL,
             label TEXT NOT NULL);
         CREATE TABLE concept_tags (rid INTEGER NOT NULL,
             node INTEGER NOT NULL, concept INTEGER NOT NULL,
             UNIQUE(rid, node, concept));
         -- A8: ordered-scalar exact index — the value_tags analogue of
         -- concept_tags. `qvalue` is a SIGNED sortable key (date →
         -- days-from-civil; SQLite INTEGER orders signed, so pre-1970 is a
         -- negative that sorts first — no bias). A range query is
         -- `qvalue BETWEEN lo AND hi`. Not interned: the key is already a
         -- number, not a dict term.
         CREATE TABLE value_tags (rid INTEGER NOT NULL,
             node INTEGER NOT NULL, qvalue INTEGER NOT NULL,
             UNIQUE(rid, node, qvalue));
         -- A8.2: spatial exact index — one axis-aligned bounding box per
         -- (rid, node) geometry. Corners are RAW f64 (REAL), NOT quantized: a
         -- bbox-overlap filter is already a coarse SUPERSET of true intersection
         -- (a candidate set with no false negatives), so there is nothing to gain
         -- from a lossy space-filling-curve key here — the client verifies exact
         -- intersection on hydration. A bbox is deliberately NOT a centroid: a
         -- polygon whose centroid sits outside a query box still overlaps it, and
         -- a centroid index would wrongly drop it.
         CREATE TABLE geo_bbox (rid INTEGER NOT NULL, node INTEGER NOT NULL,
             min_lng REAL NOT NULL, min_lat REAL NOT NULL,
             max_lng REAL NOT NULL, max_lat REAL NOT NULL,
             UNIQUE(rid, node, min_lng, min_lat, max_lng, max_lat));
         -- P12 shadow reverse-index: the EXACT (link node, target, source) edge,
         -- so a reverse lookup ('who links TO X via this node?') is an indexed
         -- point query instead of a coarse chunk_link_summary scan + chunk read +
         -- verify. `source` is the CITER's resource term_id (not rid), so the
         -- reverse query returns citer UUIDs via one dict join — no spine join.
         -- This is the dual of the deliberately-absent forward exact link table
         -- (P1: forward HasLink stays coarse): materialized only because measured
         -- reverse lookups scanned 71–100% of a model's link chunks (A8-locality
         -- orders a model for its FORWARD field, scattering its reverse targets —
         -- the shadow index decouples the two directions).
         CREATE TABLE reverse_links (node INTEGER NOT NULL,
             target INTEGER NOT NULL, source INTEGER NOT NULL,
             UNIQUE(node, target, source));
         -- fragment_dir doubles as the layer-composition PRESENCE index: a
         -- (rid, nodegroup) row means this layer carries this nodegroup for
         -- this resource. Layer precedence is per-NODEGROUP, not per-node,
         -- because a nodegroup is the atomic unit a layer adds or replaces
         -- (see ros-madair-read layers module) — so no separate node-level
         -- presence table is needed, and the row that already exists here for
         -- every emitted tile is exactly the signal composition wants. An
         -- overlay that RETRACTS a nodegroup ships an empty tile for it, which
         -- still produces a fragment_dir row, so deleted and never-mentioned
         -- stay distinguishable.
         CREATE TABLE fragment_dir (rid INTEGER NOT NULL,
             nodegroup INTEGER NOT NULL, chunk INTEGER NOT NULL,
             tile_count INTEGER NOT NULL);
         CREATE TABLE chunks (chunk INTEGER PRIMARY KEY,
             hash TEXT UNIQUE NOT NULL);
         CREATE TABLE chunk_summary (chunk INTEGER NOT NULL,
             node INTEGER NOT NULL, min_concept INTEGER NOT NULL,
             max_concept INTEGER NOT NULL, n INTEGER NOT NULL);
         -- P1/P2 coarse links: the head stores chunk->target-range only;
         -- exact (rid, target) pairs resurface from the tile chunks
         -- client-side. (An exact head link table is a possible opt-in,
         -- schema-declared field class later.)
         CREATE TABLE chunk_link_summary (chunk INTEGER NOT NULL,
             node INTEGER NOT NULL, min_target INTEGER NOT NULL,
             max_target INTEGER NOT NULL, n INTEGER NOT NULL);
         -- A8: coarse ordered-value ranges — the chunk_summary analogue for
         -- ordered scalars, so a browser client can prune which chunks to fetch
         -- for a range query (the native reader uses value_tags directly).
         CREATE TABLE chunk_value_summary (chunk INTEGER NOT NULL,
             node INTEGER NOT NULL, min_qvalue INTEGER NOT NULL,
             max_qvalue INTEGER NOT NULL, n INTEGER NOT NULL);
         -- A8.2: coarse chunk->region — the union bbox of every resource bbox in
         -- the chunk, so a browser client can prune which chunks to fetch for a
         -- spatial query (the native reader uses geo_bbox directly). Same coarse-
         -- prune role chunk_summary/chunk_value_summary play for concepts/scalars.
         CREATE TABLE chunk_geo_summary (chunk INTEGER NOT NULL,
             node INTEGER NOT NULL, min_lng REAL NOT NULL, min_lat REAL NOT NULL,
             max_lng REAL NOT NULL, max_lat REAL NOT NULL, n INTEGER NOT NULL);",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Population
// ---------------------------------------------------------------------------

/// Create one model's spine table + its term index. Split out of
/// population so ALL spine tables are created up front (before the single
/// streaming transaction opens); creation order matches the pre-split
/// emit (schema tables, then spine tables in model order), keeping the
/// sqlite_master order — and hence VACUUM'd byte layout — stable.
///
/// NOTE: the literal's embedded whitespace is load-bearing — SQLite
/// stores the CREATE TABLE text verbatim in sqlite_master, so the
/// continuation indent below must stay exactly as it was in the batch
/// emit() (snapshot id stability).
pub(crate) fn create_spine_table(conn: &Connection, spine_table: &str) -> Result<(), EmitError> {
    conn.execute_batch(&format!(
        "CREATE TABLE {spine_table} (rid INTEGER PRIMARY KEY,
                 term_id INTEGER NOT NULL, display_name TEXT);
             CREATE INDEX idx_{spine_table}_term ON {spine_table} (term_id);"
    ))?;
    Ok(())
}

/// Per-model routing tables (node id -> datatype / config), built once
/// from the graph and reused across all of that model's streamed
/// resources. Borrows the graph, which outlives the streaming pass.
pub(crate) struct ModelCtx<'g> {
    node_datatype: HashMap<&'g str, &'g str>,
    node_config: HashMap<&'g str, serde_json::Value>,
    /// The model graph, borrowed — `build_descriptors` runs on `&StaticGraph`
    /// directly, so evaluating the per-resource spine display_name (A1: the
    /// EVALUATED descriptor template, not the raw `'<Headword>'` literal) needs no
    /// graph clone. Borrowed for `'g`, like the node maps above.
    graph: &'g StaticGraph,
}

impl<'g> ModelCtx<'g> {
    pub(crate) fn new(graph: &'g StaticGraph) -> Self {
        let node_datatype = graph
            .nodes_slice()
            .iter()
            .map(|n| (n.nodeid.as_str(), n.datatype.as_str()))
            .collect();
        // Per-node config as JSON (for handler-owned collection resolution
        // in datatype_index_spec). Only nodes with config appear.
        let node_config = graph
            .nodes_slice()
            .iter()
            .filter_map(|n| node_config_value(&n.config).map(|cfg| (n.nodeid.as_str(), cfg)))
            .collect();
        ModelCtx {
            node_datatype,
            node_config,
            graph,
        }
    }

    /// Evaluate the resource's display name: the descriptor template rendered
    /// against its tiles (A1). The registry is passed so a descriptor over an
    /// extension datatype (e.g. `reference`) resolves rather than blanking.
    ///
    /// **Fall back to the raw `name` unless the template FULLY resolved.** A
    /// residual `<placeholder>` means the template referenced a node the resource
    /// does not carry, or one whose name does not match (Arches matches
    /// placeholders on node *name*, case-sensitively — a `<title>` template will
    /// not resolve against a node named `Title`). An unresolved `<title>` is a
    /// strictly worse display name than whatever `name` the export already
    /// carried, so it must never win. (When the export's `name` is *itself* the
    /// unrendered template — the case A1 targets — both are placeholders and the
    /// result is unchanged, i.e. no regression; the real remedy there is a
    /// descriptor whose placeholders match, which is a data concern.)
    fn display_name(
        &self,
        tiles: &[StaticTile],
        raw_name: &str,
        registry: &ExtensionTypeRegistry,
    ) -> String {
        let evaluated = self.graph.build_descriptors_with_context(
            tiles,
            &mut Vec::new(),
            None,
            Some(registry),
        );
        match evaluated.name {
            Some(n) if !n.is_empty() && !n.contains('<') => n,
            _ => raw_name.to_string(),
        }
    }
}

/// Populate the head from ONE streamed resource: spine row, exact
/// concept_tags, and the chunk sink's pending concept/link pairs and
/// tile buckets. The resource is consumed (its tiles are moved into the
/// sink) so it can be dropped immediately after — this is the memory
/// bound: only one resource (plus persistent interner/closure/sink) is
/// resident, regardless of corpus size.
///
/// Order within a resource is preserved byte-for-byte from the batch
/// populate_model loop (term_id intern, sorted-node tile scan, spine
/// insert, then sink.add_resource_tiles): rid/interner encounter order is
/// the snapshot-id oracle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_resource(
    tx: &Transaction,
    spine_table: &str,
    ctx: &ModelCtx,
    mut resource: StaticResource,
    closure: &Closure,
    interner: &mut Interner,
    sink: &mut ChunkSink,
    next_rid: &mut i64,
    total_resources: &mut usize,
    total_tiles: &mut usize,
    registry: &ExtensionTypeRegistry,
) -> Result<(), EmitError> {
    let mut stmt_spine = tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {spine_table} (rid, term_id, display_name)
             VALUES (?1, ?2, ?3)"
    ))?;
    let mut stmt_ct = tx.prepare_cached("INSERT OR IGNORE INTO concept_tags VALUES (?1,?2,?3)")?;
    let mut stmt_vt = tx.prepare_cached("INSERT OR IGNORE INTO value_tags VALUES (?1,?2,?3)")?;
    let mut stmt_geo =
        tx.prepare_cached("INSERT OR IGNORE INTO geo_bbox VALUES (?1,?2,?3,?4,?5,?6)")?;
    let mut stmt_rev =
        tx.prepare_cached("INSERT OR IGNORE INTO reverse_links VALUES (?1,?2,?3)")?;

    let res_id = resource.resourceinstance.resourceinstanceid.clone();
    let rid = *next_rid;
    *next_rid += 1;
    *total_resources += 1;
    let term_id = interner.intern(&res_id);
    let tiles = resource.tiles.take().unwrap_or_default();
    *total_tiles += tiles.len();

    for tile in &tiles {
        // Sorted node iteration: tile.data is a HashMap, and iteration
        // order decides interner id assignment and concept_tags insert
        // order (head file bytes).
        let mut data: Vec<(&String, &serde_json::Value)> = tile.data.iter().collect();
        data.sort_by_key(|(k, _)| k.as_str());
        for (node_id, value) in data {
            let dt = ctx
                .node_datatype
                .get(node_id.as_str())
                .copied()
                .unwrap_or("");
            if value.is_null() {
                continue;
            }
            // Datatype extraction routes through the core index-keys seam:
            // the registry (with the CLM reference handler registered) owns
            // extension datatypes; core's built-in match covers concept/link
            // only. The emitter still owns quantization — it interns the
            // returned keys and picks the head table per IndexClass.
            let cfg = ctx.node_config.get(node_id.as_str());
            let spec = datatype_index_spec(dt, value, cfg, Some(registry));
            match spec.class {
                IndexClass::ConceptHierarchical { .. } => {
                    let node_int = interner.intern(node_id);
                    for vid in spec.keys {
                        let concept_id = closure.value_map.get(&vid).cloned().unwrap_or(vid);
                        // Concepts were pre-interned in DFS order, so this id
                        // sits inside its subtree's [dfs_enter, dfs_leave]
                        // interval — no ancestor expansion needed (P10).
                        let concept_int = interner.intern(&concept_id);
                        stmt_ct.execute((rid, node_int, concept_int))?;
                        sink.pending_concepts
                            .entry(tile.nodegroup_id.clone())
                            .or_default()
                            .push((node_int, concept_int));
                    }
                }
                IndexClass::Link => {
                    // FORWARD stays coarse (P1): targets feed the per-chunk
                    // min/max summary; exact pairs resurface from the tiles.
                    // REVERSE is exact (P12): the same edge is materialized into
                    // reverse_links keyed by target, so `cited_by` is an indexed
                    // lookup — `source` is THIS resource's term_id (the citer).
                    let node_int = interner.intern(node_id);
                    for target in spec.keys {
                        let target_int = interner.intern(&target);
                        sink.pending_links
                            .entry(tile.nodegroup_id.clone())
                            .or_default()
                            .push((node_int, target_int));
                        stmt_rev.execute((node_int, target_int, term_id))?;
                    }
                }
                IndexClass::Ordered => {
                    // A8: quantize the raw value into the head's signed sortable
                    // key space and write the exact row (value_tags) + feed the
                    // per-chunk coarse range (chunk_value_summary). Unparseable
                    // values are simply not indexed — they still live in the
                    // chunk, like any detail-only value.
                    let node_int = interner.intern(node_id);
                    for raw in spec.keys {
                        let Some(qvalue) = quantize_ordered(dt, &raw) else {
                            continue;
                        };
                        stmt_vt.execute((rid, node_int, qvalue))?;
                        sink.pending_values
                            .entry(tile.nodegroup_id.clone())
                            .or_default()
                            .push((node_int, qvalue));
                    }
                }
                IndexClass::SpatialBbox => {
                    // A8.2: extract the geometry's bounding box and write the
                    // exact row (geo_bbox) + feed the per-chunk union bbox
                    // (chunk_geo_summary). The key is the raw serialized GeoJSON;
                    // unparseable/empty geometry is simply not indexed (it still
                    // lives in the chunk, like any detail-only value).
                    let node_int = interner.intern(node_id);
                    for raw in spec.keys {
                        let Some((min_lng, min_lat, max_lng, max_lat)) = crate::geo::extract_bbox(&raw)
                        else {
                            continue;
                        };
                        stmt_geo.execute((rid, node_int, min_lng, min_lat, max_lng, max_lat))?;
                        sink.pending_geo
                            .entry(tile.nodegroup_id.clone())
                            .or_default()
                            .push((node_int, [min_lng, min_lat, max_lng, max_lat]));
                    }
                }
                // Strings/numbers/etc.: detail-only — NOT head-indexed (they
                // live in the chunks).
                IndexClass::DetailOnly => {}
            }
        }
    }

    // A1: the spine display_name is the EVALUATED descriptor (`abadh`), not the
    // raw template literal (`<Headword>`). `tiles` is still resident here (it
    // moves into the sink below), so this reads the same tiles that were just
    // indexed — no second pass, no extra memory.
    let display_name = ctx.display_name(&tiles, &resource.resourceinstance.name, registry);
    stmt_spine.execute((rid, term_id, &display_name))?;
    // Statements borrow tx; drop them before feeding the sink (the sink
    // does not touch the connection, but keep the borrow scope tight).
    drop(stmt_spine);
    drop(stmt_ct);
    drop(stmt_vt);
    drop(stmt_geo);
    drop(stmt_rev);
    sink.add_resource_tiles(rid, tiles, interner)?;
    Ok(())
}

/// Bulk-insert dict (bulk), vocab, chunks, chunk_summary,
/// chunk_link_summary, fragment_dir.
pub(crate) fn insert_bulk(
    conn: &mut Connection,
    interner: &Interner,
    vocab_rows: &[VocabRow],
    sink: &ChunkSink,
) -> Result<(), EmitError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached("INSERT INTO dict (term_id, term) VALUES (?1, ?2)")?;
        let mut terms: Vec<(&String, &i64)> = interner.map.iter().collect();
        terms.sort_by_key(|(_, id)| **id);
        for (term, id) in terms {
            stmt.execute((id, term))?;
        }
        let mut stmt = tx.prepare_cached(
            "INSERT INTO vocab (concept, dfs_enter, dfs_leave, label) VALUES (?1,?2,?3,?4)",
        )?;
        for row in vocab_rows {
            stmt.execute((row.concept, row.dfs_enter, row.dfs_leave, &row.label))?;
        }
        let mut stmt = tx.prepare_cached("INSERT INTO chunks (chunk, hash) VALUES (?1, ?2)")?;
        for (chunk, hash) in &sink.chunk_rows {
            stmt.execute((chunk, hash))?;
        }
        let mut stmt = tx.prepare_cached("INSERT INTO chunk_summary VALUES (?1,?2,?3,?4,?5)")?;
        for row in &sink.summary_rows {
            stmt.execute(*row)?;
        }
        let mut stmt =
            tx.prepare_cached("INSERT INTO chunk_link_summary VALUES (?1,?2,?3,?4,?5)")?;
        for row in &sink.link_summary_rows {
            stmt.execute(*row)?;
        }
        let mut stmt =
            tx.prepare_cached("INSERT INTO chunk_value_summary VALUES (?1,?2,?3,?4,?5)")?;
        for row in &sink.value_summary_rows {
            stmt.execute(*row)?;
        }
        let mut stmt =
            tx.prepare_cached("INSERT INTO chunk_geo_summary VALUES (?1,?2,?3,?4,?5,?6,?7)")?;
        for row in &sink.geo_summary_rows {
            stmt.execute(*row)?;
        }
        let mut stmt = tx.prepare_cached("INSERT INTO fragment_dir VALUES (?1,?2,?3,?4)")?;
        for row in &sink.fragment_rows {
            stmt.execute(*row)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Rollups + indexes + ANALYZE + VACUUM. Per-ancestor rollup counts
/// are computed via DFS ranges over the EXACT concept_tags table
/// (vocab self-join) — the expanded table is gone (P10 + P1: the
/// hierarchy index is intervals, not materialized rows). The
/// temporary bare-concept index makes the range join an index scan
/// per ancestor instead of a full-table scan.
pub(crate) fn finalize(conn: &Connection) -> Result<(), EmitError> {
    conn.execute_batch(
        "CREATE INDEX idx_ct_concept_tmp ON concept_tags (concept);
         CREATE TABLE rollup_concept_counts AS
             SELECT ct.node AS node, v.concept AS ancestor,
                    COUNT(DISTINCT ct.rid) AS n
             FROM vocab v JOIN concept_tags ct
               ON ct.concept BETWEEN v.dfs_enter AND v.dfs_leave
             GROUP BY ct.node, v.concept;
         DROP INDEX idx_ct_concept_tmp;
         -- Point-lookup index for the CountRecords rollup fast path
         -- (query_ir try_rollup_count): (node, ancestor) -> n.
         CREATE INDEX idx_rollup ON rollup_concept_counts (node, ancestor);
         -- Covering index for concept-driven access (query_ir exact-count
         -- and concept-driven select): (node, concept, rid) makes
         -- COUNT(DISTINCT rid) and rid-ordered selects index-only — no
         -- spine scan.
         CREATE INDEX idx_ct ON concept_tags (node, concept, rid);
         -- A8: covering index for range scans — (node, qvalue) so
         -- `qvalue BETWEEN lo AND hi` for a node is an index range, rid trailing.
         CREATE INDEX idx_vt ON value_tags (node, qvalue, rid);
         -- A8.2: covering index for the bbox-overlap scan. Leading `node`
         -- equality restricts to the geo node; the remaining corners + rid make
         -- the overlap predicate index-only (no R-tree — plain SQLite — so the
         -- box test still scans that node's rows, but never touches geo_bbox's
         -- heap or the spine).
         CREATE INDEX idx_geo
             ON geo_bbox (node, min_lng, max_lng, min_lat, max_lat, rid);
         -- P12: covering index for the reverse lookup — (node, target) equality
         -- yields the source term_ids directly, index-only.
         CREATE INDEX idx_rev ON reverse_links (node, target, source);
         CREATE INDEX idx_frag ON fragment_dir (rid, nodegroup);
         CREATE INDEX idx_summary ON chunk_summary (node, min_concept, max_concept);
         CREATE INDEX idx_link_summary
             ON chunk_link_summary (node, min_target, max_target);
         CREATE INDEX idx_value_summary
             ON chunk_value_summary (node, min_qvalue, max_qvalue);
         CREATE INDEX idx_geo_summary
             ON chunk_geo_summary (node, min_lng, max_lng, min_lat, max_lat);
         ANALYZE;
         VACUUM;",
    )?;
    // P17: stamp the format version into the head's own header, AFTER vacuum (so
    // it survives the rebuild). The reader gates `open_head` on it — a head from a
    // skewed emitter is refused, not silently misqueried. `user_version` defaults
    // to 0 on any DB that never set it, so a pre-P17 head fails the gate too.
    conn.execute_batch(&format!(
        "PRAGMA user_version = {};",
        ros_madair_format::FORMAT_VERSION
    ))?;
    Ok(())
}
