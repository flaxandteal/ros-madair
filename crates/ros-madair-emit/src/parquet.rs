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
//!   - **promoted typed columns** (`q_ordered`, `concept_id`, `geo_*`) are the
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
use alizarin_core::graph::{StaticGraph, StaticNode, StaticNodegroup, StaticResource};
use alizarin_core::rdm_cache::{RdmCache, RdmCollection};

use arrow::array::{Float64Array, Int32Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

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
#[derive(Debug, Clone)]
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
            Some(p) if ids.contains(p.as_str()) => {
                children.entry(p.as_str()).or_default().push(&ng.nodegroupid)
            }
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
        let mut tops: Vec<String> = coll.get_top_concepts().iter().map(|c| c.id.clone()).collect();
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
fn write_concept_catalog(mut rows: Vec<ConceptCatalogRow>, path: &Path) -> Result<usize, EmitError> {
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
            Arc::new(StringArray::from_iter(rows.iter().map(|r| Some(r.concept_id.as_str())))),
            Arc::new(Int64Array::from_iter(rows.iter().map(|r| Some(r.dfs_enter)))),
            Arc::new(Int64Array::from_iter(rows.iter().map(|r| Some(r.dfs_leave)))),
            Arc::new(StringArray::from_iter(rows.iter().map(|r| Some(r.label.as_str())))),
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
        for (node_id, value) in &tile.data {
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
// Row assembly + write
// ---------------------------------------------------------------------------

struct Row {
    cluster_key: u64,
    /// The tile's nodegroup DFS pre-order index (its subtree-range coordinate).
    ng_order: i64,
    resource_id: String,
    descriptor_name: Option<String>,
    nodegroup_id: String,
    tileid: Option<String>,
    parenttile_id: Option<String>,
    sortorder: Option<i32>,
    q_ordered: Option<i64>,
    concept_id: Option<String>,
    /// Exact link target ids as a JSON array string (null if the tile has no
    /// link node), so `HasLink` tests membership precisely.
    link_targets: Option<String>,
    geo: Option<(f64, f64, f64, f64)>,
    data_json: String,
}

/// Write one model's tiles to `<out>/tiles_<slug>.parquet`.
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
    let mut rows: Vec<Row> = Vec::new();
    let ng_intervals = nodegroup_dfs_intervals(graph.nodegroups_slice());

    for r in resources {
        let name = r.resourceinstance.descriptors.name.clone();
        let centroid = first_geo_bbox(r, graph, registry)
            .map(|(mnx, mny, mxx, mxy)| ((mnx + mxx) / 2.0, (mny + mxy) / 2.0));
        let key = cluster_key(&cfg.dimensions, centroid, name.as_deref().unwrap_or(""));

        let Some(tiles) = r.tiles.as_ref() else {
            continue;
        };
        for tile in tiles {
            let mut q_ordered = None;
            let mut concept_id = None;
            let mut geo = None;
            let mut link_targets: Option<String> = None;

            for (node_id, value) in &tile.data {
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
                match spec.class {
                    IndexClass::Ordered if q_ordered.is_none() => {
                        // date/edtf → days-from-civil, the same quantizer the head used.
                        if matches!(node.datatype.as_str(), "date" | "edtf") {
                            q_ordered = spec
                                .keys
                                .first()
                                .and_then(|k| alizarin_core::quantize::quantize_date(k));
                        }
                    }
                    IndexClass::SpatialBbox if geo.is_none() => {
                        if let Ok(s) = serde_json::to_string(value) {
                            geo = crate::geo::extract_bbox(&s);
                        }
                    }
                    IndexClass::ConceptHierarchical { .. } if concept_id.is_none() => {
                        // Resolve value-id → concept-id (canonical), falling back
                        // to the raw key when it is not a controlled-list value.
                        concept_id = spec.keys.first().map(|k| {
                            value_to_concept.get(k).cloned().unwrap_or_else(|| k.clone())
                        });
                    }
                    // Links are EXACT here (unlike the head's coarse
                    // chunk_link_summary): the tile carries its actual target
                    // ids. Store them as a JSON array so a HasLink query can test
                    // membership precisely.
                    IndexClass::Link if link_targets.is_none() => {
                        if !spec.keys.is_empty() {
                            link_targets = Some(serde_json::to_string(&spec.keys)?);
                        }
                    }
                    _ => {}
                }
            }

            // Unknown nodegroups sort last (sentinel), so a malformed tile never
            // lands inside a real subtree's range.
            let ng_order = ng_intervals
                .get(&tile.nodegroup_id)
                .map(|(pre, _)| *pre)
                .unwrap_or(i64::MAX);

            rows.push(Row {
                cluster_key: key,
                ng_order,
                resource_id: tile.resourceinstance_id.clone(),
                descriptor_name: name.clone(),
                nodegroup_id: tile.nodegroup_id.clone(),
                tileid: tile.tileid.clone(),
                parenttile_id: tile.parenttile_id.clone(),
                sortorder: tile.sortorder,
                q_ordered,
                concept_id,
                link_targets,
                geo,
                data_json: serde_json::to_string(&tile.data)?,
            });
        }
    }

    // The physical row order IS the locality. Two primary axes:
    //  - hierarchical: nodegroup DFS `pre` first, so a subtree is one contiguous
    //    range (single range read for "X and its children");
    //  - default: the cluster key first, keeping a resource's tiles contiguous
    //    and geo/name zone-maps tight.
    if cfg.nodegroup_hierarchical_order {
        rows.sort_by(|a, b| {
            a.ng_order
                .cmp(&b.ng_order)
                .then_with(|| a.cluster_key.cmp(&b.cluster_key))
                .then_with(|| a.resource_id.cmp(&b.resource_id))
                .then_with(|| a.sortorder.cmp(&b.sortorder))
        });
    } else {
        rows.sort_by(|a, b| {
            a.cluster_key
                .cmp(&b.cluster_key)
                .then_with(|| a.resource_id.cmp(&b.resource_id))
                .then_with(|| a.nodegroup_id.cmp(&b.nodegroup_id))
                .then_with(|| a.sortorder.cmp(&b.sortorder))
        });
    }

    // The subtree-interval sidecar: a consumer maps "nodegroup X" -> [pre, submax]
    // and reads the single `ng_order BETWEEN pre AND submax` range.
    if cfg.nodegroup_hierarchical_order {
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
        fs::write(&sidecar, serde_json::to_vec_pretty(&serde_json::Value::Object(obj))?)?;
    }

    let tiles = rows.len();
    let (out_path, row_groups, partitions) = if cfg.partition_by_nodegroup {
        // Nodegroup as a PARTITION axis: one file per nodegroup under a
        // Hive-partitioned directory, each internally sorted by the cluster key.
        // `rows` is already globally cluster-sorted, so per-partition order is a
        // stable sub-sequence — no re-sort needed.
        let dir = path.with_extension(""); // drop `.parquet` -> the partition root
        let _ = fs::remove_dir_all(&dir);
        let mut by_ng: HashMap<&str, Vec<&Row>> = HashMap::new();
        for r in &rows {
            by_ng.entry(r.nodegroup_id.as_str()).or_default().push(r);
        }
        // Deterministic partition order.
        let mut ngs: Vec<&str> = by_ng.keys().copied().collect();
        ngs.sort_unstable();
        let mut total_rg = 0usize;
        for ng in &ngs {
            let part_dir = dir.join(format!("nodegroup_id={ng}"));
            fs::create_dir_all(&part_dir)?;
            total_rg += write_rows(by_ng[ng].iter().copied(), cfg.row_group_size, &part_dir.join("tiles.parquet"))?;
        }
        (dir.display().to_string(), total_rg, ngs.len())
    } else {
        let rg = write_rows(rows.iter(), cfg.row_group_size, path)?;
        (path.display().to_string(), rg, 0)
    };

    Ok(ParquetModelSummary {
        slug: slug.to_string(),
        graph_id: graph.graphid.clone(),
        path: out_path,
        resources: resources.len(),
        tiles,
        row_groups,
        partitions,
    })
}

/// The frozen tile-row schema — identical for the single-file and partitioned
/// layouts, so a reader treats a partition file and a whole-model file alike.
fn tile_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("resource_id", DataType::Utf8, false),
        Field::new("descriptor_name", DataType::Utf8, true),
        Field::new("nodegroup_id", DataType::Utf8, false),
        Field::new("tileid", DataType::Utf8, true),
        Field::new("parenttile_id", DataType::Utf8, true),
        Field::new("sortorder", DataType::Int32, true),
        Field::new("cluster_key", DataType::UInt64, false),
        Field::new("ng_order", DataType::Int64, false),
        Field::new("q_ordered", DataType::Int64, true),
        Field::new("concept_id", DataType::Utf8, true),
        Field::new("link_targets", DataType::Utf8, true),
        Field::new("geo_min_lng", DataType::Float64, true),
        Field::new("geo_min_lat", DataType::Float64, true),
        Field::new("geo_max_lng", DataType::Float64, true),
        Field::new("geo_max_lat", DataType::Float64, true),
        Field::new("data", DataType::Utf8, false),
    ]))
}

