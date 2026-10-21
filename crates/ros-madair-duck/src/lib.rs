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

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::StaticGraph;
use alizarin_core::StaticTile;
use duckdb::Connection;
use ros_madair_compile::{compile_query, sql_lit, CompileError};
use ros_madair_query::Query;

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
impl From<CompileError> for DuckError {
    fn from(e: CompileError) -> Self {
        DuckError::Compile(e.0)
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
    /// the tiles glob) — drives `Expr::OnLink` path predicates. Absent for a head
    /// with no links; an OnLink then errors rather than mis-compiling.
    has_edges: bool,
    /// Whether a `concept_index` view is registered (sibling `concepts_*.parquet`) —
    /// drives `Expr::Concept` (node-scoped membership semijoin + DFS-catalog range join).
    /// Absent for a head with no concept fields; a Concept then errors rather than
    /// mis-compiling.
    has_concept_index: bool,
    /// Whether an `ordered_index` view is registered (sibling `ordered_*.parquet`) —
    /// drives `Expr::Range`. Absent for a head with no date/number fields; a Range
    /// then errors.
    has_ordered: bool,
    /// Whether a `grid` view is registered (sibling `grid_*.parquet`) — drives
    /// `Expr::GridBox` (the cell-sorted spatial prune). Absent for a head with no geo
    /// resources; a GridBox then errors rather than mis-compiling.
    has_grid: bool,
    /// Whether a `resources` view is registered (sibling `resources_*.parquet`, the
    /// per-resource entry table). Drives [`render_summaries`](Self::render_summaries)
    /// and the tile locator in [`resource_tiles`](Self::resource_tiles). Always carried
    /// by a head with resources (one row per resource, with `cluster_key`).
    has_resources: bool,
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
        // Reverse edge index (the sibling redges_*.parquet, the SAME edge rows sorted
        // target-first): registered as `edges_rev` so a TARGET-anchored probe — an
        // untyped reverse lookup or a shared-neighbour self-join — can zone-map prune
        // on target_resource instead of scanning the src-sorted `edges`. Best-effort,
        // like the others: a pre-reverse-index snapshot still OPENS without it. Named
        // `redges_` on disk so this glob cannot also match the forward `edges_*`.
        let edge_rev_glob = parquet_glob.replace("tiles_", "redges_");
        let has_edges_rev = edge_rev_glob != parquet_glob
            && conn
                .execute_batch(&format!(
                    "CREATE VIEW edges_rev AS SELECT * FROM read_parquet('{}');",
                    sql_lit(&edge_rev_glob)
                ))
                .is_ok();
        let _ = has_edges_rev; // wired into the compiler's direction choice in a follow-on
                               // Melted concept / ordered axis stores (the siblings concepts_*.parquet /
                               // ordered_*.parquet next to the tiles glob), same best-effort registration as
                               // edges: a head with no concept/date/number fields still OPENS; only a
                               // Concept/Range query then errors typed. concept_index is the node/value-sorted
                               // membership store (distinct from
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
        // Grid-cell spatial edge (Expr::GridBox): the sibling grid_*.parquet,
        // (cell, resource_id, descriptor, geo_descriptor, render_summary) sorted by cell.
        // Best-effort like the others — a head with no geo resources still OPENS; a
        // GridBox then errors typed.
        let grid_glob = parquet_glob.replace("tiles_", "grid_");
        let has_grid = grid_glob != parquet_glob
            && conn
                .execute_batch(&format!(
                    "CREATE VIEW grid AS SELECT * FROM read_parquet('{}');",
                    sql_lit(&grid_glob)
                ))
                .is_ok();
        // Entry table (resources_*.parquet): one row per resource (slug/model/name
        // spine + render_summary), for render/list/backlink reads. Best-effort.
        let resources_glob = parquet_glob.replace("tiles_", "resources_");
        let has_resources = resources_glob != parquet_glob
            && conn
                .execute_batch(&format!(
                    "CREATE VIEW resources AS SELECT * FROM read_parquet('{}');",
                    sql_lit(&resources_glob)
                ))
                .is_ok();
        Ok(Self {
            conn,
            has_catalog: false,
            has_edges,
            has_concept_index,
            has_ordered,
            has_grid,
            has_resources,
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
        // Two read-side gates per layer:
        //  1. FORMAT_VERSION. Refuse a layer emitted at a version this reader does
        //     not implement — a loud, actionable error (re-emit) instead of a
        //     cryptic failure on a since-renamed/dropped column downstream. A layer
        //     with no manifest (a raw local glob) is not gated; a `format_version`
        //     of 0 (a pre-versioning manifest) is refused, since no reader is v0.
        //  2. Tamper. Refuse a layer whose SIGNED content has been altered since it
        //     was signed — closes the sign→verify loop. A layer with no
        //     manifest/attestations (a local or unsigned build) verifies as
        //     Unsigned, or is simply not a signed snapshot; both proceed. Only a
        //     proven tamper (`Failed`) is fatal.
        for dir in dirs {
            if let Ok(bytes) = std::fs::read(dir.join("manifest.json")) {
                if let Ok(m) = serde_json::from_slice::<ros_madair_format::Manifest>(&bytes) {
                    if m.format_version != ros_madair_format::FORMAT_VERSION {
                        return Err(DuckError::Compile(format!(
                            "layer {}: snapshot FORMAT_VERSION {} is not supported by this \
                             reader (expected {}); re-emit the snapshot",
                            dir.display(),
                            m.format_version,
                            ros_madair_format::FORMAT_VERSION
                        )));
                    }
                }
            }
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
        // Same shadow-set composition as the stores, so a filter that is NOT the
        // dedup key — above all a SPATIAL box on the promoted geo_* columns — pushes
        // into each layer's scan and prunes on the row-group bbox zone-maps, instead
        // of the old `row_number() OVER (PARTITION BY resource_id, nodegroup_id)`
        // window forcing a full tile scan to resolve the dedup. The switch also fixes
        // that window's one-row-per-(resource,nodegroup) semantics: shadow-set keeps
        // ALL tiles of the winning layer, correct for a cardinality-n nodegroup (the
        // old form silently dropped all but one of its tiles).
        conn.execute_batch(&format!(
            "CREATE VIEW tiles AS {};",
            shadow_set_union(&tile_layers, &["resource_id", "nodegroup_id"], "*")
        ))?;

        // Edges (optional): keep ALL edges from the winning layer per
        // (src_resource, src_nodegroup) — an overlay that re-links a tile replaces
        // that tile's whole edge set, not one row.
        let edge_layers = layers_with(&conn, dirs, "edges_*.parquet");
        let has_edges = !edge_layers.is_empty();
        if has_edges {
            conn.execute_batch(&format!(
                "CREATE VIEW edges AS {};",
                shadow_set_union(
                    &edge_layers,
                    &["src_resource", "src_nodegroup"],
                    "src_resource, src_node, src_nodegroup, src_tile, target_resource",
                )
            ))?;
        }
        // Reverse edge index across layers (the sibling redges_*.parquet), so an
        // untyped `Backlink` prunes on target across a base+overlay composition.
        // Same winning-layer shadow-set as edges; named redges_ so the edges_*
        // glob above can't swallow it.
        let rev_layers = layers_with(&conn, dirs, "redges_*.parquet");
        if !rev_layers.is_empty() {
            conn.execute_batch(&format!(
                "CREATE VIEW edges_rev AS {};",
                shadow_set_union(
                    &rev_layers,
                    &["src_resource", "src_nodegroup"],
                    "src_resource, src_node, src_nodegroup, src_tile, target_resource",
                )
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
                 SELECT concept_id, dfs_enter, dfs_leave, label, value_ids FROM ({}) \
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
                "CREATE VIEW concept_index AS {};",
                shadow_set_union(
                    &concept_layers,
                    &["resource_id", "nodegroup_id"],
                    "resource_id, tile_id, nodegroup_id, node_id, concept_id",
                )
            ))?;
        }
        let ordered_layers = layers_with(&conn, dirs, "ordered_*.parquet");
        let has_ordered = !ordered_layers.is_empty();
        if has_ordered {
            conn.execute_batch(&format!(
                "CREATE VIEW ordered_index AS {};",
                shadow_set_union(
                    &ordered_layers,
                    &["resource_id", "nodegroup_id"],
                    "resource_id, tile_id, nodegroup_id, node_id, q_ordered",
                )
            ))?;
        }
        // Grid edge across layers: a resource's cell is a whole-resource fact (one
        // row), so the shadow key is `resource_id` alone — the topmost layer that
        // carries the resource supersedes its cell (an overlay that moves/re-geocodes
        // a resource replaces its grid row).
        let grid_layers = layers_with(&conn, dirs, "grid_*.parquet");
        let has_grid = !grid_layers.is_empty();
        if has_grid {
            conn.execute_batch(&format!(
                "CREATE VIEW grid AS {};",
                shadow_set_union(
                    &grid_layers,
                    &["resource_id"],
                    "cell, resource_id, descriptor, geo_descriptor, render_summary",
                )
            ))?;
        }
        // Entry table across layers: one row per resource, so the shadow key is
        // `resource_id` — the topmost layer's row wins (an overlay that re-titles a
        // resource supersedes the base entry).
        let resource_layers = layers_with(&conn, dirs, "resources_*.parquet");
        let has_resources = !resource_layers.is_empty();
        if has_resources {
            conn.execute_batch(&format!(
                "CREATE VIEW resources AS {};",
                shadow_set_union(
                    &resource_layers,
                    &["resource_id"],
                    "resource_id, graph_id, legacyid, descriptor, cluster_key",
                )
            ))?;
        }

        Ok(Self {
            conn,
            has_catalog,
            has_edges,
            has_concept_index,
            has_ordered,
            has_grid,
            has_resources,
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

    /// Per-resource render summary for a set of resource ids — the compact
    /// `{nodeId:[{id,title}]}` related-title blob (e.g. the linked site → ward) emit
    /// carried on the grid edge, PLUS the resource's own descriptor and its
    /// `geo_descriptor` (a geojson `Point`, v4). The render header without a tile
    /// fetch — descriptor for the chip, geo_descriptor for the map pin, summary
    /// for the card — read from one small store. Each value is per the tuple order
    /// `(descriptor, geo_descriptor, render_summary)`, matching the grid columns.
    /// Empty without a grid view; `geo_descriptor` is `None` for a non-geo resource.
    /// (A spatial result reads these columns for free from the same cell-pruned
    /// `GridBox` scan; this by-id method serves non-spatial renders and
    /// native/python callers.)
    pub fn render_summaries(
        &self,
        uris: &[String],
    ) -> Result<HashMap<String, (Option<String>, Option<String>, Option<String>)>, DuckError> {
        let mut out = HashMap::new();
        if uris.is_empty() || !self.has_grid {
            return Ok(out);
        }
        let placeholders = std::iter::repeat_n("?", uris.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT resource_id, descriptor, geo_descriptor, render_summary FROM grid \
             WHERE resource_id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(duckdb::params_from_iter(uris.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, descriptor, geo_descriptor, summary) = row?;
            out.entry(id)
                .or_insert((descriptor, geo_descriptor, summary));
        }
        Ok(out)
    }

    /// Entry-table rows for a set of resource ids: `(graph_id, legacyid, descriptor)`
    /// read from the per-resource `resources` view — the slug/model-scoping/name
    /// spine a consumer lists, counts, and slug-backlinks on (consumer limitation #1).
    /// Small (no hydration `data`), sorted by `resource_id` so an id-probe prunes.
    /// Empty for a head with no `resources` view.
    pub fn resource_entries(
        &self,
        uris: &[String],
    ) -> Result<HashMap<String, (String, Option<String>, Option<String>)>, DuckError> {
        let mut out = HashMap::new();
        if uris.is_empty() || !self.has_resources {
            return Ok(out);
        }
        let placeholders = std::iter::repeat_n("?", uris.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT resource_id, graph_id, legacyid, descriptor FROM resources \
             WHERE resource_id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(duckdb::params_from_iter(uris.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, graph_id, legacyid, descriptor) = row?;
            out.entry(id).or_insert((graph_id, legacyid, descriptor));
        }
        Ok(out)
    }

    /// All `(id, label)` from the catalog — the bulk id→label dump (Gréasán
    /// `v2_closure`, and the native hydrate label map), keyed by BOTH the concept id
    /// AND every prefLabel VALUE id that concept owns (`value_ids`). A tile stores the
    /// VALUE id (Arches: `concept.id != prefLabels.en.id`), so a value-id-keyed entry
    /// is what lets hydrate resolve a tile's reference to its label — without it the
    /// raw value-id leaks (the v5 fix). Empty without a catalog attached.
    pub fn concept_labels(&self) -> Result<HashMap<String, String>, DuckError> {
        let mut out = HashMap::new();
        if !self.has_catalog {
            return Ok(out);
        }
        // Concept-id -> label AND value-id -> label (unnest the per-concept value_ids),
        // so the map resolves whichever identity a tile happens to carry.
        let mut stmt = self.conn.prepare(
            "SELECT concept_id, label FROM concepts WHERE label IS NOT NULL AND label <> '' \
             UNION ALL \
             SELECT unnest(value_ids), label FROM concepts \
             WHERE value_ids IS NOT NULL AND label IS NOT NULL AND label <> ''",
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
        // Tile locator, as TWO pruned reads. Tiles are sorted by (cluster_key, resource_id),
        // so a bare `resource_id = ?` can't skip row groups — ids scatter across every
        // cluster-key group (the N^1.2–1.4 on entry pages).
        //   Step 1: look the resource's cluster_key up in the resource_id-sorted entry table
        //           (prunes on resource_id).
        //   Step 2: BIND that key as a literal constant into the tile scan. A bound constant
        //           is what lets DuckDB's parquet reader skip row groups by the cluster_key
        //           zone-map; a scalar subquery (`cluster_key = (SELECT …)`) is NOT reliably
        //           pushed into row-group pruning on bundled DuckDB, which would leave this
        //           correct but still O(N) — the silent regression the scale harness exists
        //           to catch. `resource_id = ?` is then the exact within-cell filter.
        // Empty if the resource is absent from the entry table (not an error).
        let mut key_stmt = self
            .conn
            .prepare("SELECT cluster_key FROM resources WHERE resource_id = ?")?;
        let mut keys = key_stmt.query_map([uuid], |r| r.get::<_, u64>(0))?;
        let cluster_key: u64 = match keys.next() {
            Some(k) => k?,
            None => return Ok(Vec::new()),
        };
        drop(keys);
        drop(key_stmt);
        let mut stmt = self.conn.prepare(
            "SELECT nodegroup_id, tileid, parenttile_id, sortorder, data \
             FROM tiles \
             WHERE cluster_key = ? AND resource_id = ?",
        )?;
        let rows = stmt.query_map(duckdb::params![cluster_key, uuid], |r| {
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
        Ok(compile_query(
            query,
            graph,
            graphs,
            registry,
            self.has_catalog,
            self.has_edges,
            self.has_concept_index,
            self.has_ordered,
            self.has_grid,
            self.spatial,
        )?)
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

/// Compose layers keeping every row of the HIGHEST-ranked layer that carries
/// each `keys` tuple — the same winning-layer semantics as a
/// `max(_layer) OVER (PARTITION BY keys)` window, but WITHOUT a window over the
/// base.
///
/// Why not the window: a window partitioned by `keys` blocks predicate pushdown
/// for any filter that is NOT one of `keys` (pushing it would change which layer
/// wins per partition). So a selective cross-layer query — a spatial box, a
/// `Concept` semijoin, an `OnLink` target filter — full-scans the whole base to
/// resolve the window before filtering (measured ~50× on a selective edge
/// probe). Instead, each layer anti-joins only against the (small) set of keys
/// that HIGHER layers redefine: overlays are tiny, so the base's anti-join build
/// side is tiny AND the base keeps its own pushable filters. Equivalent output
/// (topmost layer per key wins, all its rows kept), pruning preserved.
///
/// `cols` is the projected column list; `keys` the shadow key columns. A single
/// layer yields a bare scan (no shadowing).
fn shadow_set_union(layers: &[(usize, String)], keys: &[&str], cols: &str) -> String {
    let mut sorted = layers.to_vec();
    sorted.sort_by_key(|(r, _)| *r);
    let key_proj = keys.join(", ");
    sorted
        .iter()
        .enumerate()
        .map(|(i, (_r, glob))| {
            let higher = &sorted[i + 1..];
            if higher.is_empty() {
                format!("SELECT {cols} FROM read_parquet('{}') base", sql_lit(glob))
            } else {
                let higher_keys = higher
                    .iter()
                    .map(|(_, g)| format!("SELECT {key_proj} FROM read_parquet('{}')", sql_lit(g)))
                    .collect::<Vec<_>>()
                    .join(" UNION ALL ");
                let on = keys
                    .iter()
                    .map(|k| format!("h.{k} = base.{k}"))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                format!(
                    "SELECT {cols} FROM read_parquet('{}') base \
                     WHERE NOT EXISTS (SELECT 1 FROM ({higher_keys}) h WHERE {on})",
                    sql_lit(glob)
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL BY NAME\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_links_and_runs() {
        let conn = Connection::open_in_memory().unwrap();
        let v: i64 = conn.query_row("SELECT 1 + 41", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 42);
    }

    /// The layer-pushdown guard: `shadow_set_union` must let a selective filter
    /// PRUNE the base scan, where the old `max(_layer) OVER (PARTITION BY …)`
    /// window forced a full base scan. Builds a large base + tiny overlay, composes
    /// the edges view both ways, and compares the largest scan cardinality each
    /// plan reaches (via EXPLAIN ANALYZE). If a window is ever reintroduced this
    /// fails loudly. Also checks the generated SQL carries no window and the exact
    /// same result set as the window form.
    #[test]
    fn shadow_set_union_prunes_where_the_window_scanned() {
        const BASE: i64 = 50_000;
        const DOMAIN: i64 = 50; // target filter ~ BASE/DOMAIN rows
        let dir = std::env::temp_dir().join(format!("rm-shadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("edges_base.parquet");
        let overlay = dir.join("edges_overlay.parquet");
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT ('r'||i)::VARCHAR AS src_resource, 'works_at' AS src_node, \
                   'ng' AS src_nodegroup, ('tl'||i)::VARCHAR AS src_tile, \
                   ('t'||lpad((i%{DOMAIN})::VARCHAR,4,'0')) AS target_resource \
             FROM range({BASE}) t(i) ORDER BY target_resource, src_resource) \
             TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE 2048);",
            base.display()
        ))
        .unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT ('o'||i)::VARCHAR AS src_resource, 'works_at' AS src_node, \
                   'ng' AS src_nodegroup, ('ot'||i)::VARCHAR AS src_tile, 't0000' AS target_resource \
             FROM range(10) t(i)) TO '{}' (FORMAT PARQUET, ROW_GROUP_SIZE 2048);",
            overlay.display()
        ))
        .unwrap();

        let layers = vec![
            (0usize, base.display().to_string()),
            (1usize, overlay.display().to_string()),
        ];
        let cols = "src_resource, src_node, src_nodegroup, src_tile, target_resource";
        let shadow_sql = shadow_set_union(&layers, &["src_resource", "src_nodegroup"], cols);
        assert!(
            !shadow_sql.to_uppercase().contains("OVER ("),
            "no window in the shadow-set form"
        );
        assert!(
            shadow_sql.contains("NOT EXISTS"),
            "base layer anti-joins the overlay's keys"
        );

        // The old window composition, for the pruning comparison.
        let window_sql = format!(
            "SELECT {cols} FROM (SELECT *, max(_layer) OVER (PARTITION BY src_resource, src_nodegroup) \
             AS _win FROM (SELECT *, 0 AS _layer FROM read_parquet('{}') UNION ALL BY NAME \
             SELECT *, 1 AS _layer FROM read_parquet('{}'))) WHERE _layer = _win",
            base.display(),
            overlay.display()
        );

        // Largest scan cardinality any operator in the plan reaches.
        let max_rows = |view_sql: &str| -> i64 {
            conn.execute_batch("DROP VIEW IF EXISTS e;").ok();
            conn.execute_batch(&format!("CREATE VIEW e AS {view_sql};"))
                .unwrap();
            let plan: String = conn
                .query_row(
                    "EXPLAIN ANALYZE SELECT count(*) FROM e WHERE target_resource = 't0000'",
                    [],
                    |r| r.get::<_, String>(1),
                )
                .unwrap();
            plan.split_whitespace()
                .filter_map(|tok| {
                    let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
                    (!digits.is_empty() && tok.chars().all(|c| c.is_ascii_digit() || c == ','))
                        .then(|| digits.parse::<i64>().ok())
                        .flatten()
                })
                .max()
                .unwrap_or(0)
        };
        let shadow_max = max_rows(&shadow_sql);
        let window_max = max_rows(&window_sql);
        eprintln!("max scan cardinality — shadow: {shadow_max}, window: {window_max}");
        assert!(
            shadow_max * 3 < window_max,
            "shadow-set must prune the base (shadow {shadow_max} vs window {window_max})"
        );
        assert!(
            shadow_max < BASE,
            "shadow-set did not full-scan the base ({shadow_max})"
        );

        // And the composition still returns the SAME rows as the window form.
        let count = |view_sql: &str| -> i64 {
            conn.execute_batch("DROP VIEW IF EXISTS e;").ok();
            conn.execute_batch(&format!("CREATE VIEW e AS {view_sql};"))
                .unwrap();
            conn.query_row("SELECT count(*) FROM e", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            count(&shadow_sql),
            count(&window_sql),
            "shadow-set == window result set"
        );
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
