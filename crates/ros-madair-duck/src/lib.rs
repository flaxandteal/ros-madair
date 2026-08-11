// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Slice 2 of the DuckDB+Parquet substrate: the shared structured read path.**
//!
//! Compiles the Rós Madair [`Query`] IR — the SAME typed query the head-SQL
//! compiler takes — to DuckDB SQL over the tile-row Parquet that
//! `ros-madair-emit`'s `parquet` module writes. One IR, two compilers: the head
//! path and this one answer the same queries, so a consumer picks a backend
//! without rewriting its queries.
//!
//! The headline difference from the head path: **spatial is exact here.** The
//! head returned a bbox-overlap SUPERSET and left the exact intersection to an
//! unimplemented client step; this coarse-prunes on the promoted `geo_*` columns
//! (the zone-map) and then runs `ST_Intersects` on the geometry parsed out of the
//! tile `data` blob — the fine step, actually done.
//!
//! Slice-2 scope (called out, not hidden):
//!   - `Concept` supports `Is` (exact) always, and `DescendantOrSelfOf` when a
//!     concept catalog is attached (`open_with_catalog`) — a DFS-interval range
//!     read over the catalog projected from `RdmCache`. Works for `concept` AND
//!     `reference` (shared `ConceptHierarchical` class).
//!   - `HasLink` is EXACT — the emitter promotes the tile's actual target ids
//!     as a per-NODE object `link_targets` = `{node_id: [targets]}`, so membership
//!     is precise even when a nodegroup holds >1 link node (the head was coarse
//!     here). `geo_points` uses the same per-node object for reverse lookups.
//!   - `path` resolves a bare alias; dot-qualified paths are a later increment.
//!   - `concept_id` promotion is still "first indexed node of a class per tile", so
//!     a nodegroup with two CONCEPT nodes is not yet distinguished (links are now
//!     per-node; concepts would need the same treatment).

use std::collections::HashMap;
use std::path::Path;

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode};
use alizarin_core::StaticTile;
use duckdb::Connection;
use ros_madair_query::{ConceptOp, Expr, Query};

/// Apply the spatial-extension source to a fresh connection. Best-effort for the
/// online path (a non-spatial query still works if it fails); for the offline
/// path it disables autoinstall so a missing local binary is a loud error, not a
/// silent network fetch.
fn configure_spatial(conn: &Connection, spatial: &SpatialSource) {
    match spatial {
        SpatialSource::None => {}
        SpatialSource::Auto => {
            let _ = conn.execute_batch("INSTALL spatial; LOAD spatial;");
        }
        SpatialSource::OfflineDir(dir) => {
            let _ = conn.execute_batch(&format!(
                "SET autoinstall_known_extensions=false; \
                 SET extension_directory='{}'; \
                 LOAD spatial;",
                sql_lit(&dir.display().to_string())
            ));
        }
    }
}

/// Why a DuckDB read failed.
#[derive(Debug)]
pub enum DuckError {
    /// The underlying DuckDB engine errored (open, SQL, extension load).
    Duck(duckdb::Error),
    /// The query could not be compiled to DuckDB SQL (typed/schema error).
    Compile(String),
}

impl std::fmt::Display for DuckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DuckError::Duck(e) => write!(f, "duckdb: {e}"),
            DuckError::Compile(m) => write!(f, "compile: {m}"),
        }
    }
}
impl std::error::Error for DuckError {}
impl From<duckdb::Error> for DuckError {
    fn from(e: duckdb::Error) -> Self {
        DuckError::Duck(e)
    }
}

/// A DuckDB connection with a `tiles` view over one model's Parquet, and
/// optionally a `concepts` view over the DFS-ordered concept catalog.
pub struct DuckReader {
    conn: Connection,
    has_catalog: bool,
}

/// A resource's search display (from [`DuckReader::search_display`]): headword plus
/// POS and dialect concept labels.
#[derive(Debug, Default, Clone)]
pub struct SearchRow {
    pub headword: Option<String>,
    pub pos: Option<String>,
    pub dialects: Vec<String>,
}