/// Write a run of rows (already in the intended physical order) to one Parquet
/// file with `row_group_size` as the chunk size. Returns the row-group count.
fn write_rows<'a>(
    rows: impl Iterator<Item = &'a Row> + Clone,
    row_group_size: usize,
    path: &Path,
) -> Result<usize, EmitError> {
    let schema = tile_schema();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter(rows.clone().map(|r| Some(r.resource_id.as_str())))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| r.descriptor_name.as_deref()))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| Some(r.nodegroup_id.as_str())))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| r.tileid.as_deref()))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| r.parenttile_id.as_deref()))),
            Arc::new(Int32Array::from_iter(rows.clone().map(|r| r.sortorder))),
            Arc::new(UInt64Array::from_iter(rows.clone().map(|r| Some(r.cluster_key)))),
            Arc::new(Int64Array::from_iter(rows.clone().map(|r| Some(r.ng_order)))),
            Arc::new(Int64Array::from_iter(rows.clone().map(|r| r.q_ordered))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| r.concept_id.as_deref()))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| r.link_targets.as_deref()))),
            Arc::new(Float64Array::from_iter(rows.clone().map(|r| r.geo.map(|g| g.0)))),
            Arc::new(Float64Array::from_iter(rows.clone().map(|r| r.geo.map(|g| g.1)))),
            Arc::new(Float64Array::from_iter(rows.clone().map(|r| r.geo.map(|g| g.2)))),
            Arc::new(Float64Array::from_iter(rows.clone().map(|r| r.geo.map(|g| g.3)))),
            Arc::new(StringArray::from_iter(rows.clone().map(|r| Some(r.data_json.as_str())))),
        ],
    )?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(path)?;
    let props = WriterProperties::builder()
        .set_max_row_group_size(row_group_size.max(1))
        .build();
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
    writer.write(&batch)?;
    let meta = writer.close()?;
    Ok(meta.row_groups.len())
}

