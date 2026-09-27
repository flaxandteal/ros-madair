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
//!   - `HasLink`/`cited_by`/`geo_points` are EXACT, over the columnar edge table
//!     (`edges_<slug>.parquet`, one row per link target) — per node via `src_node`
//!     (globally unique), so membership is precise even when a nodegroup holds >1
//!     link node (the head was coarse here). The old per-tile `link_targets` JSON
//!     column is dropped: hydration reads `data`, and links live only in `edges`.
//!   - `path` resolves a bare alias OR a dot-qualified path walked from the root
//!     (e.g. `address.location`), so a hop can reach a link node nested under
//!     within-resource nodegroups.
//!   - `concept_ids` promotes EVERY concept id in a tile as a JSON array (a
//!     concept-list node, or two CONCEPT nodes in one nodegroup, both contribute),
//!     so a `Concept` filter is membership (`json_contains`), not equality. Node-
//!     level distinction within a nodegroup is still coarse (the filter keys on the
//!     nodegroup, matching any of its concepts) — the same node-vs-nodegroup
//!     granularity the head had.

use std::collections::HashMap;
use std::path::Path;

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode};
use alizarin_core::StaticTile;
use duckdb::Connection;
use ros_madair_query::{ConceptOp, Expr, Query};

mod hydrate;
pub use hydrate::hydrate_tiles_with_labels;

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

/// Verify a snapshot directory's signed content: recompute its `snapshot_id` from
/// the files on disk and check it against the attestation. Returns the
/// three-state [`ros_madair_format::attest::HeadTrust`] — Verified / Unsigned /
/// Failed — for a caller that wants to inspect trust explicitly. [`DuckReader::open_layers`]
/// calls this and refuses a `Failed` (tampered) layer; `Unsigned` and unsigned
/// local builds proceed. The verify implementation lives in `ros-madair-format`
/// (shared with the emitter's signing), so a signed id and a recomputed id
/// cannot drift.
pub fn verify_snapshot(dir: &Path) -> Result<ros_madair_format::attest::HeadTrust, DuckError> {
    ros_madair_format::verify::verify_snapshot(dir).map_err(|e| DuckError::Compile(e.to_string()))
}