/// Where DuckDB's spatial extension comes from. `json` is statically bundled
/// (cargo feature) so it needs no source; `parquet` reading is core. Spatial is
/// the one extension that cannot be cargo-bundled.
pub enum SpatialSource<'a> {
    /// Best-effort network `INSTALL spatial` (dev / online).
    Auto,
    /// **Offline**: load `spatial.duckdb_extension` from a local
    /// `extension_directory` (`<dir>/<version>/<platform>/spatial.duckdb_extension`)
    /// with autoinstall disabled — no network. This is the on-device / mobile
    /// path: the app ships the platform's spatial binary and points here.
    OfflineDir(&'a Path),
    /// Skip spatial — non-spatial queries only (a `Bbox` will then error at the
    /// DuckDB layer rather than silently mis-answer).
    None,
}

impl DuckReader {
    /// Open a reader over a Parquet path/glob (a single `tiles_<slug>.parquet`,
    /// or a Hive-partitioned glob), installing spatial over the network if
    /// needed. For offline/on-device use [`open_offline`](Self::open_offline).
    pub fn open(parquet_glob: &str) -> Result<Self, DuckError> {
        Self::open_with(parquet_glob, SpatialSource::Auto)
    }

    /// Open for **offline** use: json is bundled (compiled in), and spatial loads
    /// from `extension_dir` with no network. Parquet reading is core. This is the
    /// deployment path for a bundled app.
    pub fn open_offline(parquet_glob: &str, extension_dir: &Path) -> Result<Self, DuckError> {
        Self::open_with(parquet_glob, SpatialSource::OfflineDir(extension_dir))
    }

    /// Open with an explicit spatial source.
    pub fn open_with(parquet_glob: &str, spatial: SpatialSource) -> Result<Self, DuckError> {
        let conn = Connection::open_in_memory()?;
        // json_extract* are available with NO INSTALL — the `json` cargo feature
        // statically links the JSON extension into libduckdb. parquet is core.
        configure_spatial(&conn, &spatial);
        conn.execute_batch(&format!(
            "CREATE VIEW tiles AS SELECT * FROM read_parquet('{}');",
            sql_lit(parquet_glob)
        ))?;
        Ok(Self { conn, has_catalog: false })
    }

    /// Attach a concept catalog (the `concept_catalog.parquet` projected from
    /// `RdmCache`), enabling `DescendantOrSelfOf` — a subtree range read
    /// (`dfs_enter BETWEEN pre_X AND submax_X`), zone-map-pruned to the subtree.
    pub fn with_catalog(mut self, catalog_glob: &str) -> Result<Self, DuckError> {
        self.conn.execute_batch(&format!(
            "CREATE VIEW concepts AS SELECT * FROM read_parquet('{}');",
            sql_lit(catalog_glob)
        ))?;
        self.has_catalog = true;
        Ok(self)
    }

    /// Convenience: [`open`](Self::open) + [`with_catalog`](Self::with_catalog).
    pub fn open_with_catalog(parquet_glob: &str, catalog_glob: &str) -> Result<Self, DuckError> {
        Self::open(parquet_glob)?.with_catalog(catalog_glob)
    }