/// Emit tile-row Parquet for every model in `data_dir` into `out_dir`, one
/// `tiles_<slug>.parquet` per model. `config_by_graph` maps a graph id to its
/// [`ClusterConfig`]; graphs without an entry get the default.
pub fn emit_parquet(
    data_dir: &str,
    out_dir: &str,
    base_uri: &str,
    registry: &ExtensionTypeRegistry,
    config_by_graph: &HashMap<String, ClusterConfig>,
) -> Result<Vec<ParquetModelSummary>, EmitError> {
    let data_dir = Path::new(data_dir);
    let out = Path::new(out_dir);
    fs::create_dir_all(out)?;

    // Build the canonical RDM cache from the collections emit already loads, and
    // project it into (a) the value→concept resolution used for the tile
    // `concept_id` column and (b) the DFS-ordered concept catalog Parquet that a
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

    let mut resources_by_model: Vec<Vec<StaticResource>> = (0..models.len()).map(|_| Vec::new()).collect();
    for path in crate::input::business_data_files(data_dir)? {
        let bytes = fs::read(&path)?;
        let parsed = match alizarin_core::parse_business_data_bytes(&bytes) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping {} ({e})", path.display());
                continue;
            }
        };
        for r in parsed {
            if let Some(&gi) = by_graph.get(r.resourceinstance.graph_id.as_str()) {
                resources_by_model[gi].push(r);
            }
        }
    }

    let mut summaries = Vec::new();
    for (i, m) in models.iter().enumerate() {
        if resources_by_model[i].is_empty() {
            continue;
        }
        let cfg = config_by_graph.get(&m.graph.graphid).cloned().unwrap_or_default();
        let path = out.join(format!("tiles_{}.parquet", m.slug.replace('-', "_")));
        summaries.push(write_model_parquet(
            &m.slug,
            &m.graph,
            &resources_by_model[i],
            &cfg,
            registry,
            &value_to_concept,
            &path,
        )?);
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(in_a(n), "{n} must fall in A's interval [{pre_a},{submax_a}]");
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
                pref_label: HashMap::new(),
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
        assert!(!inside("a", "b") && !inside("a", "root"), "b/root not under a");
        assert_eq!(iv["a1"].0, iv["a1"].1, "leaf is a point interval");
    }
}