/// A DuckDB connection with a `tiles` view over one model's Parquet, and
/// optionally a `concepts` view over the DFS-ordered concept catalog.
pub struct DuckReader {
    conn: Connection,
    has_catalog: bool,
    /// Whether an `edges` view is registered (sibling `edges_*.parquet` next to
    /// the tiles glob) — drives `Expr::OnLink` path predicates. Absent for a
    /// tiles-only snapshot; an OnLink then errors rather than mis-compiling.
    has_edges: bool,
    /// Whether a `concept_index` view is registered (sibling `concepts_*.parquet`) —
    /// drives `Expr::Concept` (node-scoped membership semijoin + DFS-catalog range join).
    /// Absent for a legacy tiles-only / pre-melt snapshot; a Concept then errors rather
    /// than mis-compiling against a dropped `concept_ids` column.
    has_concept_index: bool,
    /// Whether an `ordered_index` view is registered (sibling `ordered_*.parquet`) —
    /// drives `Expr::Range`. Absent for a pre-melt snapshot; a Range then errors.
    has_ordered: bool,
    /// Whether the `spatial` extension actually loaded (ST_Intersects available).
    /// Drives the `Expr::Bbox` fine step: present -> exact ST_Intersects; absent
    /// (mobile - no android spatial binary) -> coarse-only bbox-overlap. See the
    /// README "Platform limitation: the exact spatial fine step is DESKTOP-ONLY".
    spatial: bool,
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
    /// Skip spatial — no ST_Intersects. A *bare* `Bbox` then compiles to the
    /// coarse bbox-overlap SUPERSET (recall-tolerant, the mobile path); a
    /// *negated* `Bbox` is refused at compile time, since you cannot soundly
    /// complement a superset (see `first_coarse_path` / the `Not` arm).
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
        // configure_spatial is best-effort (Auto needs network; OfflineDir needs a
        // platform binary; None never loads), so DETECT whether it took rather than
        // trust the source: probe for the ST_Intersects function. This is the switch
        // between the exact and coarse-only Bbox paths.
        let spatial: bool = conn
            .query_row(
                "SELECT count(*) > 0 FROM duckdb_functions() \
                 WHERE lower(function_name) = 'st_intersects'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        conn.execute_batch(&format!(
            "CREATE VIEW tiles AS SELECT * FROM read_parquet('{}');",
            sql_lit(parquet_glob)
        ))?;
        // Edge table (Expr::OnLink): the sibling edges_*.parquet next to the tiles
        // glob. Best-effort — a tiles-only snapshot (a pre-edge-slice artifact, a
        // hand-built fixture, a partial/governed export) still OPENS; only an
        // OnLink query then errors, with a typed message. This keeps "openable"
        // independent of "has edges" and fails late (on use), not early (at open).
        let edge_glob = parquet_glob.replace("tiles_", "edges_");
        let has_edges = edge_glob != parquet_glob
            && conn
                .execute_batch(&format!(
                    "CREATE VIEW edges AS SELECT * FROM read_parquet('{}');",
                    sql_lit(&edge_glob)
                ))
                .is_ok();
        // Melted concept / ordered axis stores (the siblings concepts_*.parquet /
        // ordered_*.parquet next to the tiles glob), same best-effort registration as
        // edges: a pre-melt snapshot still OPENS; only a Concept/Range query then errors
        // typed. concept_index is the node/value-sorted membership store (distinct from
        // the DFS-interval `concepts` catalog view registered by with_catalog).
        let concept_glob = parquet_glob.replace("tiles_", "concepts_");
        let has_concept_index = concept_glob != parquet_glob
            && conn
                .execute_batch(&format!(
                    "CREATE VIEW concept_index AS SELECT * FROM read_parquet('{}');",
                    sql_lit(&concept_glob)
                ))
                .is_ok();
        let ordered_glob = parquet_glob.replace("tiles_", "ordered_");
        let has_ordered = ordered_glob != parquet_glob
            && conn
                .execute_batch(&format!(
                    "CREATE VIEW ordered_index AS SELECT * FROM read_parquet('{}');",
                    sql_lit(&ordered_glob)
                ))
                .is_ok();
        Ok(Self {
            conn,
            has_catalog: false,
            has_edges,
            has_concept_index,
            has_ordered,
            spatial,
        })
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

    /// Open a LAYERED reader over `dirs` (base first, topmost last): the `tiles`,
    /// `edges`, and `concepts` views are the layers COMPOSED with overlay
    /// precedence, so `resolve_ids`/`count_records` — and, crucially, an OnLink
    /// hop — see the merged state, and a hop can cross layers (an edge in one
    /// layer semijoins a target tile in another, because the composed views union
    /// them). The compiler is unchanged; only the views it targets are composed.
    ///
    /// Precedence is keyed per `(resource_id, nodegroup_id)` for tiles /
    /// `(src_resource, src_nodegroup)` for edges — the topmost layer that carries
    /// that tile wins, and everything it does not touch falls through to the base.
    /// (This is the cardinality-1 rule; cardinality-n tile merge — as
    /// `hydrate_layers` does — is a refinement.)
    pub fn open_layers(dirs: &[&Path]) -> Result<Self, DuckError> {
        if dirs.is_empty() {
            return Err(DuckError::Compile("open_layers: no layers".into()));
        }
        // Refuse a layer whose SIGNED content has been altered since it was
        // signed — the read-side gate that closes the sign→verify loop. A layer
        // with no manifest/attestations (a local or unsigned build) verifies as
        // Unsigned, or is simply not a signed snapshot; both proceed. Only a
        // proven tamper (`Failed`) is fatal.
        for dir in dirs {
            if let Ok(ros_madair_format::attest::HeadTrust::Failed { reason }) =
                ros_madair_format::verify::verify_snapshot(dir)
            {
                return Err(DuckError::Compile(format!(
                    "layer {}: snapshot verification failed — {reason}",
                    dir.display()
                )));
            }
        }
        let conn = Connection::open_in_memory()?;
        // Match the hydration layer path: no network. A Bbox then compiles coarse.
        configure_spatial(&conn, &SpatialSource::None);
        let spatial: bool = conn
            .query_row(
                "SELECT count(*) > 0 FROM duckdb_functions() \
                 WHERE lower(function_name) = 'st_intersects'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);

        // base-first: the layer index IS the precedence rank (higher wins).
        let ranked_union = |layers: &[(usize, String)]| -> String {
            layers
                .iter()
                .map(|(i, g)| {
                    format!(
                        "SELECT *, {i} AS _layer FROM read_parquet('{}')",
                        sql_lit(g)
                    )
                })
                .collect::<Vec<_>>()
                .join("\nUNION ALL BY NAME\n")
        };

        // Tiles (required): topmost tile per (resource_id, nodegroup_id).
        let tile_layers = layers_with(&conn, dirs, "tiles_*.parquet");
        if tile_layers.is_empty() {
            return Err(DuckError::Compile(
                "open_layers: no tiles_*.parquet in any layer".into(),
            ));
        }
        conn.execute_batch(&format!(
            "CREATE VIEW tiles AS SELECT * EXCLUDE (_layer) FROM ({}) \
             QUALIFY row_number() OVER \
               (PARTITION BY resource_id, nodegroup_id ORDER BY _layer DESC) = 1;",
            ranked_union(&tile_layers)
        ))?;

        // Edges (optional): keep ALL edges from the winning layer per
        // (src_resource, src_nodegroup) — an overlay that re-links a tile replaces
        // that tile's whole edge set, not one row.
        let edge_layers = layers_with(&conn, dirs, "edges_*.parquet");
        let has_edges = !edge_layers.is_empty();
        if has_edges {
            conn.execute_batch(&format!(
                "CREATE VIEW edges AS \
                 SELECT src_resource, src_node, src_nodegroup, src_tile, target_resource FROM (\
                   SELECT *, max(_layer) OVER \
                     (PARTITION BY src_resource, src_nodegroup) AS _win FROM ({})\
                 ) WHERE _layer = _win;",
                ranked_union(&edge_layers)
            ))?;
        }

        // Concept catalog (optional): union the DFS-interval catalogs, topmost wins per
        // concept_id. (The `concepts` view is the interval catalog — distinct from the
        // melted `concept_index` store composed below.)
        let cat_layers = layers_with(&conn, dirs, "concept_catalog.parquet");
        let has_catalog = !cat_layers.is_empty();
        if has_catalog {
            conn.execute_batch(&format!(
                "CREATE VIEW concepts AS \
                 SELECT concept_id, dfs_enter, dfs_leave, label FROM ({}) \
                 QUALIFY row_number() OVER (PARTITION BY concept_id ORDER BY _layer DESC) = 1;",
                ranked_union(&cat_layers)
            ))?;
        }

        // Melted concept / ordered stores (optional): same winning-layer rule as edges —
        // the topmost layer that carries a (resource, nodegroup) replaces its whole melt
        // set, so a re-emitted tile in an overlay supersedes the base's node rows.
        let concept_layers = layers_with(&conn, dirs, "concepts_*.parquet");
        let has_concept_index = !concept_layers.is_empty();
        if has_concept_index {
            conn.execute_batch(&format!(
                "CREATE VIEW concept_index AS \
                 SELECT resource_id, tile_id, nodegroup_id, node_id, concept_id FROM (\
                   SELECT *, max(_layer) OVER \
                     (PARTITION BY resource_id, nodegroup_id) AS _win FROM ({})\
                 ) WHERE _layer = _win;",
                ranked_union(&concept_layers)
            ))?;
        }
        let ordered_layers = layers_with(&conn, dirs, "ordered_*.parquet");
        let has_ordered = !ordered_layers.is_empty();
        if has_ordered {
            conn.execute_batch(&format!(
                "CREATE VIEW ordered_index AS \
                 SELECT resource_id, tile_id, nodegroup_id, node_id, q_ordered FROM (\
                   SELECT *, max(_layer) OVER \
                     (PARTITION BY resource_id, nodegroup_id) AS _win FROM ({})\
                 ) WHERE _layer = _win;",
                ranked_union(&ordered_layers)
            ))?;
        }

        Ok(Self {
            conn,
            has_catalog,
            has_edges,
            has_concept_index,
            has_ordered,
            spatial,
        })
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
        let placeholders = std::iter::repeat_n("?", uris.len())
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

    /// All `(concept_id, label)` from the catalog - the bulk concept→label dump
    /// (Gréasán `v2_closure`, for on-device reference rendering). Empty without a
    /// catalog attached.
    pub fn concept_labels(&self) -> Result<HashMap<String, String>, DuckError> {
        let mut out = HashMap::new();
        if !self.has_catalog {
            return Ok(out);
        }
        let mut stmt = self.conn.prepare(
            "SELECT concept_id, label FROM concepts WHERE label IS NOT NULL AND label <> ''",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, label) = row?;
            out.insert(id, label);
        }
        Ok(out)
    }

    /// Reverse-link geo lookup (Gréasán `v2_geo_points`, MapView): every resource
    /// that links to `target` through node `node_id`, with its descriptor + point
    /// (`geo_min_lat`/`geo_min_lng`). Node-precise via the edge table (`src_node`).
    /// The link and the geometry live on different tiles of the same
    /// resource, so self-join on `resource_id`.
    pub fn geo_points(
        &self,
        node_id: &str,
        target: &str,
    ) -> Result<Vec<(String, String, f64, f64)>, DuckError> {
        if !self.has_edges {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT e.src_resource, any_value(g.descriptor_name), \
                    any_value(g.geo_min_lat), any_value(g.geo_min_lng) \
             FROM edges e \
             JOIN tiles g ON g.resource_id = e.src_resource AND g.geo_min_lat IS NOT NULL \
             WHERE e.src_node = '{}' AND e.target_resource = '{}' \
             GROUP BY e.src_resource",
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

    /// Reverse-link lookup returning just the citer ids: every resource that links
    /// to `target` through node `node_id` (a reverse scan of the edge table).
    /// Reverse traversal over the edge table — the retired SQLite `Layers::cited_by`
    /// counterpart, now over Parquet (used for
    /// Logainm placenames via the place graph's `element_entry` node, cognates via
    /// `cognate_entry_id`, external examples via `headword_entry`).
    pub fn cited_by(&self, node_id: &str, target: &str) -> Result<Vec<String>, DuckError> {
        if !self.has_edges {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT DISTINCT src_resource FROM edges \
             WHERE src_node = '{}' AND target_resource = '{}'",
            sql_lit(node_id),
            sql_lit(target)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
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
    /// to the crate's storage-agnostic [`hydrate_tiles_with_labels`] (the tile→tree
    /// half of hydration). The catalog must be attached for labels.
    pub fn hydrate(
        &self,
        uuid: &str,
        graph: &StaticGraph,
        languages: &[&str],
    ) -> Result<serde_json::Value, DuckError> {
        let tiles = self.resource_tiles(uuid)?;
        let labels = self.concept_labels()?;
        hydrate_tiles_with_labels(&tiles, uuid, graph, &labels, languages)
            .map_err(|e| DuckError::Compile(format!("hydrate: {e}")))
    }

    /// Resolve a query to the sorted set of matching resource ids.
    pub fn resolve_ids(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: &ExtensionTypeRegistry,
    ) -> Result<Vec<String>, DuckError> {
        self.resolve_ids_linked(query, graph, &[], registry)
    }

    /// Resolve with ADDITIONAL linked models available for cross-model OnLink hops.
    /// `graph` is the query's model; `linked` are the other models an OnLink `where`
    /// may target. Their tiles/edges must be in this reader's views — open over a
    /// glob (or [`open_layers`](Self::open_layers)) spanning every model the query
    /// traverses (nodegroup/node ids are globally unique, so one `tiles`/`edges`
    /// view holding several models is unambiguous).
    pub fn resolve_ids_linked(
        &self,
        query: &Query,
        graph: &StaticGraph,
        linked: &[&StaticGraph],
        registry: &ExtensionTypeRegistry,
    ) -> Result<Vec<String>, DuckError> {
        let mut graphs: Vec<&StaticGraph> = Vec::with_capacity(1 + linked.len());
        graphs.push(graph);
        graphs.extend_from_slice(linked);
        let sql = format!(
            "SELECT resource_id FROM ({}) t ORDER BY resource_id",
            self.matching_ids_select(query, graph, &graphs, registry)?
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
        self.count_records_linked(query, graph, &[], registry)
    }

    /// [`count_records`](Self::count_records) with additional linked models for
    /// cross-model OnLink hops (see [`resolve_ids_linked`](Self::resolve_ids_linked)).
    pub fn count_records_linked(
        &self,
        query: &Query,
        graph: &StaticGraph,
        linked: &[&StaticGraph],
        registry: &ExtensionTypeRegistry,
    ) -> Result<usize, DuckError> {
        let mut graphs: Vec<&StaticGraph> = Vec::with_capacity(1 + linked.len());
        graphs.push(graph);
        graphs.extend_from_slice(linked);
        let sql = format!(
            "SELECT COUNT(*) FROM ({}) t",
            self.matching_ids_select(query, graph, &graphs, registry)?
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
        graphs: &[&StaticGraph],
        registry: &ExtensionTypeRegistry,
    ) -> Result<String, DuckError> {
        match &query.r#where {
            Some(expr) => compile_expr(
                expr,
                graph,
                graphs,
                registry,
                self.has_catalog,
                self.has_edges,
                self.has_concept_index,
                self.has_ordered,
                self.spatial,
            ),
            None => Ok("SELECT DISTINCT resource_id FROM tiles".to_string()),
        }
    }
}

/// Open a layer dir as a `DuckReader` (tiles glob + optional concept catalog).
/// For a per-layer filename pattern, the `(rank, glob)` of the layers that
/// actually contain matching files — so the composed view never `read_parquet`s
/// a zero-match glob (which errors at bind). Rank = the layer's index in `dirs`.
fn layers_with(conn: &Connection, dirs: &[&Path], pattern: &str) -> Vec<(usize, String)> {
    dirs.iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let glob = format!("{}/{}", d.display(), pattern);
            let n: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM glob('{}')", sql_lit(&glob)),
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            (n > 0).then_some((i, glob))
        })
        .collect()
}

fn open_layer(dir: &Path) -> Result<DuckReader, DuckError> {
    let glob = format!("{}/tiles_*.parquet", dir.display());
    let mut duck = DuckReader::open_with(&glob, SpatialSource::None)?;
    let catalog = dir.join("concept_catalog.parquet");
    if catalog.is_file() {
        duck = duck.with_catalog(&catalog.to_string_lossy())?;
    }
    Ok(duck)
}

/// Run `f` against a layer's `DuckReader`, opened ONCE and pooled for reuse across
/// hydrates. Opening a DuckDB per layer was the dominant per-entry cost (~75ms
/// each, and every layer is opened on every entry open); a resource's tiles are a
/// query, not a reason to re-open. `resource_tiles`/`concept_labels` take `&self`,
/// so one pooled reader serves every query. The pool lock is held during the query
/// — fine for a dictionary app opening entries sequentially; concurrent hydrations
/// (e.g. parallel cognate loads) serialize briefly on the (fast, post-open) query.
fn with_layer<T>(
    dir: &Path,
    f: impl FnOnce(&DuckReader) -> Result<T, DuckError>,
) -> Result<T, DuckError> {
    use std::sync::{Mutex, OnceLock};
    static POOL: OnceLock<Mutex<HashMap<std::path::PathBuf, DuckReader>>> = OnceLock::new();
    let mut pool = POOL
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("layer reader pool poisoned");
    if !pool.contains_key(dir) {
        let reader = open_layer(dir)?;
        pool.insert(dir.to_path_buf(), reader);
    }
    f(pool.get(dir).expect("just inserted"))
}

/// The merged concept-label map for a layer set, cached. Concept labels are stable
/// for a given set of installed layers (they do not change per resource), and
/// reading every layer's full `concept_catalog.parquet` on every hydrate was ~half
/// the per-entry cost. Build once per dir set (base-first fold, topmost wins), then
/// reuse. Keyed by the dir set; a layer install/uninstall changes the set and
/// rebuilds.
fn cached_concept_labels(
    dirs: &[&Path],
) -> Result<std::sync::Arc<HashMap<String, String>>, DuckError> {
    use std::sync::{Arc, Mutex, OnceLock};
    // Dir-set key → the merged, shared concept-label map for that layer set.
    type ConceptLabelCache = HashMap<String, Arc<HashMap<String, String>>>;
    static CACHE: OnceLock<Mutex<ConceptLabelCache>> = OnceLock::new();
    let key = dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    {
        let cache = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("concept-label cache poisoned");
        if let Some(l) = cache.get(&key) {
            return Ok(l.clone());
        }
    }
    let mut labels = HashMap::new();
    for dir in dirs {
        labels.extend(with_layer(dir, |r| r.concept_labels())?);
    }
    let arc = Arc::new(labels);
    CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("concept-label cache poisoned")
        .insert(key, arc.clone());
    Ok(arc)
}

/// Pre-open + pool every layer's `DuckReader` and build the concept-label cache,
/// so the FIRST hydrate does not pay the ~75ms/layer DuckDB open cost. Call once
/// after the layer set is known (ideally off the UI thread). Idempotent: a warmed
/// layer is skipped by the pool. Errors on individual layers are swallowed - a bad
/// layer just isn't pre-warmed and pays its open on first use.
pub fn prewarm(dirs: &[&Path]) {
    for dir in dirs {
        let _ = with_layer(dir, |_| Ok(()));
    }
    let _ = cached_concept_labels(dirs);
}

/// `cited_by` across a layer set: every resource (in any layer) that links to
/// `target` through node `node_id`, unioned + deduped. Pooled reader reuse. The
/// parquet counterpart of the app's sqlite `Layers::cited_by` path.
pub fn cited_by(dirs: &[&Path], node_id: &str, target: &str) -> Result<Vec<String>, DuckError> {
    let mut ids = Vec::new();
    for dir in dirs {
        ids.append(&mut with_layer(dir, |r| r.cited_by(node_id, target))?);
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// `descriptors` across a layer set (pooled reader reuse), first-wins per uri to
/// match the app's `v2_descriptors` composition (base-first order, earliest layer
/// keeps the name). The pooled counterpart of iterating `DuckReader::open_with`
/// per dir + calling the `descriptors` method - so the enrichment path (cognate /
/// example / placename display names) reuses the warm pool instead of cold-opening
/// every layer on every entry.
pub fn descriptors(dirs: &[&Path], uris: &[String]) -> Result<HashMap<String, String>, DuckError> {
    let mut out: HashMap<String, String> = HashMap::new();
    for dir in dirs {
        for (uri, name) in with_layer(dir, |r| r.descriptors(uris))? {
            out.entry(uri).or_insert(name);
        }
    }
    Ok(out)
}

/// The merged concept-label map for a layer set (pooled + cached) - the public
/// entry point for the app's `v2_closure`. It hits the SAME reader pool and label
/// cache the hydrate path warms, so after `prewarm` it is a cache hit rather than a
/// fresh per-layer `concept_catalog.parquet` read (which was ~2s cold across the
/// full stack). Base-first fold, topmost label wins - as the sqlite insert did.
pub fn concept_labels(dirs: &[&Path]) -> Result<HashMap<String, String>, DuckError> {
    Ok((*cached_concept_labels(dirs)?).clone())
}

/// Multi-layer composed hydration from Parquet - the retired SQLite
/// `Layers::hydrate_resource` counterpart. Gathers a resource's tiles from
/// every layer that has it (TOPMOST first, so `merge_resources`' first-wins ==
/// topmost-wins), merges with per-nodegroup precedence, folds concept labels
/// base-first (topmost label wins on overwrite), and hydrates. `dirs` is base-first
/// (the app's composition order). Same composition as the sqlite `Layers` path;
/// only the tile source (the Parquet `data` column) differs.
pub fn hydrate_layers(
    dirs: &[&Path],
    uuid: &str,
    graph: &alizarin_core::LayeredGraph,
    languages: &[&str],
    layer_ids: Option<&[String]>,
    registry: &alizarin_core::FunctionsRegistry,
) -> Result<serde_json::Value, DuckError> {
    use alizarin_core::graph::{merge_resources, unify_cardinality_one_tiles, TileMergeMode};

    // Perf instrumentation: RM_HYDRATE_PERF=1 logs per-phase timings to stderr
    // (RustStdoutStderr in logcat). Zero cost when unset.
    let perf = std::env::var_os("RM_HYDRATE_PERF").is_some();
    let t0 = std::time::Instant::now();
    macro_rules! mark {
        ($label:expr) => {
            if perf {
                eprintln!(
                    "[perf] hydrate {:<8} {:>5}ms",
                    $label,
                    t0.elapsed().as_millis()
                );
            }
        };
    }

    // Tiles: topmost first (rev of base-first `dirs`). Capture, in the SAME pass,
    // which layers actually carried tiles for this resource - that IS the
    // membership set the compute-tiles hook needs, so we never re-open + re-query
    // the layers just to test membership.
    let mut stack = Vec::new();
    let mut present_ids: Vec<&str> = Vec::new();
    for (i, dir) in dirs.iter().enumerate().rev() {
        let tiles = with_layer(dir, |r| r.resource_tiles(uuid))?;
        if tiles.is_empty() {
            continue;
        }
        if let Some(ids) = layer_ids {
            if let Some(id) = ids.get(i) {
                present_ids.push(id.as_str());
            }
        }
        stack.push(as_resource(uuid, graph, tiles));
    }
    if stack.is_empty() {
        return Err(DuckError::Compile(format!("resource {uuid} in no layer")));
    }
    mark!("gather");
    let merged = merge_resources(stack).map_err(DuckError::Compile)?;
    let mut tiles = merged.resource.tiles.unwrap_or_default();
    unify_cardinality_one_tiles(&mut tiles, graph, false, TileMergeMode::PerNodegroup)
        .map_err(DuckError::Compile)?;
    mark!("unify");
    if perf {
        let mut ng: HashMap<&str, usize> = HashMap::new();
        for t in &tiles {
            *ng.entry(t.nodegroup_id.as_str()).or_default() += 1;
        }
        eprintln!("[perf] pre-derive tiles={} by_ng={ng:?}", tiles.len());
    }

    // Compute-tiles hook: run any compute-tiles functions declared on the graph.
    // Membership (`present_ids`) came free from the gather above.
    if layer_ids.is_some() {
        let is_member = |layer_id: &str| -> bool { present_ids.contains(&layer_id) };
        // Resolve each graph-declared Derive function's provider from `registry`
        // by UUID and merge its JIT tiles in (attested wins; see
        // alizarin_core::apply_derive_functions). An empty registry is a no-op.
        //
        // fxgs are NOT base-only: a computed layer carries its compute-tiles
        // declaration on its OWN graph. `graph` is the caller's composed
        // LayeredGraph (one layer or many - internal to it); its
        // `functions_x_graphs()` unions across layers, so an overlay's fxg (and
        // any nodes it adds) are visible while the base provides the shared
        // nodegroups. `&LayeredGraph` coerces to the `&dyn GraphLookup` this takes.
        alizarin_core::apply_derive_functions(&mut tiles, graph, uuid, &is_member, registry);
    }
    mark!("derive");
    if perf {
        let mut ng: HashMap<&str, usize> = HashMap::new();
        for t in &tiles {
            *ng.entry(t.nodegroup_id.as_str()).or_default() += 1;
        }
        eprintln!("[perf] post-derive tiles={} by_ng={ng:?}", tiles.len());
    }

    // Labels: cached per layer set (base-first fold, topmost wins) - see
    // cached_concept_labels. Was ~half the per-entry cost (a full catalog read of
    // every layer, every hydrate); now built once per installed-layer set.
    let labels = cached_concept_labels(dirs)?;
    mark!("labels");
    let out = hydrate_tiles_with_labels(&tiles, uuid, graph, &labels, languages)
        .map_err(|e| DuckError::Compile(format!("hydrate: {e}")));
    mark!("tree");
    out
}

/// Build a tiles-only `StaticResource` for merging.
fn as_resource(
    uuid: &str,
    graph: &dyn alizarin_core::GraphLookup,
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

#[allow(clippy::too_many_arguments)]
fn compile_expr(
    expr: &Expr,
    graph: &StaticGraph,
    // The model set, for resolving an OnLink hop's target model. `graph` is the
    // CURRENT model (this level's aliases); `graphs` resolves the link target.
    graphs: &[&StaticGraph],
    registry: &ExtensionTypeRegistry,
    has_catalog: bool,
    has_edges: bool,
    has_concept_index: bool,
    has_ordered: bool,
    spatial: bool,
) -> Result<String, DuckError> {
    match expr {
        Expr::All(children) => {
            if children.is_empty() {
                return Ok("SELECT DISTINCT resource_id FROM tiles".to_string());
            }
            let parts: Result<Vec<_>, _> = children
                .iter()
                .map(|c| {
                    compile_expr(
                        c,
                        graph,
                        graphs,
                        registry,
                        has_catalog,
                        has_edges,
                        has_concept_index,
                        has_ordered,
                        spatial,
                    )
                })
                .collect();
            Ok(parts?.join("\nINTERSECT\n"))
        }
        Expr::Any(children) => {
            if children.is_empty() {
                return Ok("SELECT resource_id FROM tiles WHERE false".to_string());
            }
            let parts: Result<Vec<_>, _> = children
                .iter()
                .map(|c| {
                    compile_expr(
                        c,
                        graph,
                        graphs,
                        registry,
                        has_catalog,
                        has_edges,
                        has_concept_index,
                        has_ordered,
                        spatial,
                    )
                })
                .collect();
            Ok(parts?.join("\nUNION\n"))
        }
        Expr::Not(inner) => {
            // Soundness: `EXCEPT inner` is set-complement over resource_id, so if
            // `inner` compiles to a SUPERSET (a coarse predicate) the complement
            // UNDER-approximates and silently drops real matches. The only coarse
            // leaf in this backend is a `Bbox` WITHOUT the spatial extension (the
            // bbox-overlap superset, no exact ST_Intersects fine step). Refuse it
            // rather than mis-answer — the exact analogue of the head compiler's
            // `NegatedCoarsePredicate`. With spatial loaded a Bbox is exact and
            // this passes, so a Bbox under `Not` REQUIRES spatial.
            if let Some(path) = first_coarse_path(inner, spatial) {
                return Err(DuckError::Compile(format!(
                    "cannot negate the bbox predicate on '{path}' without the spatial \
                     extension: bbox-overlap over-approximates, so EXCEPT would drop \
                     matching resources; open with the spatial extension (exact \
                     ST_Intersects) to negate a spatial predicate"
                )));
            }
            let inner_sql = compile_expr(
                inner,
                graph,
                graphs,
                registry,
                has_catalog,
                has_edges,
                has_concept_index,
                has_ordered,
                spatial,
            )?;
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles EXCEPT {inner_sql}"
            ))
        }
        Expr::Concept { path, op, value } => {
            if !has_concept_index {
                return Err(DuckError::Compile(format!(
                    "concept filter on '{path}' needs the melted concept store — this \
                     reader has no concepts_*.parquet (a pre-melt tiles-only snapshot is \
                     unsupported; re-emit with the melt)"
                )));
            }
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::ConceptHierarchical { .. }),
                path,
                "concept",
            )?;
            match op {
                // Node-scoped EXACT membership over the melted concept store, keyed on
                // the node (globally unique) — so two concept nodes in one nodegroup, or
                // a concept-list node's several values, stay distinct. No cross-node
                // conflation, and the (node_id, concept_id) sort zone-map-prunes.
                ConceptOp::Is => Ok(format!(
                    "SELECT DISTINCT resource_id FROM concept_index \
                     WHERE node_id = '{}' AND concept_id = '{}'",
                    sql_lit(&node.nodeid),
                    sql_lit(value)
                )),
                // Descendant-or-self: the node's concept must fall in the queried
                // concept's DFS interval — range-join the DFS-ordered `concepts` catalog
                // over the melted store (works for `concept` AND `reference` — shared
                // class). concept_index = the melt; concepts = the interval catalog.
                ConceptOp::DescendantOrSelfOf => {
                    if !has_catalog {
                        return Err(DuckError::Compile(format!(
                            "descendant-or-self on '{path}' needs the concept \
                             catalog — open with `open_with_catalog`"
                        )));
                    }
                    Ok(format!(
                        "SELECT DISTINCT ci.resource_id FROM concept_index ci \
                         JOIN concepts c ON c.concept_id = ci.concept_id \
                         WHERE ci.node_id = '{}' \
                           AND c.dfs_enter BETWEEN \
                             (SELECT dfs_enter FROM concepts WHERE concept_id = '{}') AND \
                             (SELECT dfs_leave FROM concepts WHERE concept_id = '{}')",
                        sql_lit(&node.nodeid),
                        sql_lit(value),
                        sql_lit(value)
                    ))
                }
            }
        }
        Expr::Range { path, lo, hi } => {
            if !has_ordered {
                return Err(DuckError::Compile(format!(
                    "range filter on '{path}' needs the melted ordered store — this \
                     reader has no ordered_*.parquet (re-emit with the melt)"
                )));
            }
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::Ordered),
                path,
                "range",
            )?;
            // Node-scoped range over the (node_id, q_ordered)-sorted ordered store, so
            // the predicate zone-map-prunes instead of scanning the geo-clustered tile row.
            Ok(format!(
                "SELECT DISTINCT resource_id FROM ordered_index \
                 WHERE node_id = '{}' AND q_ordered BETWEEN {lo} AND {hi}",
                sql_lit(&node.nodeid)
            ))
        }
        Expr::Bbox {
            path,
            min_lng,
            min_lat,
            max_lng,
            max_lat,
        } => {
            let (node, ng) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::SpatialBbox),
                path,
                "bbox",
            )?;
            // Coarse zone-map overlap (a sound SUPERSET) AND, when spatial is loaded,
            // an exact ST_Intersects against EVERY feature of the tile (see
            // `bbox_predicate`). No spatial → coarse-only superset (the mobile path).
            let pred = bbox_predicate(
                "",
                &node.nodeid,
                *min_lng,
                *min_lat,
                *max_lng,
                *max_lat,
                spatial,
            );
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles WHERE nodegroup_id = '{}' AND {pred}",
                sql_lit(&ng)
            ))
        }
        Expr::HasLink { path, target } => {
            if !has_edges {
                return Err(DuckError::Compile(format!(
                    "has-link on '{path}' needs the edge table — this reader has no \
                     edges_*.parquet (open a snapshot emitted with the edge slice)"
                )));
            }
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::Link),
                path,
                "link",
            )?;
            // EXACT membership over the columnar edge table — per node via src_node
            // (globally unique), so a multi-link nodegroup stays precise. `None`
            // target = "has any link on this node".
            let cond = match target {
                Some(t) => format!(" AND target_resource = '{}'", sql_lit(t)),
                None => String::new(),
            };
            Ok(format!(
                "SELECT DISTINCT src_resource AS resource_id FROM edges \
                 WHERE src_node = '{}'{cond}",
                sql_lit(&node.nodeid)
            ))
        }
        // Cross-resource PATH predicate → an edge-table semijoin. Compile the inner
        // predicate against the TARGET model (a set of target resource ids), then
        // keep the source resources whose `path` link lands in that set. Nested
        // OnLinks fold naturally: each wraps the inner set in another semijoin, and
        // because the inner is nested INSIDE the semijoin the selective leaf is
        // evaluated first.
        Expr::OnLink {
            path,
            model,
            r#where,
        } => {
            if !has_edges {
                return Err(DuckError::Compile(format!(
                    "on-link path '{path}' needs the edge table — this reader has no \
                     edges_*.parquet (open a snapshot emitted with the edge slice)"
                )));
            }
            let (link_node, src_ng) = resolve(graph, path)?;
            expect_class(
                link_node,
                registry,
                |c| matches!(c, IndexClass::Link),
                path,
                "link",
            )?;
            // The hop declares its target model (the link node's config does not
            // carry it), so the inner predicate's aliases resolve against the right
            // model. Match by graph id.
            let target = graphs
                .iter()
                .copied()
                .find(|g| g.graphid == *model)
                .ok_or_else(|| {
                    DuckError::Compile(format!(
                        "on-link target model '{model}' not available to the compiler \
                         (open the reader over all linked models)"
                    ))
                })?;
            let inner = compile_expr(
                r#where,
                target,
                graphs,
                registry,
                has_catalog,
                has_edges,
                has_concept_index,
                has_ordered,
                spatial,
            )?;
            Ok(format!(
                "SELECT DISTINCT src_resource AS resource_id FROM edges \
                 WHERE src_node = '{}' AND src_nodegroup = '{}' \
                   AND target_resource IN ({inner})",
                sql_lit(&link_node.nodeid),
                sql_lit(&src_ng)
            ))
        }
        // Same-TILE correlation: every child must hold on ONE tile of the resource.
        // A tile is one nodegroup instance, so all children must resolve to a single
        // nodegroup (else unsatisfiable — a typed error). Promoted leaves read that
        // tile row `t`'s columns directly; a link/hop child becomes `t.tileid IN
        // (edges semijoin on src_tile)`, so the link must originate from THIS tile.
        // The result is the resources with such a tile — distinct from `All`, which
        // only requires each child somewhere in the resource.
        Expr::OnTile(children) => {
            let mut ng = None;
            for c in children {
                tile_nodegroup(c, graph, &mut ng)?;
            }
            let ng = ng.ok_or_else(|| {
                DuckError::Compile(
                    "on_tile has no leaf predicate to anchor a nodegroup (it needs at \
                     least one concept/range/bbox/link condition)"
                        .to_string(),
                )
            })?;
            let cond = compile_tile_cond(
                &Expr::All(children.clone()),
                &ng,
                graph,
                graphs,
                registry,
                has_catalog,
                has_edges,
                has_concept_index,
                has_ordered,
                spatial,
            )?;
            Ok(format!(
                "SELECT DISTINCT t.resource_id FROM tiles t \
                 WHERE t.nodegroup_id = '{}' AND {cond}",
                sql_lit(&ng)
            ))
        }
    }
}

