// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quantized head ("RM-the-schema in SQLite"): schema DDL, dictionary
//! interning, spine/concept_tags/vocab/chunk summary/fragment_dir
//! population, rollups, indexes and VACUUM.

use std::collections::{BTreeMap, HashMap, HashSet};

use alizarin_core::skos::{SkosCollection, SkosConcept};
use alizarin_core::{
    datatype_index_spec, ExtensionTypeRegistry, IndexClass, IndexedGraph, StaticGraph,
    StaticResource, StaticTile,
};
use rusqlite::{Connection, Transaction};

use crate::chunks::ChunkSink;
use crate::closure::Closure;
use crate::{EmitError, FieldClassError};
use ros_madair_format::FieldEntry;

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
    rows: &mut Vec<(i64, i64, i64)>,
) {
    // Poly-hierarchy / repeated subtrees: first DFS occurrence wins;
    // a repeat visit would fracture the interval, so skip it.
    if interner.map.contains_key(&concept.id) {
        return;
    }
    let enter = interner.intern(&concept.id);
    for child in concept.children.iter().flatten() {
        preintern_concepts_dfs(interner, child, rows);
    }
    // Only concept ids have been interned so far, so the current max
    // term_id is the last id assigned within this subtree.
    let leave = interner.len() as i64;
    rows.push((enter, enter, leave));
}

/// Pre-intern concepts in closure DFS order BEFORE anything else is
/// interned, so concept term_ids are dense, contiguous per subtree
/// (P18-corollary: interning order is locality order). Traversal
/// mirrors build_closure.
pub(crate) fn preintern_concepts(
    interner: &mut Interner,
    collections: &[SkosCollection],
) -> Vec<(i64, i64, i64)> {
    let mut vocab_rows: Vec<(i64, i64, i64)> = Vec::new();
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
            preintern_concepts_dfs(interner, concept, &mut vocab_rows);
        }
    }
    vocab_rows
}

// ---------------------------------------------------------------------------
// Index routing
// ---------------------------------------------------------------------------

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
         CREATE TABLE vocab (concept INTEGER PRIMARY KEY,
             dfs_enter INTEGER NOT NULL, dfs_leave INTEGER NOT NULL);
         CREATE TABLE concept_tags (rid INTEGER NOT NULL,
             node INTEGER NOT NULL, concept INTEGER NOT NULL,
             UNIQUE(rid, node, concept));
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
             max_target INTEGER NOT NULL, n INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// Field plan: datatype inference proposes, the schema declaration