    /// The label for a concept id, from the catalog (unblocks `v2_closure`-style
    /// display). `None` if no catalog is attached or the concept is unknown.
    pub fn concept_label(&self, concept_id: &str) -> Result<Option<String>, DuckError> {
        if !self.has_catalog {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT label FROM concepts WHERE concept_id = ?1 LIMIT 1")?;
        let mut rows = stmt.query_map([concept_id], |r| r.get::<_, Option<String>>(0))?;
        match rows.next() {
            Some(r) => Ok(r?),
            None => Ok(None),
        }
    }

    /// Per-resource descriptor (display name) for a set of resource ids, read from
    /// the promoted `descriptor_name` tile column. The Parquet counterpart of the
    /// sqlite head's `spine.display_name` join (Gréasán `v2_descriptors`): a flat
    /// column read instead of a spine⨝dict join. Ids with no non-empty descriptor
    /// are simply absent from the map.
    pub fn descriptors(&self, uris: &[String]) -> Result<HashMap<String, String>, DuckError> {
        let mut out = HashMap::new();
        if uris.is_empty() {
            return Ok(out);
        }
        let placeholders = std::iter::repeat("?")
            .take(uris.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT resource_id, descriptor_name FROM tiles \
             WHERE descriptor_name IS NOT NULL AND descriptor_name <> '' \
             AND resource_id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(duckdb::params_from_iter(uris.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, name) = row?;
            out.entry(id).or_insert(name);
        }
        Ok(out)
    }

    /// Canonical search display per resource: headword (`descriptor_name`), plus
    /// the POS + dialect concept labels. POS and each dialect are top-level
    /// single-concept nodegroups, so each is its own tile: match tiles by
    /// `nodegroup_id` (= the node id) and join `concept_id` to the catalog label.
    /// POS/dialects need the concept catalog (`with_catalog`); without it only
    /// headwords come back.
    pub fn search_display(
        &self,
        uris: &[String],
        pos_node: &str,
        dialect_node: &str,
    ) -> Result<HashMap<String, SearchRow>, DuckError> {
        let mut out: HashMap<String, SearchRow> = HashMap::new();
        if uris.is_empty() {
            return Ok(out);
        }
        let ph = std::iter::repeat("?").take(uris.len()).collect::<Vec<_>>().join(",");

        // Headword (descriptor_name), one per resource.
        let sql = format!(
            "SELECT resource_id, descriptor_name FROM tiles \
             WHERE descriptor_name IS NOT NULL AND descriptor_name <> '' \
             AND resource_id IN ({ph})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(duckdb::params_from_iter(uris.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, name) = row?;
            out.entry(id).or_default().headword.get_or_insert(name);
        }

        if !self.has_catalog {
            return Ok(out);
        }

        // POS + dialect concept labels: tiles of the given nodegroup ⨝ catalog.
        // params = [node, uris...]; dialects are cardinality-n so collect a Vec.
        let node_query = |node: &str| -> Result<Vec<(String, String)>, DuckError> {
            let sql = format!(
                "SELECT t.resource_id, c.label FROM tiles t \
                 JOIN concepts c ON c.concept_id = t.concept_id \
                 WHERE t.nodegroup_id = ? AND c.label IS NOT NULL \
                 AND t.resource_id IN ({ph})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params = std::iter::once(node.to_string()).chain(uris.iter().cloned());
            let rows = stmt.query_map(duckdb::params_from_iter(params), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        };

        for (id, label) in node_query(pos_node)? {
            out.entry(id).or_default().pos.get_or_insert(label);
        }
        for (id, label) in node_query(dialect_node)? {
            out.entry(id).or_default().dialects.push(label);
        }
        Ok(out)
    }

    /// All `(concept_id, label)` from the catalog - the bulk concept→label dump
    /// (Gréasán `v2_closure`, for on-device reference rendering). Empty without a
    /// catalog attached.
    pub fn concept_labels(&self) -> Result<HashMap<String, String>, DuckError> {
        let mut out = HashMap::new();
        if !self.has_catalog {
            return Ok(out);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT concept_id, label FROM concepts WHERE label IS NOT NULL AND label <> ''")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, label) = row?;
            out.insert(id, label);
        }
        Ok(out)
    }

    /// Reverse-link geo lookup (Gréasán `v2_geo_points`, MapView): every resource
    /// that links to `target` through node `node_id`, with its descriptor + point
    /// (`geo_min_lat`/`geo_min_lng`). Node-precise via the per-node `link_targets`
    /// object. The link and the geometry live on different tiles of the same
    /// resource, so self-join on `resource_id`.
    pub fn geo_points(
        &self,
        node_id: &str,
        target: &str,
    ) -> Result<Vec<(String, String, f64, f64)>, DuckError> {
        let sql = format!(
            "SELECT l.resource_id, any_value(l.descriptor_name), \
                    any_value(g.geo_min_lat), any_value(g.geo_min_lng) \
             FROM tiles l \
             JOIN tiles g ON g.resource_id = l.resource_id AND g.geo_min_lat IS NOT NULL \
             WHERE json_contains(json_extract(l.link_targets, '$.\"{}\"'), '\"{}\"') \
             GROUP BY l.resource_id",
            sql_lit(node_id),
            sql_lit(target)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Reconstruct a resource's tiles from the Parquet `data` column - the
    /// hydration tile source, replacing the msgpack chunk read. The `data` column
    /// is the tile's `{node_id: value}` JSON; the other columns give the tile
    /// tree structure (nodegroup, tileid, parent, sortorder).
    pub fn resource_tiles(&self, uuid: &str) -> Result<Vec<StaticTile>, DuckError> {
        let mut stmt = self.conn.prepare(
            "SELECT nodegroup_id, tileid, parenttile_id, sortorder, data \
             FROM tiles WHERE resource_id = ?",
        )?;
        let rows = stmt.query_map([uuid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i32>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut tiles = Vec::new();
        for row in rows {
            let (nodegroup_id, tileid, parenttile_id, sortorder, data_json) = row?;
            let data = serde_json::from_str(&data_json)
                .map_err(|e| DuckError::Compile(format!("tile data JSON: {e}")))?;
            tiles.push(StaticTile {
                data,
                nodegroup_id,
                resourceinstance_id: uuid.to_string(),
                tileid,
                parenttile_id,
                provisionaledits: None,
                sortorder,
            });
        }
        Ok(tiles)
    }

    /// Hydrate a resource to a display JSON tree from Parquet: tiles from the
    /// `data` column ([`resource_tiles`]) + concept labels from the catalog, fed
    /// to ros-madair-read's storage-agnostic `hydrate_tiles_with_labels` (the
    /// tile→tree half of hydration). The catalog must be attached for labels.
    pub fn hydrate(
        &self,
        uuid: &str,
        graph: &StaticGraph,
        languages: &[&str],
    ) -> Result<serde_json::Value, DuckError> {
        let tiles = self.resource_tiles(uuid)?;
        let labels = self.concept_labels()?;
        ros_madair_read::hydrate_tiles_with_labels(&tiles, uuid, graph, &labels, languages)
            .map_err(|e| DuckError::Compile(format!("hydrate: {e}")))
    }

    /// Resolve a query to the sorted set of matching resource ids.
    pub fn resolve_ids(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: &ExtensionTypeRegistry,
    ) -> Result<Vec<String>, DuckError> {
        let sql = format!(
            "SELECT resource_id FROM ({}) t ORDER BY resource_id",
            self.matching_ids_select(query, graph, registry)?
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for r in rows {
            ids.push(r?);
        }
        Ok(ids)
    }

    /// Count the matching resources (the `CountRecords` measure): the same
    /// Expr→SQL compile as `resolve_ids`, wrapped in `COUNT(*)`.
    pub fn count_records(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: &ExtensionTypeRegistry,
    ) -> Result<usize, DuckError> {
        let sql = format!(
            "SELECT COUNT(*) FROM ({}) t",
            self.matching_ids_select(query, graph, registry)?
        );
        let n: i64 = self.conn.query_row(&sql, [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// The `SELECT DISTINCT resource_id` subquery for a query's WHERE - shared by
    /// `resolve_ids` and `count_records` so both measures compile the IR identically.
    fn matching_ids_select(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: &ExtensionTypeRegistry,
    ) -> Result<String, DuckError> {
        match &query.r#where {
            Some(expr) => compile_expr(expr, graph, registry, self.has_catalog),
            None => Ok("SELECT DISTINCT resource_id FROM tiles".to_string()),
        }
    }
}

/// Multi-layer composed hydration from Parquet - the counterpart of
/// ros-madair-read's `Layers::hydrate_resource`. Gathers a resource's tiles from
/// every layer that has it (TOPMOST first, so `merge_resources`' first-wins ==
/// topmost-wins), merges with per-nodegroup precedence, folds concept labels
/// base-first (topmost label wins on overwrite), and hydrates. `dirs` is base-first
/// (the app's composition order). Same composition as the sqlite `Layers` path;
/// only the tile source (the Parquet `data` column) differs.
pub fn hydrate_layers(
    dirs: &[&Path],
    uuid: &str,
    graph: &StaticGraph,
    languages: &[&str],
) -> Result<serde_json::Value, DuckError> {
    use alizarin_core::graph::{
        merge_resources, unify_cardinality_one_tiles, TileMergeMode,
    };

    let open_layer = |dir: &Path| -> Result<DuckReader, DuckError> {
        let glob = format!("{}/tiles_*.parquet", dir.display());
        let mut duck = DuckReader::open_with(&glob, SpatialSource::None)?;
        let catalog = dir.join("concept_catalog.parquet");
        if catalog.is_file() {
            duck = duck.with_catalog(&catalog.to_string_lossy())?;
        }
        Ok(duck)
    };

    // Tiles: topmost first (rev of base-first `dirs`).
    let mut stack = Vec::new();
    for dir in dirs.iter().rev() {
        let tiles = open_layer(dir)?.resource_tiles(uuid)?;
        if tiles.is_empty() {
            continue;
        }
        stack.push(as_resource(uuid, graph, tiles));
    }
    if stack.is_empty() {
        return Err(DuckError::Compile(format!("resource {uuid} in no layer")));
    }
    let merged = merge_resources(stack).map_err(DuckError::Compile)?;
    let mut tiles = merged.resource.tiles.unwrap_or_default();
    unify_cardinality_one_tiles(&mut tiles, graph, false, TileMergeMode::PerNodegroup)
        .map_err(DuckError::Compile)?;

    // Labels: base-first fold so the topmost layer's label wins on overwrite.
    let mut labels = HashMap::new();
    for dir in dirs {
        labels.extend(open_layer(dir)?.concept_labels()?);
    }
    ros_madair_read::hydrate_tiles_with_labels(&tiles, uuid, graph, &labels, languages)
        .map_err(|e| DuckError::Compile(format!("hydrate: {e}")))
}

/// Build a tiles-only `StaticResource` for merging (mirrors ros-madair-read's
/// private `as_resource`).
fn as_resource(
    uuid: &str,
    graph: &StaticGraph,
    tiles: Vec<StaticTile>,
) -> alizarin_core::graph::StaticResource {
    use alizarin_core::graph::{StaticResource, StaticResourceMetadata};
    StaticResource {
        resourceinstance: StaticResourceMetadata {
            descriptors: Default::default(),
            graph_id: graph.graph_id().to_string(),
            name: String::new(),
            resourceinstanceid: uuid.to_string(),
            publication_id: None,
            principaluser_id: None,
            legacyid: None,
            graph_publication_id: None,
            createdtime: None,
            lastmodified: None,
        },
        tiles: Some(tiles),
        metadata: Default::default(),
        cache: None,
        scopes: None,
        tiles_loaded: Some(true),
    }
}

/// One-shot convenience: open, resolve, done.
pub fn resolve_ids(
    parquet_glob: &str,
    query: &Query,
    graph: &StaticGraph,
    registry: &ExtensionTypeRegistry,
) -> Result<Vec<String>, DuckError> {
    DuckReader::open(parquet_glob)?.resolve_ids(query, graph, registry)
}

// ---------------------------------------------------------------------------
// Expr → a SELECT of DISTINCT resource_id
// ---------------------------------------------------------------------------

fn compile_expr(
    expr: &Expr,
    graph: &StaticGraph,
    registry: &ExtensionTypeRegistry,
    has_catalog: bool,
) -> Result<String, DuckError> {
    match expr {
        Expr::All(children) => {
            if children.is_empty() {
                return Ok("SELECT DISTINCT resource_id FROM tiles".to_string());
            }
            let parts: Result<Vec<_>, _> = children
                .iter()
                .map(|c| compile_expr(c, graph, registry, has_catalog))
                .collect();
            Ok(parts?.join("\nINTERSECT\n"))
        }
        Expr::Any(children) => {
            if children.is_empty() {
                return Ok("SELECT resource_id FROM tiles WHERE false".to_string());
            }
            let parts: Result<Vec<_>, _> = children
                .iter()
                .map(|c| compile_expr(c, graph, registry, has_catalog))
                .collect();
            Ok(parts?.join("\nUNION\n"))
        }
        Expr::Not(inner) => {
            let inner_sql = compile_expr(inner, graph, registry, has_catalog)?;
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles EXCEPT {inner_sql}"
            ))
        }
        Expr::Concept { path, op, value } => {
            let (node, ng) = resolve(graph, path)?;
            expect_class(&node, registry, |c| matches!(c, IndexClass::ConceptHierarchical { .. }), path, "concept")?;
            match op {
                ConceptOp::Is => Ok(format!(
                    "SELECT DISTINCT resource_id FROM tiles \
                     WHERE nodegroup_id = '{}' AND concept_id = '{}'",
                    sql_lit(&ng),
                    sql_lit(value)
                )),
                // Descendant-or-self: the tile's concept must fall in the queried
                // concept's DFS interval. One range read over the DFS-ordered
                // catalog (works for `concept` AND `reference` — shared class).
                ConceptOp::DescendantOrSelfOf => {
                    if !has_catalog {
                        return Err(DuckError::Compile(format!(
                            "descendant-or-self on '{path}' needs the concept \
                             catalog — open with `open_with_catalog`"
                        )));
                    }
                    Ok(format!(
                        "SELECT DISTINCT t.resource_id FROM tiles t \
                         JOIN concepts c ON c.concept_id = t.concept_id \
                         WHERE t.nodegroup_id = '{}' \
                           AND c.dfs_enter BETWEEN \
                             (SELECT dfs_enter FROM concepts WHERE concept_id = '{}') AND \
                             (SELECT dfs_leave FROM concepts WHERE concept_id = '{}')",
                        sql_lit(&ng),
                        sql_lit(value),
                        sql_lit(value)
                    ))
                }
            }
        }
        Expr::Range { path, lo, hi } => {
            let (node, ng) = resolve(graph, path)?;
            expect_class(&node, registry, |c| matches!(c, IndexClass::Ordered), path, "range")?;
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles \
                 WHERE nodegroup_id = '{}' AND q_ordered BETWEEN {lo} AND {hi}",
                sql_lit(&ng)
            ))
        }
        Expr::Bbox { path, min_lng, min_lat, max_lng, max_lat } => {
            let (node, ng) = resolve(graph, path)?;
            expect_class(&node, registry, |c| matches!(c, IndexClass::SpatialBbox), path, "bbox")?;
            // Coarse bbox-overlap prune on the promoted zone-map columns …
            let coarse = format!(
                "NOT (geo_min_lng > {max_lng} OR geo_max_lng < {min_lng} \
                  OR geo_min_lat > {max_lat} OR geo_max_lat < {min_lat})"
            );
            // … then the EXACT fine step: parse the geometry out of the tile blob
            // and intersect. `data` is `{{ node_id: FeatureCollection }}`.
            let geom = format!(
                "ST_GeomFromGeoJSON(json_extract_string(\
                   json_extract(data, '$.\"{}\".features[0]'), '$.geometry'))",
                sql_lit(&node.nodeid)
            );
            let box_wkt = format!(
                "POLYGON (({min_lng} {min_lat}, {max_lng} {min_lat}, \
                  {max_lng} {max_lat}, {min_lng} {max_lat}, {min_lng} {min_lat}))"
            );
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles \
                 WHERE nodegroup_id = '{}' AND {coarse} \
                 AND ST_Intersects({geom}, ST_GeomFromText('{box_wkt}'))",
                sql_lit(&ng)
            ))
        }
        Expr::HasLink { path, target } => {
            let (node, ng) = resolve(graph, path)?;
            expect_class(&node, registry, |c| matches!(c, IndexClass::Link), path, "link")?;
            // EXACT, unlike the head's coarse chunk_link_summary. `link_targets` is
            // now `{node_id: [targets]}` (per-node), so extract THIS node's array -
            // a multi-link nodegroup (e.g. name_elements) stays precise. A missing
            // node key → json_extract NULL → the predicate is false.
            // `None` target = "has any link on this node".
            let arr = format!("json_extract(link_targets, '$.\"{}\"')", sql_lit(&node.nodeid));
            let cond = match target {
                Some(t) => format!("json_contains({arr}, '\"{}\"')", sql_lit(t)),
                None => format!("json_array_length({arr}) > 0"),
            };
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles \
                 WHERE nodegroup_id = '{}' AND link_targets IS NOT NULL AND {cond}",
                sql_lit(&ng)
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a bare-alias path to its node and nodegroup id.
fn resolve<'g>(graph: &'g StaticGraph, path: &str) -> Result<(&'g StaticNode, String), DuckError> {
    if path.contains('.') {
        return Err(DuckError::Compile(format!(
            "dot-qualified path '{path}' not supported yet (bare alias only)"
        )));
    }
    let node = graph
        .find_node_by_alias(path)
        .ok_or_else(|| DuckError::Compile(format!("unknown path alias '{path}'")))?;
    let ng = node
        .nodegroup_id
        .clone()
        .ok_or_else(|| DuckError::Compile(format!("node '{path}' has no nodegroup")))?;
    Ok((node, ng))
}

fn node_config_value(node: &StaticNode) -> Option<serde_json::Value> {
    if node.config.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(
        node.config.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
    ))
}

/// Assert the node's index class matches the predicate, or a typed error (same
/// discipline the head-SQL compiler enforces).
fn expect_class(
    node: &StaticNode,
    registry: &ExtensionTypeRegistry,
    ok: impl Fn(&IndexClass) -> bool,
    path: &str,
    predicate: &str,
) -> Result<(), DuckError> {
    let class = datatype_index_spec(
        &node.datatype,
        &serde_json::Value::Null,
        node_config_value(node).as_ref(),
        Some(registry),
    )
    .class;
    if ok(&class) {
        Ok(())
    } else {
        Err(DuckError::Compile(format!(
            "'{path}' (datatype '{}') is not indexed for a {predicate} predicate",
            node.datatype
        )))
    }
}

/// Escape a single-quoted SQL string literal (double the quotes). Node/ng ids are
/// UUIDs and paths are trusted, but concept values / file paths get defended.
fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_links_and_runs() {
        let conn = Connection::open_in_memory().unwrap();
        let v: i64 = conn.query_row("SELECT 1 + 41", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 42);
    }

    /// OFFLINE guarantee for JSON: `json_extract` works with autoinstall DISABLED
    /// and no `INSTALL`/`LOAD` — proving the `json` extension is statically
    /// compiled in (the `json` cargo feature), not fetched from the network. This
    /// is the function the `Bbox`/`data`-blob path relies on.
    #[test]
    fn json_is_bundled_no_network() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("SET autoinstall_known_extensions=false; SET autoload_known_extensions=false;")
            .unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT json_extract('{\"a\": 42}', '$.a')::BIGINT",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 42, "json_extract must work with no network install");
    }

    /// OFFLINE seam for spatial: with autoinstall DISABLED and `extension_directory`
    /// pointed at a local dir holding `spatial.duckdb_extension`, `LOAD spatial`
    /// succeeds from disk — no network. Skips if this dev box has no cached
    /// extension (CI / a device ships its own per-platform binary).
    #[test]
    fn spatial_loads_offline_from_local_dir() {
        let Some(home) = std::env::var_os("HOME") else {
            eprintln!("no HOME — skipping");
            return;
        };
        let ext_dir = Path::new(&home).join(".duckdb").join("extensions");
        // The layout is <dir>/<version>/<platform>/spatial.duckdb_extension.
        let has_any = std::fs::read_dir(&ext_dir).ok().is_some_and(|rd| {
            rd.filter_map(|e| e.ok()).any(|v| {
                v.path()
                    .join("linux_amd64")
                    .join("spatial.duckdb_extension")
                    .exists()
            })
        });
        if !has_any {
            eprintln!("no cached spatial extension — skipping offline spatial test");
            return;
        }
        let conn = Connection::open_in_memory().unwrap();
        configure_spatial(&conn, &SpatialSource::OfflineDir(&ext_dir));
        // If spatial loaded from disk, a spatial function resolves.
        let area: f64 = conn
            .query_row(
                "SELECT ST_Area(ST_GeomFromText('POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))'))",
                [],
                |r| r.get(0),
            )
            .expect("spatial loaded offline from the local extension_directory");
        assert_eq!(area, 4.0);
    }
}