/// Accumulate the single nodegroup every leaf/link node in an `OnTile` subtree
/// belongs to, into `acc`. Errors if two children resolve to different nodegroups
/// — a tile is one nodegroup instance, so correlating across nodegroups is
/// unsatisfiable and a caller mistake. Leaves `acc == None` if the subtree carries
/// no leaf (e.g. an empty `All`). The OnLink/HasLink `path` resolves to the LINK
/// node, whose nodegroup is the source tile's — correct, the link lives in the tile.
fn tile_nodegroup(
    expr: &Expr,
    graph: &StaticGraph,
    acc: &mut Option<String>,
) -> Result<(), DuckError> {
    match expr {
        Expr::All(cs) | Expr::Any(cs) | Expr::OnTile(cs) => {
            for c in cs {
                tile_nodegroup(c, graph, acc)?;
            }
            Ok(())
        }
        Expr::Not(inner) => tile_nodegroup(inner, graph, acc),
        Expr::Concept { path, .. }
        | Expr::Range { path, .. }
        | Expr::Bbox { path, .. }
        | Expr::HasLink { path, .. }
        | Expr::OnLink { path, .. } => {
            let (_, ng) = resolve(graph, path)?;
            match acc {
                None => *acc = Some(ng),
                Some(existing) if *existing == ng => {}
                Some(existing) => {
                    return Err(DuckError::Compile(format!(
                        "on_tile children must share one nodegroup (a tile is one \
                         nodegroup instance), but '{path}' is in '{ng}' while a sibling \
                         is in '{existing}'"
                    )))
                }
            }
            Ok(())
        }
    }
}

