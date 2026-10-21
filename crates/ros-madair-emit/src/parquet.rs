// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Slice 1 of the DuckDB+Parquet substrate: the tile-row Parquet writer.**
//!
//! This is *additive* — it runs alongside the existing head/chunk writer and
//! touches none of the streaming emit. It reuses the emit front-half (graph load,
//! resource parse, datatype→index classification, geo bbox extraction) and adds a
//! new back-half: tiles → Parquet.
//!
//! The correspondence to the retired RM coarse/fine machinery:
//!   - **tile = one row** (the finest unit of retrieval),
//!   - a **row group** is the RM "chunk" (the unit DuckDB range-fetches + prunes),
//!   - **row order** — sorted by a per-graph *cluster key* — is the RM "locality"
//!     (Hilbert page assignment); here a Z-order blend of geo + descriptor name,
//!   - **promoted typed columns** (`q_ordered`, `concept_ids`, `geo_*`) are the
//!     node-level index; their per-row-group min/max stats in the Parquet footer
//!     ARE the zone-map — no separate index, no `summary_quads`.
//!
//! What stays in the `data` JSON blob is everything hydration needs and nothing
//! the query prunes on — the same head/detail split the SQLite head made, now in
//! one file with the stats for free.
//!
//! Slice-1 scope/limits (called out, not hidden): builds a model's rows in memory
//! (fine for the demo corpus; a streaming writer is a later slice); promotes the
//! FIRST indexed node of each class per tile (a nodegroup with two ordered or two
//! concept nodes would need per-node columns — the wide layout we deferred); the
//! cluster key is a Z-order (Morton), not the full Hilbert curve `locality.rs`
//! already has (a drop-in refinement).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{
    StaticGraph, StaticNode, StaticNodegroup, StaticResource, StaticResourceRegistry,
};
use alizarin_core::rdm_cache::{RdmCache, RdmCollection};

// Arrow/parquet: the small in-memory concept-catalog writer only. The tile path
// streams through DuckDB (below).
use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use duckdb::{params, Connection};

use crate::EmitError;

// ---------------------------------------------------------------------------
// Per-graph, index-time cluster config
// ---------------------------------------------------------------------------

/// A clustering dimension. Order in [`ClusterConfig::dimensions`] is the Z-order
/// bit priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterDim {
    /// The resource's geometry centroid (skipped for a resource with no geometry).
    Geo,
    /// The resource descriptor name, normalized to a sortable prefix code.
    Descriptor,
}

/// How one graph's tiles are clustered into row groups at emit time. Different
/// graphs have different needs, so this is keyed per graph by the caller; the
/// default — geospatial-if-present blended with the descriptor name — is the
/// sensible baseline for a spatial heritage corpus.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub dimensions: Vec<ClusterDim>,
    /// The row-group size — the RM "chunk size". Parquet flushes a row group
    /// every this-many rows.
    pub row_group_size: usize,
    /// When true, write one Parquet per nodegroup under a Hive-partitioned
    /// directory (`tiles_<slug>/nodegroup_id=<ng>/tiles.parquet`) instead of one
    /// file per model. This makes nodegroup a PARTITION axis (orthogonal to the
    /// cluster sort): a "give me tiles of nodegroups X,Y,Z across N resources"
    /// query reads ONLY those partitions and skips every other nodegroup's bytes.
    /// Cost: single-resource hydration then touches one file per nodegroup the
    /// resource has, rather than one contiguous block.
    pub partition_by_nodegroup: bool,
    /// When true, order tiles PRIMARILY by their nodegroup's DFS-interval `pre`
    /// index (then the cluster key), and emit a `*.nodegroup_intervals.json`
    /// sidecar mapping each nodegroup to its `[pre, submax]` subtree interval.
    /// Because a nodegroup's descendants occupy a contiguous `pre` range, a
    /// "nodegroup X and its children" query is then a single `pre BETWEEN pre_X
    /// AND submax_X` range read. Targets the single-file layout; when
    /// `partition_by_nodegroup` is also set, partitioning owns the physical
    /// layout and this only affects in-partition order + the sidecar.
    pub nodegroup_hierarchical_order: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            dimensions: vec![ClusterDim::Geo, ClusterDim::Descriptor],
            row_group_size: 8192,
            partition_by_nodegroup: false,
            nodegroup_hierarchical_order: false,
        }
    }
}

/// What writing one model's Parquet produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParquetModelSummary {
    pub slug: String,
    pub graph_id: String,
    /// The single file, or (when partitioned) the partition root directory.
    pub path: String,
    pub resources: usize,
    pub tiles: usize,
    pub row_groups: usize,
    /// Nodegroup partition count — 0 when written as a single file.
    pub partitions: usize,
    /// Edge rows written to `edges_<slug>.parquet` (one per link target).
    pub edges: usize,
    /// Melted concept rows written to `concepts_<slug>.parquet` (one per node value).
    pub concepts: usize,
    /// Melted ordered (date/edtf) rows written to `ordered_<slug>.parquet`.
    pub ordered: usize,
    /// Grid-cell edge rows written to `grid_<slug>.parquet` (one per geo resource).
    pub grid: usize,
}

// ---------------------------------------------------------------------------
// Z-order cluster key
// ---------------------------------------------------------------------------

/// Spread the 32 low bits of `x` into the even bit positions of a u64.
fn spread32(x: u32) -> u64 {
    let mut x = x as u64 & 0xFFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Interleave the bits of `a` (even positions) and `b` (odd positions) — a
/// 2D Morton / Z-order code.
fn morton2(a: u32, b: u32) -> u64 {
    spread32(a) | (spread32(b) << 1)
}

/// A geometry centroid → a 32-bit Z-order cell (two 16-bit lng/lat cells
/// interleaved). Whole-world extent; ample resolution for locality.
fn geo_cell(cx: f64, cy: f64) -> u32 {
    let nx = ((cx + 180.0) / 360.0).clamp(0.0, 1.0);
    let ny = ((cy + 90.0) / 180.0).clamp(0.0, 1.0);
    let gx = (nx * 65535.0).round() as u32;
    let gy = (ny * 65535.0).round() as u32;
    (morton2(gx, gy) & 0xFFFF_FFFF) as u32
}

/// Normalize a descriptor name to a 4-byte prefix code so lexically-adjacent
/// names cluster. Prefix-lossy by design — the leading characters carry the
/// locality, exactly as a geo cell coarsens coordinates.
fn name_code(name: &str) -> u32 {
    let norm: Vec<u8> = name
        .trim()
        .to_lowercase()
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric() || *b == b' ')
        .collect();
    let mut code = 0u32;
    for i in 0..4 {
        code = (code << 8) | norm.get(i).copied().unwrap_or(0) as u32;
    }
    code
}

/// The per-resource cluster key. Blends the configured dimensions; when geo is
/// wanted but absent, collapses to a name-only ordering.
fn cluster_key(dims: &[ClusterDim], geo_centroid: Option<(f64, f64)>, name: &str) -> u64 {
    let want_geo = dims.contains(&ClusterDim::Geo);
    let want_name = dims.contains(&ClusterDim::Descriptor);
    let g = if want_geo {
        geo_centroid.map(|(cx, cy)| geo_cell(cx, cy))
    } else {
        None
    };
    let n = if want_name { name_code(name) } else { 0 };
    match g {
        Some(gc) => morton2(gc, n),
        None => n as u64,
    }
}

// ---------------------------------------------------------------------------
// Nodegroup hierarchy → DFS intervals (nested-set labels)
// ---------------------------------------------------------------------------