/// disposes (M1 item 3). Inference: concepts and links are head-indexed;
/// everything else (strings, numbers, dates, geo, …) is detail-only and
/// lives in the body chunks. A declared class overrides inference:
///   "detail-only" — the node is NOT head-indexed (its node id goes into
///     the returned set, gating concept_tags / chunk summary population;
///     rollups follow automatically since they are computed FROM
///     concept_tags);
///   "filterable" — legal only on concept and resource-link datatypes;
///     anything else would require text in the head, so it is a typed
///     error (`FieldClassError::NotFilterable`).
/// Aliases seen in `declared` are recorded in `matched` so the caller
/// can reject declarations that touch no node in any model.
pub(crate) fn field_plan(
    graph: &StaticGraph,
    declared: &BTreeMap<String, String>,
    matched: &mut std::collections::BTreeSet<String>,
    registry: &ExtensionTypeRegistry,
) -> Result<(BTreeMap<String, FieldEntry>, HashSet<String>), EmitError> {
    let mut fields: BTreeMap<String, FieldEntry> = BTreeMap::new();
    let mut detail_only: HashSet<String> = HashSet::new();
    for node in graph.nodes_slice() {
        let Some(alias) = node.alias.clone() else {
            continue;
        };
        if node.nodegroup_id.is_none() {
            continue;
        }
        let dt = node.datatype.as_str();
        // Datatype inference now routes through the core index-keys seam:
        // the registry (with the CLM reference handler registered) decides
        // an extension datatype's class; core's built-in match covers only
        // concept/link. The tile value is irrelevant to the *class*, so a
        // null value suffices here (keys are extracted per-tile in
        // populate_model).
        let cfg = node_config_value(&node.config);
        let index_class =
            datatype_index_spec(dt, &serde_json::Value::Null, cfg.as_ref(), Some(registry)).class;
        let (storage, class) = match declared.get(&alias).map(String::as_str) {
            Some("filterable") => {
                matched.insert(alias.clone());
                match index_class {
                    IndexClass::ConceptHierarchical { .. } => ("concept", "filterable"),
                    // Coarse remains the only head link storage (P1);
                    // the declaration is recorded for the M2 compiler.
                    IndexClass::Link => ("link-coarse", "filterable"),
                    IndexClass::DetailOnly => {
                        return Err(Box::new(FieldClassError::NotFilterable {
                            alias,
                            datatype: dt.to_string(),
                        }));
                    }
                }
            }
            Some("detail-only") => {
                matched.insert(alias.clone());
                detail_only.insert(node.nodeid.clone());
                ("detail-only", "detail-only")
            }
            Some(other) => {
                // emit_with_options validates upfront; kept as a typed
                // error rather than an unreachable! for defence.
                return Err(Box::new(FieldClassError::UnknownClass {
                    alias,
                    class: other.to_string(),
                }));
            }
            None => match index_class {
                IndexClass::ConceptHierarchical { .. } => ("concept", "filterable"),
                IndexClass::Link => ("link-coarse", "coarse"),
                IndexClass::DetailOnly => ("detail-only", "detail-only"),
            },
        };
        fields.insert(
            alias,
            FieldEntry {
                node_id: node.nodeid.clone(),
                datatype: dt.to_string(),
                storage: storage.to_string(),
                class: class.to_string(),
            },
        );
    }
    Ok((fields, detail_only))
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
    /// Built once per model so the per-resource spine display_name can be the
    /// EVALUATED descriptor template, not the raw literal. Cloning the graph
    /// once per model is a fixed setup cost; it does not touch the per-resource
    /// memory bound (only tiles stream). (A1: `spine.display_name` used to emit
    /// the unrendered template, e.g. `'<Headword>'`, making the resource→
    /// descriptor index structurally present but useless for display.)
    indexed: IndexedGraph,
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
            indexed: IndexedGraph::new(graph.clone()),
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
        let evaluated = self.indexed.build_descriptors_with_context(
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
    detail_only: &HashSet<String>,
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
            if detail_only.contains(node_id.as_str()) {
                // Schema-declared detail-only (M1 item 3): NOT head-indexed
                // regardless of datatype — no concept_tags rows, no chunk
                // concept/link summaries (and hence no rollups: those are
                // computed from concept_tags in finalize). The values still
                // live in the tile chunks.
                continue;
            }
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
                    // Coarse only (P1): no exact head row — targets feed the
                    // per-chunk min/max summary; exact pairs resurface from
                    // the tiles.
                    let node_int = interner.intern(node_id);
                    for target in spec.keys {
                        let target_int = interner.intern(&target);
                        sink.pending_links
                            .entry(tile.nodegroup_id.clone())
                            .or_default()
                            .push((node_int, target_int));
                    }
                }
                // Strings/numbers/dates/etc.: detail-only — NOT head-indexed
                // (they live in the chunks).
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
    sink.add_resource_tiles(rid, tiles, interner)?;
    Ok(())
}

/// Bulk-insert dict (bulk), vocab, chunks, chunk_summary,
/// chunk_link_summary, fragment_dir.
pub(crate) fn insert_bulk(
    conn: &mut Connection,
    interner: &Interner,
    vocab_rows: &[(i64, i64, i64)],
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
            "INSERT INTO vocab (concept, dfs_enter, dfs_leave) VALUES (?1,?2,?3)",
        )?;
        for row in vocab_rows {
            stmt.execute(*row)?;
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
         CREATE INDEX idx_frag ON fragment_dir (rid, nodegroup);
         CREATE INDEX idx_summary ON chunk_summary (node, min_concept, max_concept);
         CREATE INDEX idx_link_summary
             ON chunk_link_summary (node, min_target, max_target);
         ANALYZE;
         VACUUM;",
    )?;
    Ok(())
}