/// Compile an `OnTile` child into a boolean SQL fragment over the tile row bound
/// as `t` — every fragment constrains THE SAME tile. Promoted leaves read `t`'s
/// columns; a link/hop child becomes `t.tileid IN (edges semijoin on src_tile)`,
/// so the link must originate from this tile. Booleans compose. `ng` is the single
/// nodegroup every child belongs to (validated by [`tile_nodegroup`] first).
#[allow(clippy::too_many_arguments)]
fn compile_tile_cond(
    expr: &Expr,
    ng: &str,
    graph: &StaticGraph,
    graphs: &[&StaticGraph],
    registry: &ExtensionTypeRegistry,
    has_catalog: bool,
    has_edges: bool,
    has_concept_index: bool,
    has_ordered: bool,
    spatial: bool,
) -> Result<String, DuckError> {
    match expr {
        Expr::All(children) => {
            if children.is_empty() {
                return Ok("true".to_string());
            }
            let parts: Result<Vec<_>, _> = children
                .iter()
                .map(|c| {
                    compile_tile_cond(
                        c,
                        ng,
                        graph,
                        graphs,
                        registry,
                        has_catalog,
                        has_edges,
                        has_concept_index,
                        has_ordered,
                        spatial,
                    )
                })
                .collect();
            Ok(format!("({})", parts?.join(" AND ")))
        }
        Expr::Any(children) => {
            if children.is_empty() {
                return Ok("false".to_string());
            }
            let parts: Result<Vec<_>, _> = children
                .iter()
                .map(|c| {
                    compile_tile_cond(
                        c,
                        ng,
                        graph,
                        graphs,
                        registry,
                        has_catalog,
                        has_edges,
                        has_concept_index,
                        has_ordered,
                        spatial,
                    )
                })
                .collect();
            Ok(format!("({})", parts?.join(" OR ")))
        }
        Expr::Not(inner) => {
            // Same coarse-negation soundness rule as the top-level `Not`: a coarse
            // bbox (no spatial) complemented under-approximates.
            if let Some(path) = first_coarse_path(inner, spatial) {
                return Err(DuckError::Compile(format!(
                    "cannot negate the bbox predicate on '{path}' without the spatial \
                     extension inside on_tile: bbox-overlap over-approximates, so the \
                     negation would drop matching tiles"
                )));
            }
            Ok(format!(
                "NOT ({})",
                compile_tile_cond(
                    inner,
                    ng,
                    graph,
                    graphs,
                    registry,
                    has_catalog,
                    has_edges,
                    has_concept_index,
                    has_ordered,
                    spatial
                )?
            ))
        }
        Expr::Concept { path, op, value } => {
            if !has_concept_index {
                return Err(DuckError::Compile(format!(
                    "concept filter on '{path}' inside on_tile needs the melted concept \
                     store (concepts_*.parquet)"
                )));
            }
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::ConceptHierarchical { .. }),
                path,
                "concept",
            )?;
            match op {
                // Same-tile, node-scoped membership: THIS tile carries the (node, value)
                // in the melted store (tile_id correlates back to t, like edges.src_tile).
                ConceptOp::Is => Ok(format!(
                    "t.tileid IN (SELECT tile_id FROM concept_index \
                     WHERE node_id = '{}' AND concept_id = '{}')",
                    sql_lit(&node.nodeid),
                    sql_lit(value)
                )),
                ConceptOp::DescendantOrSelfOf => {
                    if !has_catalog {
                        return Err(DuckError::Compile(format!(
                            "descendant-or-self on '{path}' needs the concept catalog — \
                             open with `open_with_catalog`"
                        )));
                    }
                    // This tile's node-scoped concept falls in the DFS interval.
                    Ok(format!(
                        "EXISTS (SELECT 1 FROM concept_index ci \
                           JOIN concepts c ON c.concept_id = ci.concept_id \
                         WHERE ci.tile_id = t.tileid AND ci.node_id = '{n}' \
                           AND c.dfs_enter BETWEEN \
                             (SELECT dfs_enter FROM concepts WHERE concept_id = '{v}') AND \
                             (SELECT dfs_leave FROM concepts WHERE concept_id = '{v}'))",
                        n = sql_lit(&node.nodeid),
                        v = sql_lit(value)
                    ))
                }
            }
        }
        Expr::Range { path, lo, hi } => {
            if !has_ordered {
                return Err(DuckError::Compile(format!(
                    "range filter on '{path}' inside on_tile needs the melted ordered store"
                )));
            }
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::Ordered),
                path,
                "range",
            )?;
            Ok(format!(
                "t.tileid IN (SELECT tile_id FROM ordered_index \
                 WHERE node_id = '{}' AND q_ordered BETWEEN {lo} AND {hi})",
                sql_lit(&node.nodeid)
            ))
        }
        Expr::Bbox {
            path,
            min_lng,
            min_lat,
            max_lng,
            max_lat,
        } => {
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::SpatialBbox),
                path,
                "bbox",
            )?;
            Ok(bbox_predicate(
                "t.",
                &node.nodeid,
                *min_lng,
                *min_lat,
                *max_lng,
                *max_lat,
                spatial,
            ))
        }
        Expr::HasLink { path, target } => {
            if !has_edges {
                return Err(DuckError::Compile(format!(
                    "has-link on '{path}' needs the edge table"
                )));
            }
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::Link),
                path,
                "link",
            )?;
            let cond = match target {
                Some(t) => format!(" AND target_resource = '{}'", sql_lit(t)),
                None => String::new(),
            };
            Ok(format!(
                "t.tileid IN (SELECT src_tile FROM edges \
                 WHERE src_node = '{}' AND src_nodegroup = '{}'{cond})",
                sql_lit(&node.nodeid),
                sql_lit(ng)
            ))
        }
        Expr::OnLink {
            path,
            model,
            r#where,
        } => {
            if !has_edges {
                return Err(DuckError::Compile(format!(
                    "on-link path '{path}' needs the edge table"
                )));
            }
            let (link_node, _) = resolve(graph, path)?;
            expect_class(
                link_node,
                registry,
                |c| matches!(c, IndexClass::Link),
                path,
                "link",
            )?;
            let target = graphs
                .iter()
                .copied()
                .find(|g| g.graphid == *model)
                .ok_or_else(|| {
                    DuckError::Compile(format!(
                        "on-link target model '{model}' not available to the compiler"
                    ))
                })?;
            let inner = compile_expr(
                r#where,
                target,
                graphs,
                registry,
                has_catalog,
                has_edges,
                has_concept_index,
                has_ordered,
                spatial,
            )?;
            Ok(format!(
                "t.tileid IN (SELECT src_tile FROM edges \
                 WHERE src_node = '{}' AND src_nodegroup = '{}' \
                   AND target_resource IN ({inner}))",
                sql_lit(&link_node.nodeid),
                sql_lit(ng)
            ))
        }
        Expr::OnTile(_) => Err(DuckError::Compile(
            "nested on_tile is not supported; flatten the conditions into one on_tile".to_string(),
        )),
    }
}