/// DFS pre-order interval `(pre, submax)` for every nodegroup in the graph.
/// A nodegroup is a descendant of X iff its `pre` is in `[pre_X, submax_X]`, so
/// sorting tiles by `pre` makes a whole subtree one contiguous range. This is the
/// nodegroup analogue of the concept-hierarchy DFS intervals the head already
/// uses. Deterministic: children are visited in nodegroup-id order.
fn nodegroup_dfs_intervals(ngs: &[StaticNodegroup]) -> HashMap<String, (i64, i64)> {
    let ids: std::collections::HashSet<&str> = ngs.iter().map(|n| n.nodegroupid.as_str()).collect();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut roots: Vec<&str> = Vec::new();
    for ng in ngs {
        match &ng.parentnodegroup_id {
            Some(p) if ids.contains(p.as_str()) => children
                .entry(p.as_str())
                .or_default()
                .push(&ng.nodegroupid),
            // No parent, or a parent outside this graph → a root.
            _ => roots.push(&ng.nodegroupid),
        }
    }
    for v in children.values_mut() {
        v.sort_unstable();
    }
    roots.sort_unstable();

    fn visit(
        ng: &str,
        children: &HashMap<&str, Vec<&str>>,
        counter: &mut i64,
        out: &mut HashMap<String, (i64, i64)>,
    ) -> i64 {
        let pre = *counter;
        *counter += 1;
        let mut submax = pre;
        if let Some(kids) = children.get(ng) {
            for k in kids {
                submax = submax.max(visit(k, children, counter, out));
            }
        }
        out.insert(ng.to_string(), (pre, submax));
        submax
    }

    let mut counter = 0i64;
    let mut out = HashMap::new();
    for r in &roots {
        visit(r, &children, &mut counter, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// Concept catalog (projected from RdmCache) — DFS-interval + label
// ---------------------------------------------------------------------------

/// One row of the concept catalog: a concept's DFS-interval (so a subtree is a
/// contiguous `dfs_enter` range → `DescendantOrSelfOf` is one DuckDB range read)
/// plus its display label. Projected from the canonical `RdmCache`, not
/// reimplemented — the cache is the source of resolution, hierarchy, and labels.
struct ConceptCatalogRow {
    concept_id: String,
    dfs_enter: i64,
    dfs_leave: i64,
    label: String,
}

/// DFS-number one concept subtree over `narrower`. First occurrence wins on a
/// poly-hierarchy (a repeat visit would fracture the interval) — same rule the
/// head's concept DFS uses.
fn dfs_concept(
    coll: &RdmCollection,
    id: &str,
    counter: &mut i64,
    seen: &mut std::collections::HashSet<String>,
    rows: &mut Vec<ConceptCatalogRow>,
) {
    if !seen.insert(id.to_string()) {
        return;
    }
    let Some(concept) = coll.get_concept(id) else {
        return;
    };
    let enter = *counter;
    *counter += 1;
    let mut kids: Vec<&str> = concept.narrower.iter().map(String::as_str).collect();
    kids.sort_unstable();
    for k in kids {
        dfs_concept(coll, k, counter, seen, rows);
    }
    let dfs_leave = *counter - 1; // largest pre assigned within this subtree
    rows.push(ConceptCatalogRow {
        concept_id: id.to_string(),
        dfs_enter: enter,
        dfs_leave,
        label: coll.get_label(id, "en").unwrap_or_default(),
    });
}

/// Project the `RdmCache` into (catalog rows, value-id → concept-id map). The map
/// is the emit-time resolution so a promoted concept/reference key becomes the
/// canonical concept id — the same resolution hydration and the head use.
fn build_concept_catalog(cache: &RdmCache) -> (Vec<ConceptCatalogRow>, HashMap<String, String>) {
    let mut rows = Vec::new();
    let mut value_to_concept = HashMap::new();
    let mut counter = 0i64;
    let mut coll_ids = cache.get_collection_ids();
    coll_ids.sort();
    for cid in &coll_ids {
        let Some(coll) = cache.get_collection(cid) else {
            continue;
        };
        for vid in coll.get_value_ids() {
            if let Some(concept) = coll.get_concept_id_for_value(vid) {
                value_to_concept.insert(vid.clone(), concept.to_string());
            }
        }
        let mut tops: Vec<String> = coll
            .get_top_concepts()
            .iter()
            .map(|c| c.id.clone())
            .collect();
        tops.sort_unstable();
        let mut seen = std::collections::HashSet::new();
        for t in &tops {
            dfs_concept(coll, t, &mut counter, &mut seen, &mut rows);
        }
    }
    (rows, value_to_concept)
}

/// Write the concept catalog to `<out>/concept_catalog.parquet`, DFS-ordered so a
/// subtree is a contiguous row range (zone-map-prunable). Skipped when empty.
fn write_concept_catalog(
    mut rows: Vec<ConceptCatalogRow>,
    path: &Path,
) -> Result<usize, EmitError> {
    rows.sort_by_key(|r| r.dfs_enter);
    let schema = Arc::new(Schema::new(vec![
        Field::new("concept_id", DataType::Utf8, false),
        Field::new("dfs_enter", DataType::Int64, false),
        Field::new("dfs_leave", DataType::Int64, false),
        Field::new("label", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter(
                rows.iter().map(|r| Some(r.concept_id.as_str())),
            )),
            Arc::new(Int64Array::from_iter(
                rows.iter().map(|r| Some(r.dfs_enter)),
            )),
            Arc::new(Int64Array::from_iter(
                rows.iter().map(|r| Some(r.dfs_leave)),
            )),
            Arc::new(StringArray::from_iter(
                rows.iter().map(|r| Some(r.label.as_str())),
            )),
        ],
    )?;
    let file = fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(rows.len())
}

// ---------------------------------------------------------------------------
// Classification helpers (mirror ros-madair-query's datatype seam)
// ---------------------------------------------------------------------------

fn node_config_value(node: &StaticNode) -> Option<serde_json::Value> {
    if node.config.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(
        node.config
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ))
}

/// The first geometry bbox on a resource (used for its cluster geo cell), as
/// `(min_lng, min_lat, max_lng, max_lat)`.
fn first_geo_bbox(
    resource: &StaticResource,
    graph: &StaticGraph,
    registry: &ExtensionTypeRegistry,
) -> Option<(f64, f64, f64, f64)> {
    let tiles = resource.tiles.as_ref()?;
    for tile in tiles {
        // Sorted view: HashMap iteration is per-run randomized, and this picks
        // the FIRST bbox as the row's cluster geo cell — which decides row order.
        let mut sorted: Vec<(&String, &serde_json::Value)> = tile.data.iter().collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for &(node_id, value) in &sorted {
            if value.is_null() {
                continue;
            }
            let Some(node) = graph.get_node_by_id(node_id) else {
                continue;
            };
            let spec = datatype_index_spec(
                &node.datatype,
                value,
                node_config_value(node).as_ref(),
                Some(registry),
            );
            if matches!(spec.class, IndexClass::SpatialBbox) {
                if let Ok(s) = serde_json::to_string(value) {
                    if let Some(b) = crate::geo::extract_bbox(&s) {
                        return Some(b);
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Row assembly + write — tiles STREAM through a DuckDB stage (see `TileStage`),
// so there is no in-memory row buffer; each tile is promoted and appended, then
// dropped.
// ---------------------------------------------------------------------------

/// Write one model's tiles to `<out>/tiles_<graph_id>.parquet` (+ melted siblings).
pub fn write_model_parquet(
    slug: &str,
    graph: &StaticGraph,
    resources: &[StaticResource],
    cfg: &ClusterConfig,
    registry: &ExtensionTypeRegistry,
    // value-id → concept-id resolution (from `RdmCache`), so a promoted
    // concept/reference key is the canonical concept id — matching hydration and
    // the head. Empty = identity (a key not in a controlled list passes through).
    value_to_concept: &HashMap<String, String>,
    path: &Path,
) -> Result<ParquetModelSummary, EmitError> {
    let ng_intervals = nodegroup_dfs_intervals(graph.nodegroups_slice());
    let scratch = stage_scratch(path, slug);
    let stage = TileStage::new(&scratch)?;
    for r in resources {
        append_resource(
            &stage,
            slug,
            r,
            graph,
            cfg,
            registry,
            value_to_concept,
            &ng_intervals,
        )?;
    }
    write_ng_sidecar(&ng_intervals, cfg, path)?;
    let summary = stage.write_model(slug, &graph.graphid, cfg, path)?;
    drop(stage);
    let _ = fs::remove_dir_all(&scratch);
    Ok(summary)
}

/// Escape a single-quoted SQL string literal (double the quotes). Used for the
/// paths/slugs spliced into the DuckDB `COPY`/`SET` statements.
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Reduce an Arches `to_json()`-shaped reference value to the bare list-item
/// UUID(s) the canonical reference index keys on. Arches' reference display
/// representation is an array of `{ list_item_id, labels, list_id, uri }` objects
/// with the concept id at the object level; Arches' own `transform_value_for_tile`
/// discards that wrapper and keeps `list_item_id`. We do the same before indexing
/// so references melt into the concept store. Anything already canonical — a bare
/// UUID string, or a `{ uuid }` marker — passes through untouched.
fn normalize_reference_value(value: &serde_json::Value) -> serde_json::Value {
    fn reduce(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => match m.get("list_item_id").and_then(|x| x.as_str()) {
                Some(id) => serde_json::Value::String(id.to_string()),
                None => v.clone(),
            },
            other => other.clone(),
        }
    }
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(reduce).collect())
        }
        other => reduce(other),
    }
}

/// A scratch dir for a stage keyed to the output path + slug (unique per model,
/// cleaned up after write).
fn stage_scratch(path: &Path, slug: &str) -> std::path::PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".rmstage_{}", slug.replace('-', "_")))
}

/// Write the subtree-interval sidecar (`<path>.nodegroup_intervals.json`) when
/// hierarchical ordering is on: a consumer maps "nodegroup X" → `[pre, submax]`
/// and reads the single `ng_order BETWEEN pre AND submax` range.
fn write_ng_sidecar(
    ng_intervals: &HashMap<String, (i64, i64)>,
    cfg: &ClusterConfig,
    path: &Path,
) -> Result<(), EmitError> {
    if !cfg.nodegroup_hierarchical_order {
        return Ok(());
    }
    let mut map: Vec<(&String, (i64, i64))> = ng_intervals.iter().map(|(k, v)| (k, *v)).collect();
    map.sort_by_key(|(_, (pre, _))| *pre);
    let obj: serde_json::Map<String, serde_json::Value> = map
        .into_iter()
        .map(|(k, (pre, submax))| (k.clone(), serde_json::json!([pre, submax])))
        .collect();
    let sidecar = path.with_extension("nodegroup_intervals.json");
    if let Some(parent) = sidecar.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &sidecar,
        serde_json::to_vec_pretty(&serde_json::Value::Object(obj))?,
    )?;
    Ok(())
}

/// Promote one resource's tiles and STREAM them into the stage — no `Vec<Row>`.
/// Every promoted column is a pure function of the tile (+ the resource's
/// cluster key), so this holds one tile's worth of state at a time.
#[allow(clippy::too_many_arguments)]
/// Flatten one related-resource cache object to the compact `{id, title}` a
/// render card needs, dropping the fat `descriptors`/`meta`.
fn push_related(e: &serde_json::Value, acc: &mut Vec<serde_json::Value>) {
    if let serde_json::Value::Object(o) = e {
        acc.push(serde_json::json!({
            "id": o.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "title": o.get("title").cloned().unwrap_or(serde_json::Value::Null),
        }));
    }
}

/// Project the per-tile hydration cache (`r.cache` = `{tileId: {nodeId:
/// CacheEntry}}`, where a `CacheEntry` is an untagged single `{id,title,…}` or a
/// list `{"_":[…]}`) into a COMPACT per-resource render summary
/// `{nodeId: [{id, title}]}` — the handful of related titles a result card shows
/// (e.g. the linked site's name → ward). Merged across the resource's tiles so
/// it is one blob per resource, carried on the entry table / grid edge instead
/// of buried per-tile. `None` when there is no cache (render cache off, or the
/// resource has no links). This is the Q1 fix: the render header lives with the
/// resource, not scattered across its ~10 nodegroup-partition tile rows.
fn build_render_summary(cache: Option<&serde_json::Value>) -> Option<String> {
    let serde_json::Value::Object(tiles) = cache? else {
        return None;
    };
    let mut by_node: std::collections::BTreeMap<String, Vec<serde_json::Value>> =
        std::collections::BTreeMap::new();
    for slice in tiles.values() {
        let serde_json::Value::Object(nodes) = slice else {
            continue;
        };
        for (node_id, entry) in nodes {
            let acc = by_node.entry(node_id.clone()).or_default();
            match entry {
                // List cache entry: the related resources live under "_".
                serde_json::Value::Object(o) if o.get("_").is_some() => {
                    if let Some(serde_json::Value::Array(list)) = o.get("_") {
                        for e in list {
                            push_related(e, acc);
                        }
                    }
                }
                // Single cache entry: the object itself is the relation.
                _ => push_related(entry, acc),
            }
        }
    }
    by_node.retain(|_, v| !v.is_empty());
    if by_node.is_empty() {
        return None;
    }
    serde_json::to_string(&by_node).ok()
}

fn append_resource(
    stage: &TileStage,
    model_slug: &str,
    r: &StaticResource,
    graph: &StaticGraph,
    cfg: &ClusterConfig,
    registry: &ExtensionTypeRegistry,
    value_to_concept: &HashMap<String, String>,
    ng_intervals: &HashMap<String, (i64, i64)>,
) -> Result<(), EmitError> {
    let name = r.resourceinstance.descriptors.name.clone();
    let geo_bbox = first_geo_bbox(r, graph, registry);
    let centroid = geo_bbox.map(|(mnx, mny, mxx, mxy)| ((mnx + mxx) / 2.0, (mny + mxy) / 2.0));
    let key = cluster_key(&cfg.dimensions, centroid, name.as_deref().unwrap_or(""));

    // Entry table (resources_<graphid>.parquet): ONE row per resource — the slug
    // + model-scoping + display-name spine a consumer lists/counts/backlinks on,
    // sorted by resource_id so an id-probe prunes. Written for EVERY resource
    // (even tile-less), before the tiles early-return.
    //
    // The TILE LOCATOR: `cluster_key` is carried here too. Tiles are sorted by
    // `(cluster_key, resource_id)`, so a `WHERE resource_id = X` tile read can't prune
    // (ids scatter across every cluster-key row group → the measured N^1.2–1.4 on entry
    // pages). With the key here — on the resource_id-sorted entry table, which DOES prune —
    // hydrate becomes two pruned reads: resource_id → entry → cluster_key → tiles
    // `WHERE cluster_key = ?`. `key` is already computed above; this just persists it.
    let rid = &r.resourceinstance.resourceinstanceid;
    let gid = &r.resourceinstance.graph_id;
    let legacy = &r.resourceinstance.legacyid;
    {
        let mut ra = stage.conn.appender("resources_stage")?;
        ra.append_row(params![model_slug, rid, gid, legacy, &name, key])?;
    }

    // Grid edge (grid_<graphid>.parquet): one row per (resource, CELL THE FOOTPRINT
    // BBOX OVERLAPS) + the render fields (descriptor, render_summary). Indexing the
    // footprint's whole bbox — not just its centroid — makes `GridBox` a true
    // SUPERSET of "footprint intersects the query box": a building whose footprint
    // reaches into the box is returned even if its centroid lies outside, so a
    // responder NEVER misses an asset in the hazard (over-inclusion is the safe
    // failure; under-inclusion is not). The cell scheme is the SHARED contract
    // (ros_madair_handlers::cells_in_box) emit and the compiler both compute, so a
    // `GridBox`'s `cell IN (…)` can't silently miss. Sorted by cell at write, so a
    // box query reads only the overlapping cells' row groups — and the render data
    // RIDES that prune (the Q1 insight: the render lands on the exact rows the
    // spatial query already touches). A multi-cell footprint yields >1 row, so a
    // reader dedups by resource_id (the compiler's `SELECT DISTINCT` already does).
    if let Some((mnx, mny, mxx, mxy)) = geo_bbox {
        // A building footprint spans O(1) cells; cells_in_box only errs on a
        // degenerate/global box, so fall back to the centroid cell (never drop it).
        let cells = ros_madair_handlers::cells_in_box(mnx, mny, mxx, mxy).unwrap_or_else(|_| {
            vec![ros_madair_handlers::grid_cell((mnx + mxx) / 2.0, (mny + mxy) / 2.0)]
        });
        let summary = build_render_summary(r.cache.as_ref());
        // Location descriptor (geojson Point) — prefer alizarin's `point` descriptor (a
        // true centroid / point-on-surface from the model's descriptor template, sibling of
        // `name`/`map_popup`; available since alizarin-core alpha.140); fall back to the
        // footprint's bbox-centre (`centroid`, already computed above). It rides the same
        // cell-pruned scan as `descriptor`, so a spatial result draws its pins without
        // touching the tiles. bbox-centre is a representative pin, not a true centroid (a
        // concave footprint's can fall just outside it) — fine for display, the safe fallback.
        let geo_descriptor: Option<String> = r
            .resourceinstance
            .descriptors
            .point
            .clone()
            .or_else(|| {
                centroid.map(|(lng, lat)| format!(r#"{{"type":"Point","coordinates":[{lng},{lat}]}}"#))
            });
        let mut ga = stage.conn.appender("grid_stage")?;
        for cell in cells {
            ga.append_row(params![model_slug, cell, rid, &name, &geo_descriptor, &summary])?;
        }
    }

    let Some(tiles) = r.tiles.as_ref() else {
        return Ok(());
    };
    let mut app = stage.conn.appender("stage")?;
    // Edge rows collected across this resource's tiles: one per (linking tile,
    // target). Appended after the tile appender flushes (below) — same links as
    // `link_targets`, unpivoted into the columnar edge table the path/multi-hop
    // compiler and reverse lookups query.
    let mut edge_rows: Vec<(String, String, String, Option<String>, String)> = Vec::new();
    // Melted per-node axis rows, collected across this resource's tiles and appended
    // after the tile appender flushes (exactly like edge_rows). Concepts and dates are
    // no longer crammed into a per-tile concept_ids/q_ordered blob on the tile row —
    // each node's value melts into its own store (concepts_<slug>/ordered_<slug>), so a
    // Concept/Range filter is node-scoped (no cross-node conflation) and range/zone-map
    // prunes instead of scanning the geo-clustered tile row.
    // (resource, tile, nodegroup, node, concept_id)
    let mut concept_rows: Vec<(String, Option<String>, String, String, String)> = Vec::new();
    // (resource, tile, nodegroup, node, q_ordered)
    let mut ordered_rows: Vec<(String, Option<String>, String, String, i64)> = Vec::new();
    for tile in tiles {
        let mut geo = None;
        // Per-NODE link targets `{node_id: [target,...]}` (not just the first Link
        // node's array): a tile's nodegroup can hold >1 link node (e.g. the place
        // graph's name_elements element_entry + concept_entry), and a node-specific
        // query (HasLink / reverse-link geo_points) must tell them apart. Sorted
        // (BTreeMap) so the serialized column is deterministic across emits.
        let mut link_map: std::collections::BTreeMap<&str, Vec<String>> =
            std::collections::BTreeMap::new();

        // `StaticTile.data` is a HashMap — per-run randomized iteration order. A
        // sorted view makes BOTH the `is_none()` "first wins" promotions below
        // AND the serialized `data` column (further down) deterministic run-to-
        // run, the same reason `ros-madair-format`'s `ChunkTile.data` is a
        // `BTreeMap` (see that crate's comment). Without it, content hashes on
        // the parquet output vary between identical emits.
        let mut sorted: Vec<(&String, &serde_json::Value)> = tile.data.iter().collect();
        sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for &(node_id, value) in &sorted {
            if value.is_null() {
                continue;
            }
            let Some(node) = graph.get_node_by_id(node_id) else {
                continue;
            };
            // Arches' reference `to_json()` representation — what business-data bundles
            // carry — wraps each value as `{ list_item_id, labels, list_id, uri }`, with
            // the concept id at the OBJECT level. The canonical stored shape (and hence
            // alizarin's index extractor) keys on the bare list-item UUID. Mirror Arches'
            // own `transform_value_for_tile` (arches_controlled_lists), which reduces that
            // display wrapper to its `list_item_id` on ingest, so a reference indexes into
            // the concept store instead of silently melting to nothing. No-op on an
            // already-canonical value (bare uuid / marker); the stored `data` column keeps
            // the original value, so hydration/display is unaffected.
            let normalized_ref;
            let value: &serde_json::Value = if node.datatype == "reference" {
                normalized_ref = normalize_reference_value(value);
                &normalized_ref
            } else {
                value
            };
            let spec = datatype_index_spec(
                &node.datatype,
                value,
                node_config_value(node).as_ref(),
                Some(registry),
            );
            match spec.class {
                IndexClass::Ordered => {
                    // EVERY ordered node melts (not just the first): one row per node
                    // in the ordered store, sorted (node_id, q_ordered) so a range
                    // predicate zone-map-prunes instead of scanning q_ordered across
                    // the geo-clustered tile row. Keys are RAW; the emitter owns the
                    // quantizer per IndexClass::Ordered's contract — date/edtf →
                    // days-from-civil, number → fixed-point (RM owns `number`;
                    // alizarin-core does not classify it).
                    let q = match node.datatype.as_str() {
                        "date" | "edtf" => spec
                            .keys
                            .first()
                            .and_then(|k| alizarin_core::quantize::quantize_date(k)),
                        "number" => {
                            let decimals =
                                ros_madair_handlers::number_scale(node_config_value(node).as_ref());
                            spec.keys
                                .first()
                                .and_then(|k| k.parse::<f64>().ok())
                                .and_then(|v| ros_madair_handlers::quantize_number(v, decimals))
                        }
                        _ => None,
                    };
                    if let Some(q) = q {
                        ordered_rows.push((
                            tile.resourceinstance_id.clone(),
                            tile.tileid.clone(),
                            tile.nodegroup_id.clone(),
                            node_id.clone(),
                            q,
                        ));
                    }
                }
                IndexClass::SpatialBbox if geo.is_none() => {
                    if let Ok(s) = serde_json::to_string(value) {
                        geo = crate::geo::extract_bbox(&s);
                    }
                }
                IndexClass::ConceptHierarchical { .. } => {
                    // Melt EVERY concept id (each ConceptHierarchical node, each value)
                    // into a node-scoped row (resource, tile, nodegroup, node, concept).
                    // A concept-list node with several values, or two concept nodes in one
                    // nodegroup, each contribute their own rows — so a `Concept` filter is
                    // exact per node, never conflating across nodes the way the old
                    // per-tile concept_ids array did. Resolve value-id → canonical
                    // concept-id, falling back to the raw key for non-controlled values.
                    for k in &spec.keys {
                        let cid = value_to_concept
                            .get(k)
                            .cloned()
                            .unwrap_or_else(|| k.clone());
                        concept_rows.push((
                            tile.resourceinstance_id.clone(),
                            tile.tileid.clone(),
                            tile.nodegroup_id.clone(),
                            node_id.clone(),
                            cid,
                        ));
                    }
                }
                // Links are EXACT here (unlike the head's coarse
                // chunk_link_summary): the tile carries its actual target ids.
                // Keyed per node so a multi-link nodegroup stays distinguishable.
                IndexClass::Link => {
                    if !spec.keys.is_empty() {
                        link_map.insert(node_id.as_str(), spec.keys.clone());
                    }
                }
                _ => {}
            }
        }

        // Unpivot the tile's links into edge rows (src tile → each target). This
        // is now the ONLY consumer of `link_map`: the read path (HasLink,
        // cited_by, geo_points) queries the columnar edge table, not a per-tile
        // `link_targets` JSON column (dropped — hydration reads `data`, not it).
        for (node_id, targets) in &link_map {
            for t in targets {
                edge_rows.push((
                    tile.resourceinstance_id.clone(),
                    (*node_id).to_string(),
                    tile.nodegroup_id.clone(),
                    tile.tileid.clone(),
                    t.clone(),
                ));
            }
        }

        // Unknown nodegroups sort last (sentinel), so a malformed tile never
        // lands inside a real subtree's range.
        let ng_order = ng_intervals
            .get(&tile.nodegroup_id)
            .map(|(pre, _)| *pre)
            .unwrap_or(i64::MAX);
        // Sorted key order (see the sorted view above) so the content-hashed
        // `data` column is byte-identical across runs.
        let mut data_obj: std::collections::BTreeMap<String, serde_json::Value> = sorted
            .iter()
            .map(|(k, v)| ((*k).clone(), (*v).clone()))
            .collect();
        // Render-summary carry (anti-N+1): embed this tile's slice of the resource
        // `__cache` (populate_caches filled `r.cache = {tileId: {nodeId: CacheEntry}}`
        // with each linked target's title + descriptors) under a reserved "__cache"
        // key. Hydrate reads it to resolve related-resource display from the single
        // `data` fetch instead of loading every linked target; it strips the key back
        // out before building StaticTiles. BTreeMap keeps the column byte-identical.
        if let (Some(serde_json::Value::Object(cache)), Some(tid)) =
            (r.cache.as_ref(), tile.tileid.as_deref())
        {
            if let Some(slice) = cache.get(tid) {
                data_obj.insert("__cache".to_string(), slice.clone());
            }
        }
        let data_json = serde_json::to_string(&data_obj)?;

        // Concepts and dates are melted into concepts_<slug>/ordered_<slug> (below),
        // not promoted onto the tile row — so the tile row is the 1:1 axis only
        // (geo bbox, cluster_key, hydration `data`), and facet/range filters hit their
        // own node/value-sorted store instead of scanning this geo-clustered row.
        app.append_row(params![
            model_slug,
            tile.resourceinstance_id,
            name,
            tile.nodegroup_id,
            tile.tileid,
            tile.parenttile_id,
            tile.sortorder,
            key,
            ng_order,
            geo.map(|g| g.0),
            geo.map(|g| g.1),
            geo.map(|g| g.2),
            geo.map(|g| g.3),
            data_json,
        ])?;
    }
    app.flush()?;
    drop(app);
    if !edge_rows.is_empty() {
        let mut eapp = stage.conn.appender("edges")?;
        for (src_resource, src_node, src_nodegroup, src_tile, target) in &edge_rows {
            eapp.append_row(params![
                model_slug,
                src_resource,
                src_node,
                src_nodegroup,
                src_tile,
                target,
            ])?;
        }
        eapp.flush()?;
    }
    if !concept_rows.is_empty() {
        let mut capp = stage.conn.appender("concepts_stage")?;
        for (resource, tile, ng, node, concept) in &concept_rows {
            capp.append_row(params![model_slug, resource, tile, ng, node, concept])?;
        }
        capp.flush()?;
    }
    if !ordered_rows.is_empty() {
        let mut oapp = stage.conn.appender("ordered_stage")?;
        for (resource, tile, ng, node, q) in &ordered_rows {
            oapp.append_row(params![model_slug, resource, tile, ng, node, q])?;
        }
        oapp.flush()?;
    }
    Ok(())
}

/// A file-backed DuckDB staging table for tile rows. Rows STREAM in via the
/// Appender (never held in Rust memory); [`write_model`](Self::write_model) runs
/// `COPY (… ORDER BY …)` — an external, disk-spilling sort — so a full-corpus
/// build is memory-bounded. The same engine that reads the substrate writes it.
/// `model_slug` separates models sharing one stage.
pub(crate) struct TileStage {
    conn: Connection,
}

impl TileStage {
    fn new(scratch: &Path) -> Result<Self, EmitError> {
        fs::create_dir_all(scratch)?;
        let db = scratch.join("stage.duckdb");
        let _ = fs::remove_file(&db);
        let conn = Connection::open(&db).map_err(|e| format!("duckdb open: {e}"))?;
        // The DuckDB memory cap for the staging ingest + the external `COPY … ORDER
        // BY` sort. Default 512MB keeps an on-device emit memory-bounded; a large
        // server-side emit (e.g. the 763k demo corpus, whose 6.8M-tile sort over the
        // fat `data` column exceeds 512MB even with disk spill) raises it via
        // `ROS_MADAIR_EMIT_MEMORY_LIMIT` (any DuckDB size literal, e.g. `8GB`).
        // `ROS_MADAIR_EMIT_THREADS` optionally caps sort threads (lower = less peak
        // memory). Partitioning by nodegroup (CLI `--partition-by-nodegroup`) is the
        // orthogonal lever: it shrinks each sort so a modest cap suffices.
        let mem_limit = std::env::var("ROS_MADAIR_EMIT_MEMORY_LIMIT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "512MB".to_string());
        let threads_pragma = match std::env::var("ROS_MADAIR_EMIT_THREADS") {
            Ok(t) if !t.trim().is_empty() => {
                format!("SET threads={};\n", t.trim().replace('\'', ""))
            }
            _ => String::new(),
        };
        conn.execute_batch(&format!(
            "SET temp_directory='{}';
             SET memory_limit='{mem_limit}';
             {threads_pragma}CREATE TABLE stage (
               model_slug VARCHAR, resource_id VARCHAR, descriptor_name VARCHAR,
               nodegroup_id VARCHAR, tileid VARCHAR, parenttile_id VARCHAR,
               sortorder INTEGER, cluster_key UBIGINT, ng_order BIGINT,
               geo_min_lng DOUBLE, geo_min_lat DOUBLE, geo_max_lng DOUBLE, geo_max_lat DOUBLE,
               data VARCHAR);
             CREATE TABLE edges (
               model_slug VARCHAR, src_resource VARCHAR, src_node VARCHAR,
               src_nodegroup VARCHAR, src_tile VARCHAR, target_resource VARCHAR);
             CREATE TABLE concepts_stage (
               model_slug VARCHAR, resource_id VARCHAR, tile_id VARCHAR,
               nodegroup_id VARCHAR, node_id VARCHAR, concept_id VARCHAR);
             CREATE TABLE ordered_stage (
               model_slug VARCHAR, resource_id VARCHAR, tile_id VARCHAR,
               nodegroup_id VARCHAR, node_id VARCHAR, q_ordered BIGINT);
             CREATE TABLE resources_stage (
               model_slug VARCHAR, resource_id VARCHAR, graph_id VARCHAR,
               legacyid VARCHAR, descriptor VARCHAR, cluster_key UBIGINT);
             CREATE TABLE grid_stage (
               model_slug VARCHAR, cell BIGINT, resource_id VARCHAR,
               descriptor VARCHAR, geo_descriptor VARCHAR, render_summary VARCHAR);",
            sql_lit(&scratch.display().to_string())
        ))
        .map_err(|e| format!("duckdb stage schema: {e}"))?;
        Ok(Self { conn })
    }

    /// `COPY` one model's staged rows to Parquet, sorted (external) by the
    /// cluster key — or, hierarchical, by the nodegroup DFS `pre` first.
    fn write_model(
        &self,
        slug: &str,
        graph_id: &str,
        cfg: &ClusterConfig,
        path: &Path,
    ) -> Result<ParquetModelSummary, EmitError> {
        let order = if cfg.nodegroup_hierarchical_order {
            "ng_order, cluster_key, resource_id, sortorder"
        } else {
            "cluster_key, resource_id, nodegroup_id, sortorder"
        };
        // NB: model_slug is excluded from the output — it is a stage-only routing
        // column, not part of the frozen tile-row schema.
        let cols = "resource_id, descriptor_name, nodegroup_id, tileid, parenttile_id, \
                    sortorder, cluster_key, ng_order, \
                    geo_min_lng, geo_min_lat, geo_max_lng, geo_max_lat, data";
        let slug_lit = sql_lit(slug);
        let rgs = cfg.row_group_size.max(1);
        let count = |sql: &str| -> Result<i64, EmitError> {
            Ok(self.conn.query_row(sql, params![slug], |r| r.get(0))?)
        };
        let tiles = count("SELECT count(*) FROM stage WHERE model_slug = ?")? as usize;
        let resources =
            count("SELECT count(DISTINCT resource_id) FROM stage WHERE model_slug = ?")? as usize;

        let (out_path, glob, partitions) = if cfg.partition_by_nodegroup {
            let dir = path.with_extension(""); // drop `.parquet` → partition root
            let _ = fs::remove_dir_all(&dir);
            // WRITE_PARTITION_COLUMNS keeps `nodegroup_id` inside each file (a
            // plain `read_parquet(glob)` then still has the column), matching the
            // prior layout; the path also carries it (Hive) for partition prune.
            self.conn
                .execute_batch(&format!(
                    "COPY (SELECT {cols} FROM stage WHERE model_slug = '{slug_lit}' ORDER BY {order}) \
                     TO '{}' (FORMAT PARQUET, PARTITION_BY (nodegroup_id), \
                              WRITE_PARTITION_COLUMNS true, ROW_GROUP_SIZE {rgs});",
                    dir.display()
                ))
                .map_err(|e| format!("duckdb COPY (partitioned): {e}"))?;
            let parts =
                count("SELECT count(DISTINCT nodegroup_id) FROM stage WHERE model_slug = ?")?
                    as usize;
            (
                dir.display().to_string(),
                format!("{}/**/*.parquet", dir.display()),
                parts,
            )
        } else {
            if let Some(p) = path.parent() {
                fs::create_dir_all(p)?;
            }
            self.conn
                .execute_batch(&format!(
                    "COPY (SELECT {cols} FROM stage WHERE model_slug = '{slug_lit}' ORDER BY {order}) \
                     TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                    path.display()
                ))
                .map_err(|e| format!("duckdb COPY: {e}"))?;
            (path.display().to_string(), path.display().to_string(), 0)
        };

        // The melted sibling stores share the tiles file's stem so they always track
        // its naming (`tiles_<X>.parquet` → `edges_`/`concepts_`/`ordered_<X>.parquet`),
        // whatever `<X>` is — the caller names the tiles file by graph id, and these
        // follow. Derived from the tiles path once, so there is one source of the stem.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("tiles"))
            .unwrap_or("");
        let sibling = |kind: &str| path.with_file_name(format!("{kind}{stem}.parquet"));

        // Edge table: one row per (linking tile, target), for the path/multi-hop
        // compiler and reverse lookups. Sorted by (src_node, target_resource) so a
        // forward hop (filter src_node, semijoin target) zone-map prunes, and a
        // node-scoped reverse hop (cited_by, which fixes src_node) prunes on the
        // target range WITHIN that node's band. Always a single file — nodegroup
        // partitioning is the TILES layout axis; edges partition by src_node later.
        let edge_path = sibling("edges");
        if let Some(p) = edge_path.parent() {
            fs::create_dir_all(p)?;
        }
        self.conn
            .execute_batch(&format!(
                "COPY (SELECT src_resource, src_node, src_nodegroup, src_tile, target_resource \
                 FROM edges WHERE model_slug = '{slug_lit}' ORDER BY src_node, target_resource) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                edge_path.display()
            ))
            .map_err(|e| format!("duckdb COPY (edges): {e}"))?;
        let edges = count("SELECT count(*) FROM edges WHERE model_slug = ?")? as usize;

        // Reverse edge index: the SAME edge rows sorted (target_resource, src_node),
        // so a TARGET-anchored access prunes on `target_resource`. The forward file
        // is sorted src-first, so its per-row-group target ranges span the whole
        // domain across the node bands — useless for a reverse probe that does NOT
        // fix src_node (an untyped "who links to T?", or a shared-neighbour self-join
        // "resources sharing a related resource", the v1 "same city" pattern). This
        // sibling makes that one a range read. Named `redges_` (NOT `edges_rev_`) so
        // the forward `edges_*` view glob can't swallow it and double-count.
        //
        // Sort is the ONLY prune lever here: the bundled DuckDB 1.1.x Parquet writer
        // emits no Bloom filters (WRITE_BLOOM_FILTER parses but is a no-op, verified),
        // so a scattered id-probe can't prune by hash — only by this target ordering.
        let edge_rev_path = sibling("redges");
        self.conn
            .execute_batch(&format!(
                "COPY (SELECT src_resource, src_node, src_nodegroup, src_tile, target_resource \
                 FROM edges WHERE model_slug = '{slug_lit}' ORDER BY target_resource, src_node) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                edge_rev_path.display()
            ))
            .map_err(|e| format!("duckdb COPY (redges): {e}"))?;

        // Melted concept store: one row per (tile, concept-node, concept value), sorted
        // (node_id, concept_id) so a node-scoped Concept semijoin zone-map-prunes and the
        // DFS-interval catalog range-joins it for ⊑. Its own axis, joined by resource/tile.
        let concept_path = sibling("concepts");
        self.conn
            .execute_batch(&format!(
                "COPY (SELECT resource_id, tile_id, nodegroup_id, node_id, concept_id \
                 FROM concepts_stage WHERE model_slug = '{slug_lit}' ORDER BY node_id, concept_id) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                concept_path.display()
            ))
            .map_err(|e| format!("duckdb COPY (concepts): {e}"))?;
        let concepts = count("SELECT count(*) FROM concepts_stage WHERE model_slug = ?")? as usize;

        // Melted ordered store: one row per (tile, date/edtf node, quantized value), sorted
        // (node_id, q_ordered) so a Range predicate prunes instead of scanning the tile row.
        let ordered_path = sibling("ordered");
        self.conn
            .execute_batch(&format!(
                "COPY (SELECT resource_id, tile_id, nodegroup_id, node_id, q_ordered \
                 FROM ordered_stage WHERE model_slug = '{slug_lit}' ORDER BY node_id, q_ordered) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                ordered_path.display()
            ))
            .map_err(|e| format!("duckdb COPY (ordered): {e}"))?;
        let ordered = count("SELECT count(*) FROM ordered_stage WHERE model_slug = ?")? as usize;

        // Entry table: one row per resource (slug/model/display-name spine),
        // sorted by resource_id so a scattered id-probe prunes to the row groups
        // the ids hit (the only prune available — no Bloom filters). Carries no
        // hydration `data`, so it is small; a consumer lists/counts/backlinks on it.
        let resources_path = sibling("resources");
        self.conn
            .execute_batch(&format!(
                "COPY (SELECT resource_id, graph_id, legacyid, descriptor, cluster_key \
                 FROM resources_stage WHERE model_slug = '{slug_lit}' ORDER BY resource_id) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                resources_path.display()
            ))
            .map_err(|e| format!("duckdb COPY (resources): {e}"))?;

        // Grid edge: one row per geo-bearing resource, sorted by the Z-order cell
        // so a `GridBox`'s `cell IN (…)` reads only the overlapping cells' row
        // groups — and the render fields (descriptor, geo_descriptor, render_summary)
        // ride that prune, so a spatial result renders its cards AND pins without a
        // scattered second fetch (geo_descriptor is the representative point).
        let grid_path = sibling("grid");
        self.conn
            .execute_batch(&format!(
                "COPY (SELECT cell, resource_id, descriptor, geo_descriptor, render_summary \
                 FROM grid_stage WHERE model_slug = '{slug_lit}' ORDER BY cell) \
                 TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE {rgs});",
                grid_path.display()
            ))
            .map_err(|e| format!("duckdb COPY (grid): {e}"))?;
        let grid = count("SELECT count(*) FROM grid_stage WHERE model_slug = ?")? as usize;

        let row_groups: i64 = self
            .conn
            .query_row(
                &format!(
                    "SELECT coalesce(sum(num_row_groups), 0) FROM parquet_file_metadata('{}')",
                    sql_lit(&glob)
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        Ok(ParquetModelSummary {
            slug: slug.to_string(),
            graph_id: graph_id.to_string(),
            path: out_path,
            resources,
            tiles,
            row_groups: row_groups as usize,
            partitions,
            edges,
            concepts,
            ordered,
            grid,
        })
    }
}

/// Emit tile-row Parquet for every model in `data_dir` into `out_dir`, one
/// `tiles_<slug>.parquet` per model. `config_by_graph` maps a graph id to its
/// [`ClusterConfig`]; graphs without an entry get the default. See
/// [`emit_parquet_with_progress`] for the progress-reporting variant.
pub fn emit_parquet(
    data_dir: &str,
    out_dir: &str,
    base_uri: &str,
    registry: &ExtensionTypeRegistry,
    config_by_graph: &HashMap<String, ClusterConfig>,
) -> Result<Vec<ParquetModelSummary>, EmitError> {
    emit_parquet_with_progress(
        data_dir,
        out_dir,
        base_uri,
        registry,
        config_by_graph,
        &mut |_| std::ops::ControlFlow::Continue(()),
    )
}

/// Like [`emit_parquet`], but reports [`EmitProgress`] through `on_progress`.
///
/// Unlike the sqlite `emit_with_progress` (which holds the whole resource `Vec` in
/// memory and so knows `total`), `emit_parquet` STREAMS resources to stay memory-
/// bounded and never learns the total. It therefore reports `Streaming { done,
/// total: 0 }` every [`PROGRESS_STEP`] resources during the (long) ingest, leaving
/// the caller - which already has its own resource count - to map `done` to a
/// fraction. Then it fires `Phase("writing")` once before the per-model
/// `COPY ... ORDER BY -> parquet` external sort, which is a single opaque DuckDB
/// call with no sub-progress. Returning `Break` from `on_progress` cancels the emit.
pub fn emit_parquet_with_progress(
    data_dir: &str,
    out_dir: &str,
    base_uri: &str,
    registry: &ExtensionTypeRegistry,
    config_by_graph: &HashMap<String, ClusterConfig>,
    on_progress: &mut dyn FnMut(crate::EmitProgress) -> std::ops::ControlFlow<()>,
) -> Result<Vec<ParquetModelSummary>, EmitError> {
    let data_dir = Path::new(data_dir);
    let out = Path::new(out_dir);
    fs::create_dir_all(out)?;

    // Build the canonical RDM cache from the collections emit already loads, and
    // project it into (a) the value→concept resolution used for the tile
    // `concept_ids` column and (b) the DFS-ordered concept catalog Parquet that a
    // DuckDB `DescendantOrSelfOf` range-joins.
    let collections = crate::closure::load_collections(data_dir, base_uri)?;
    let mut cache = RdmCache::new();
    cache.add_from_skos_collections(&collections);
    let (catalog_rows, value_to_concept) = build_concept_catalog(&cache);
    if !catalog_rows.is_empty() {
        write_concept_catalog(catalog_rows, &out.join("concept_catalog.parquet"))?;
    }

    let models = crate::input::load_graphs(data_dir)?;
    let by_graph: HashMap<&str, usize> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.graph.graphid.as_str(), i))
        .collect();

    // Per-model config + nodegroup DFS intervals, computed once. A graph with no
    // explicit config gets the default; `ROS_MADAIR_EMIT_PARTITION_BY_NODEGROUP`
    // (CLI `--partition-by-nodegroup`) flips the default's partition flag so a large
    // emit splits each model into per-nodegroup files — each COPY sort is then one
    // nodegroup, not the whole model. An explicit per-graph config (library API)
    // still wins.
    let partition_default = matches!(
        std::env::var("ROS_MADAIR_EMIT_PARTITION_BY_NODEGROUP")
            .unwrap_or_default()
            .trim(),
        "1" | "true" | "on" | "yes"
    );
    let default_cfg = ClusterConfig {
        partition_by_nodegroup: partition_default,
        ..Default::default()
    };
    let cfgs: Vec<ClusterConfig> = models
        .iter()
        .map(|m| {
            config_by_graph
                .get(&m.graph.graphid)
                .cloned()
                .unwrap_or_else(|| default_cfg.clone())
        })
        .collect();
    let ng_intervals: Vec<HashMap<String, (i64, i64)>> = models
        .iter()
        .map(|m| nodegroup_dfs_intervals(m.graph.nodegroups_slice()))
        .collect();

    // STREAM: one shared file-backed stage; parse each business-data file and
    // append its resources' tiles, never accumulating resources or rows. `emit`'s
    // model routing tags each row with `model_slug`.
    let scratch = out.join(".rmstage");
    let stage = TileStage::new(&scratch)?;
    let mut nonempty = vec![false; models.len()];
    // Progress: tick every PROGRESS_STEP resources through the (long) ingest. total
    // is unknown while streaming, so report `done` with total:0; the caller maps it
    // to a fraction against the count it already has. A Break cancels the emit.
    const PROGRESS_STEP: usize = 2000;
    let mut done: usize = 0;
    let mut cancelled = false;

    // Render-summary `__cache` embedding is ON by default (one-fetch hydrate), but
    // can be disabled — it inflates tile `data` ~49% (each linked resource's title +
    // descriptors), which a consumer that composes its own render path does not want,
    // and which tips a very large sorted COPY over the memory cap. Disable with the
    // CLI `--no-render-cache` (which sets this env) or `ROS_MADAIR_EMIT_RENDER_CACHE=0`.
    let render_cache = !matches!(
        std::env::var("ROS_MADAIR_EMIT_RENDER_CACHE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    );

    // PASS 1 (render-summary prerequisite): a lightweight registry of resource
    // SUMMARIES (id → descriptors) over ALL business data, so pass 2's
    // `populate_caches` can resolve a linked target's title without holding every
    // full resource in memory. Summaries only — memory-bounded even at 763k.
    // Skipped entirely when the cache is off.
    let mut summary_registry = StaticResourceRegistry::new();
    if render_cache {
        for path in crate::input::business_data_files(data_dir)? {
            let file = fs::File::open(&path)?;
            let _ = alizarin_core::stream_business_data_resources(file, |_offset, bytes| {
                if let Ok(r) = alizarin_core::parse_business_data_resource_bytes(bytes) {
                    summary_registry.insert_summary(r.to_summary());
                }
                Ok(())
            });
        }
    }

    // STREAM the business-data too: element-by-element, never a whole-file parse.
    // Téarma ships as ONE ~486 MB business_data JSON; `fs::read` + whole-file
    // `parse_business_data_bytes` balloons to multiple GB and OOMs on-device (the
    // tile-WRITE side was already disk-spill-bounded; the READ side was not).
    // `stream_business_data_resources` walks the `resources` array and hands us one
    // element's bytes at a time, so peak memory stays bounded to a single resource
    // plus the disk-spilling DuckDB stage.
    for path in crate::input::business_data_files(data_dir)? {
        if cancelled {
            break;
        }
        let file = fs::File::open(&path)?;
        // Preserve the whole-parse semantics: a *parse* error skips the file
        // (below), but an *append* error is FATAL — capture it out-of-band and
        // re-raise, rather than letting the file-skip swallow it.
        let mut append_err: Option<EmitError> = None;
        let stream_res = alizarin_core::stream_business_data_resources(file, |_offset, bytes| {
            let mut r = alizarin_core::parse_business_data_resource_bytes(bytes)?;
            if let Some(&gi) = by_graph.get(r.resourceinstance.graph_id.as_str()) {
                // PASS 2: fill `r.cache` (__cache) with each linked target's title +
                // descriptors from the pass-1 summary registry. enrich/recompute are
                // OFF — we only want the cache (hydrate re-derives descriptors from
                // the tiles), so the sole `data` change is the added `__cache` key on
                // resources that carry links. Best-effort — a dangling ref just leaves
                // its entry unresolved, never fails the emit. Skipped when the cache
                // is off (then no `__cache` key is embedded at all).
                if render_cache {
                    let _ = summary_registry.populate_caches(
                        std::slice::from_mut(&mut r),
                        &models[gi].graph,
                        false,
                        false,
                        false,
                    );
                }
                if let Err(e) = append_resource(
                    &stage,
                    &models[gi].slug,
                    &r,
                    &models[gi].graph,
                    &cfgs[gi],
                    registry,
                    &value_to_concept,
                    &ng_intervals[gi],
                ) {
                    append_err = Some(e);
                    return Err(alizarin_core::loader::LoaderError::Other(
                        "append failed".into(),
                    ));
                }
                nonempty[gi] = true;
                done += 1;
                if done.is_multiple_of(PROGRESS_STEP)
                    && on_progress(crate::EmitProgress::Streaming { done, total: 0 }).is_break()
                {
                    cancelled = true;
                    return Err(alizarin_core::loader::LoaderError::Other(
                        "emit cancelled".into(),
                    ));
                }
            }
            Ok(())
        });
        if let Some(e) = append_err {
            return Err(e);
        }
        if let Err(e) = stream_res {
            if cancelled {
                return Err("emit cancelled".into());
            }
            eprintln!("skipping {} ({e})", path.display());
            continue;
        }
    }
    // Land the ingest bar on the true resource count before the write phase.
    let _ = on_progress(crate::EmitProgress::Streaming { done, total: 0 });

    let mut summaries = Vec::new();
    // The per-model `COPY ... ORDER BY -> parquet` external sort is one opaque DuckDB
    // call with no sub-progress; report it as a single phase so the UI shows activity
    // ("writing") rather than a frozen bar for the duration of the sort.
    let _ = on_progress(crate::EmitProgress::Phase("writing"));
    for (i, m) in models.iter().enumerate() {
        if !nonempty[i] {
            continue;
        }
        // Name the tile file (and its melted siblings, which derive their stem from
        // it) by GRAPH ID, not the slug: graph id is the stable key consumers index on
        // (the manifest's `model_for_graph`, and the docs' `tiles_<graphid>.parquet`),
        // and it cannot collide the way two same-named graphs' slugs would. Verbatim
        // (hyphens kept) so it matches the graph id everywhere else.
        let path = out.join(format!("tiles_{}.parquet", m.graph.graphid));
        write_ng_sidecar(&ng_intervals[i], &cfgs[i], &path)?;
        summaries.push(stage.write_model(&m.slug, &m.graph.graphid, &cfgs[i], &path)?);
    }
    drop(stage);
    let _ = fs::remove_dir_all(&scratch);

    // Self-describing manifest (A6/I6/P17) — retires the JS-hand-crafted one for
    // the on-device path. Same derivation as the sqlite head: a real snapshot_id
    // over the content-file hashes plus the manifest itself, the ACTUAL handler
    // set (so the query side rebuilds exactly what was indexed, not a guessed
    // default), and the models. `models`/`artifacts` are unused by the duck read
    // path, but `artifacts` is what the id is computed over and a sync client
    // reads, so it is populated, not stubbed.
    let manifest_models = summaries
        .iter()
        .map(|s| ros_madair_format::ModelManifest {
            slug: s.slug.clone(),
            graph_id: s.graph_id.clone(),
            spine_table: format!("tiles_{}", s.slug.replace('-', "_")),
            resource_count: s.resources,
        })
        .collect();
    let artifacts = crate::manifest::hash_parquet_artifacts(out)?;
    let mut manifest = ros_madair_format::Manifest {
        snapshot_id: String::new(),
        format_version: ros_madair_format::FORMAT_VERSION,
        base_uri: base_uri.to_string(),
        tier: None,
        handlers: ros_madair_handlers::describe_registry(registry),
        models: manifest_models,
        artifacts,
        budgets: ros_madair_format::Budgets {
            max_result_rows: 1000,
            max_group_count: 500,
        },
    };
    manifest.snapshot_id = ros_madair_format::snapshot_id(
        &manifest.artifacts,
        &ros_madair_format::manifest_digest_bytes(&manifest)?,
    );
    fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Arches `to_json()` reference shape (what business-data bundles carry):
    // the concept id is a top-level `list_item_id`, labels are plain prefLabels.
    // Normalization must reduce it to the bare id so the canonical reference index
    // keys on it (and melts the reference into the concept store). Then verify the
    // registry path actually extracts that key — the end-to-end contract.
    #[test]
    fn reference_to_json_shape_normalizes_and_indexes() {
        const ITEM: &str = "388293d0-bb7d-5468-91e5-9baa4088b7de";
        let display = serde_json::json!([{
            "list_id": "2730d609-3a8d-49dc-bf51-6ac34e80294a",
            "list_item_id": ITEM,
            "labels": [{ "value": "Listed Building Grade II", "language_id": "en", "valuetype_id": "prefLabel" }]
        }]);
        let normalized = normalize_reference_value(&display);
        assert_eq!(normalized, serde_json::json!([ITEM]));

        let registry = ros_madair_handlers::default_registry();
        let config =
            serde_json::json!({ "controlledList": "2730d609-3a8d-49dc-bf51-6ac34e80294a" });
        let spec = datatype_index_spec("reference", &normalized, Some(&config), Some(&registry));
        assert!(
            matches!(spec.class, IndexClass::ConceptHierarchical { .. }),
            "got {:?}",
            spec.class
        );
        assert_eq!(
            spec.keys,
            vec![ITEM.to_string()],
            "reference must melt on list_item_id"
        );
    }

    // An already-canonical value (bare uuid / {uuid} marker) is untouched.
    #[test]
    fn normalize_reference_value_is_a_noop_on_canonical_shapes() {
        let bare = serde_json::json!(["388293d0-bb7d-5468-91e5-9baa4088b7de"]);
        assert_eq!(normalize_reference_value(&bare), bare);
        let marker = serde_json::json!([{ "uuid": "388293d0-bb7d-5468-91e5-9baa4088b7de" }]);
        assert_eq!(normalize_reference_value(&marker), marker);
    }

    fn ng(id: &str, parent: Option<&str>) -> StaticNodegroup {
        StaticNodegroup {
            nodegroupid: id.to_string(),
            cardinality: Some("1".to_string()),
            parentnodegroup_id: parent.map(String::from),
            legacygroupid: None,
            grouping_node_id: None,
        }
    }

    /// The defining property of the DFS-interval labels: a nodegroup is in X's
    /// subtree IFF its `pre` lies in `[pre_X, submax_X]`. This is exactly what
    /// makes "X and its children" a single contiguous range once tiles are sorted
    /// by `pre`. Tree:  A -> {B -> {D, E}, C}   and a separate root F.
    #[test]
    fn dfs_intervals_make_a_subtree_a_contiguous_range() {
        let ngs = vec![
            ng("A", None),
            ng("B", Some("A")),
            ng("C", Some("A")),
            ng("D", Some("B")),
            ng("E", Some("B")),
            ng("F", None),
        ];
        let iv = nodegroup_dfs_intervals(&ngs);

        // A's subtree is {A,B,C,D,E}; F is outside it.
        let (pre_a, submax_a) = iv["A"];
        let in_a = |n: &str| {
            let (p, _) = iv[n];
            p >= pre_a && p <= submax_a
        };
        for n in ["A", "B", "C", "D", "E"] {
            assert!(
                in_a(n),
                "{n} must fall in A's interval [{pre_a},{submax_a}]"
            );
        }
        assert!(!in_a("F"), "F is a separate root, not in A's subtree");

        // B's subtree is {B,D,E}; A and C are outside it.
        let (pre_b, submax_b) = iv["B"];
        let in_b = |n: &str| {
            let (p, _) = iv[n];
            p >= pre_b && p <= submax_b
        };
        for n in ["B", "D", "E"] {
            assert!(in_b(n), "{n} in B's subtree");
        }
        assert!(!in_b("A") && !in_b("C"), "A and C are not under B");

        // Leaves are point intervals.
        assert_eq!(iv["D"].0, iv["D"].1);
        assert_eq!(iv["F"].0, iv["F"].1);
    }

    /// The concept catalog projected from an in-memory `RdmCache`: a concept
    /// subtree is a contiguous `dfs_enter` range — what makes `DescendantOrSelfOf`
    /// a single DuckDB range read. Tree: root → { a → { a1 }, b }.
    #[test]
    fn concept_catalog_dfs_intervals_make_a_subtree_a_range() {
        use alizarin_core::rdm_cache::{RdmCache, RdmCollection, RdmConcept};
        fn c(id: &str, broader: &[&str], narrower: &[&str]) -> RdmConcept {
            RdmConcept {
                id: id.to_string(),
                // alizarin a130+ separated id/uri and made pref_label a BTreeMap.
                uri: None,
                sort_order: None,
                pref_label: std::collections::BTreeMap::new(),
                alt_labels: HashMap::new(),
                broader: broader.iter().map(|s| s.to_string()).collect(),
                narrower: narrower.iter().map(|s| s.to_string()).collect(),
                scope_note: HashMap::new(),
            }
        }
        let mut coll = RdmCollection::new("c1".to_string());
        coll.add_concept(c("root", &[], &["a", "b"]));
        coll.add_concept(c("a", &["root"], &["a1"]));
        coll.add_concept(c("a1", &["a"], &[]));
        coll.add_concept(c("b", &["root"], &[]));
        let mut cache = RdmCache::new();
        cache.add_collection(coll);

        let (rows, _v2c) = build_concept_catalog(&cache);
        let iv: HashMap<String, (i64, i64)> = rows
            .iter()
            .map(|r| (r.concept_id.clone(), (r.dfs_enter, r.dfs_leave)))
            .collect();
        let inside = |anc: &str, d: &str| {
            let (e, l) = iv[anc];
            let (de, _) = iv[d];
            de >= e && de <= l
        };
        for d in ["root", "a", "a1", "b"] {
            assert!(inside("root", d), "{d} in root's subtree");
        }
        assert!(inside("a", "a1"), "a1 under a");
        assert!(
            !inside("a", "b") && !inside("a", "root"),
            "b/root not under a"
        );
        assert_eq!(iv["a1"].0, iv["a1"].1, "leaf is a point interval");
    }
}