/// The bbox predicate for a geometry node: the coarse zone-map overlap (a sound
/// SUPERSET of true intersection) AND — when the spatial extension is loaded — an
/// exact `ST_Intersects` against EVERY feature of the tile's FeatureCollection.
/// `prefix` qualifies the columns (`""` for a bare `FROM tiles`, `"t."` inside an
/// OnTile predicate).
///
/// Iterating all features is a soundness requirement, not a nicety: emit's
/// `extract_bbox` unions every feature's coordinates into the promoted bbox, so the
/// coarse prune admits a tile if *any* feature overlaps. Checking only `features[0]`
/// in the exact step would then DROP a tile whose later feature is the one that
/// intersects — a false negative beneath a sound coarse prune.
fn bbox_predicate(
    prefix: &str,
    node_id: &str,
    min_lng: f64,
    min_lat: f64,
    max_lng: f64,
    max_lat: f64,
    spatial: bool,
) -> String {
    let coarse = format!(
        "NOT ({p}geo_min_lng > {max_lng} OR {p}geo_max_lng < {min_lng} \
          OR {p}geo_min_lat > {max_lat} OR {p}geo_max_lat < {min_lat})",
        p = prefix
    );
    if !spatial {
        // No spatial extension (mobile: no android spatial binary) → coarse-only
        // superset. A Rust `geo` fine step could refine on hydrated candidates.
        return coarse;
    }
    let box_wkt = format!(
        "POLYGON (({min_lng} {min_lat}, {max_lng} {min_lat}, \
          {max_lng} {max_lat}, {min_lng} {max_lat}, {min_lng} {min_lat}))"
    );
    // ANY feature intersecting qualifies the tile: unnest the FeatureCollection and
    // test each feature's geometry. `data` is `{{ node_id: FeatureCollection }}`.
    let exact = format!(
        "EXISTS (SELECT 1 FROM (SELECT unnest(\
           CAST(json_extract({p}data, '$.\"{node}\".features') AS JSON[])) AS feat) \
         WHERE ST_Intersects(\
           ST_GeomFromGeoJSON(json_extract_string(feat, '$.geometry')), \
           ST_GeomFromText('{box_wkt}')))",
        p = prefix,
        node = sql_lit(node_id)
    );
    format!("({coarse} AND {exact})")
}

/// The path of the first predicate in `expr` that compiles to a COARSE superset
/// under the current backend capabilities, or `None` if every predicate is exact.
///
/// In the duck backend the ONLY coarse leaf is a `Bbox` without the spatial
/// extension (bbox-overlap, no exact ST_Intersects fine step); concept/range are
/// exact index reads and has-link/on-link are exact over the edge table. Recurses
/// through the boolean connectives and into an `OnLink`'s inner predicate — an
/// OnLink over a coarse target set is itself a superset of source resources.
/// `Not` uses this to refuse negating a superset (which would under-approximate).
fn first_coarse_path(expr: &Expr, spatial: bool) -> Option<&str> {
    match expr {
        Expr::Bbox { path, .. } if !spatial => Some(path.as_str()),
        Expr::Bbox { .. } => None,
        Expr::All(children) | Expr::Any(children) | Expr::OnTile(children) => {
            children.iter().find_map(|c| first_coarse_path(c, spatial))
        }
        Expr::Not(inner) => first_coarse_path(inner, spatial),
        Expr::OnLink { r#where, .. } => first_coarse_path(r#where, spatial),
        Expr::Concept { .. } | Expr::Range { .. } | Expr::HasLink { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a bare-alias path to its node and nodegroup id.
fn resolve<'g>(graph: &'g StaticGraph, path: &str) -> Result<(&'g StaticNode, String), DuckError> {
    let node = resolve_node(graph, path)?;
    let ng = node
        .nodegroup_id
        .clone()
        .ok_or_else(|| DuckError::Compile(format!("node '{path}' has no nodegroup")))?;
    Ok((node, ng))
}

/// Resolve a path to its node: a bare alias (global lookup), or a dot-qualified
/// path walked from the root by alias through the schema tree (following edges) —
/// e.g. `address.location`. Mirrors `ros-madair-query`'s `PathResolver` so both
/// compilers resolve paths identically; independent of `build_indices`.
fn resolve_node<'g>(graph: &'g StaticGraph, path: &str) -> Result<&'g StaticNode, DuckError> {
    let components: Vec<&str> = path.split('.').collect();
    if components.len() == 1 {
        return graph
            .find_node_by_alias(components[0])
            .ok_or_else(|| DuckError::Compile(format!("unknown path alias '{path}'")));
    }
    // Dotted: children by edge (domainnode -> rangenode), then walk from the root.
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in graph.edges_slice() {
        children
            .entry(edge.domainnode_id.as_str())
            .or_default()
            .push(edge.rangenode_id.as_str());
    }
    let nodes_by_id: HashMap<&str, &StaticNode> = graph
        .nodes_slice()
        .iter()
        .map(|n| (n.nodeid.as_str(), n))
        .collect();
    let mut current: &StaticNode = graph.get_root();
    for component in &components {
        let child_ids = children
            .get(current.nodeid.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match child_ids
            .iter()
            .filter_map(|id| nodes_by_id.get(id).copied())
            .find(|n| n.alias.as_deref() == Some(*component))
        {
            Some(node) => current = node,
            None => {
                return Err(DuckError::Compile(format!(
                    "unknown path component '{component}' in '{path}'"
                )))
            }
        }
    }
    Ok(current)
}

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
        conn.execute_batch(
            "SET autoinstall_known_extensions=false; SET autoload_known_extensions=false;",
        )
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
