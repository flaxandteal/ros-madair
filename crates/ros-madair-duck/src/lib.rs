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

/// Load the `httpfs` extension so `read_parquet('https://…')` range-reads a
/// remote Parquet (reads the footer, then only the row groups the zone-map keeps)
/// instead of pulling the whole file. This is the native analogue of the
/// browser's `INSTALL httpfs; LOAD httpfs;` (duckdb-wasm) — it is what lets the
/// full corpus live on R2/S3/any-Range-CDN and a reader pull a slice.
///
/// `httpfs` is a RUNTIME extension (not a cargo feature, no libduckdb rebuild).
/// `Auto` installs it over the network (dev/online); `OfflineDir` loads the
/// bundled per-platform binary with autoinstall off (the on-device path, exactly
/// like spatial). Public `https://` objects (e.g. R2 custom-domain) need NO S3
/// credentials — plain HTTP Range. Errors propagate: without httpfs an `https`
/// read cannot work, so a failed load must be loud, not a later cryptic bind error.
fn configure_httpfs(conn: &Connection, source: &SpatialSource) -> Result<(), DuckError> {
    match source {
        SpatialSource::None => Ok(()),
        SpatialSource::Auto => {
            conn.execute_batch("INSTALL httpfs; LOAD httpfs;")?;
            Ok(())
        }
        SpatialSource::OfflineDir(dir) => {
            conn.execute_batch(&format!(
                "SET autoinstall_known_extensions=false; \
                 SET extension_directory='{}'; \
                 LOAD httpfs;",
                sql_lit(&dir.display().to_string())
            ))?;
            Ok(())
        }
    }
}

/// Register a view over an explicit list of Parquet URLs (or local paths), unioned
/// by name so several models coexist in one view. `hive` adds Hive partitioning so
/// a partitioned `tiles_<g>/nodegroup_id=…/` set recovers the `nodegroup_id` column.
/// Returns `false` (view NOT created) when `urls` is empty — the caller's `has_*`
/// flag — so a snapshot missing a store opens without it.
fn create_url_view(
    conn: &Connection,
    name: &str,
    urls: &[String],
    hive: bool,
) -> Result<bool, DuckError> {
    if urls.is_empty() {
        return Ok(false);
    }
    let list = urls
        .iter()
        .map(|u| format!("'{}'", sql_lit(u)))
        .collect::<Vec<_>>()
        .join(", ");
    let opts = if hive {
        ", hive_partitioning=true, union_by_name=true"
    } else {
        ", union_by_name=true"
    };
    conn.execute_batch(&format!(
        "CREATE VIEW {name} AS SELECT * FROM read_parquet([{list}]{opts});"
    ))?;
    Ok(true)
}

/// Whether a view of this name is registered on the connection — so [`DuckReader::excise`]
/// COPYs only the stores a snapshot actually carries.
fn view_exists(conn: &Connection, name: &str) -> Result<bool, DuckError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM duckdb_views() WHERE view_name = ?",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Whether the spatial extension loaded (ST_Intersects available) — the switch
/// between the exact and coarse-only `Bbox` paths. Shared by the http openers.
fn probe_spatial(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) > 0 FROM duckdb_functions() WHERE lower(function_name) = 'st_intersects'",
        [],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// The per-store URL lists of ONE remote snapshot (layer), grouped from its manifest.
struct LayerStores {
    tiles: Vec<String>,
    edges: Vec<String>,
    redges: Vec<String>,
    concepts: Vec<String>,
    ordered: Vec<String>,
    grid: Vec<String>,
    resources: Vec<String>,
    catalog: Vec<String>,
}

/// Read a remote snapshot's `manifest.json` THROUGH duckdb (httpfs json reader — no
/// extra HTTP client) and group its artifact paths into per-store URL lists. Shared by
/// [`DuckReader::open_http`] (one snapshot) and [`DuckReader::open_http_layers`]
/// (compose several). `artifacts` is a list of `{path, bytes, sha256}` structs.
fn http_layer_stores(conn: &Connection, base_url: &str) -> Result<LayerStores, DuckError> {
    let base = base_url.trim_end_matches('/').to_string();
    let manifest_url = format!("{base}/manifest.json");
    let mut stmt = conn.prepare(&format!(
        "SELECT a.path AS f FROM (SELECT UNNEST(artifacts) AS a \
         FROM read_json_auto('{}')) t",
        sql_lit(&manifest_url)
    ))?;
    let files: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    let mut s = LayerStores {
        tiles: vec![],
        edges: vec![],
        redges: vec![],
        concepts: vec![],
        ordered: vec![],
        grid: vec![],
        resources: vec![],
        catalog: vec![],
    };
    // Order matters: `redges_`/`concept_catalog` before the `edges_`/`concepts_`
    // prefixes they could be confused with.
    for f in &files {
        let url = format!("{base}/{f}");
        if f.starts_with("tiles_") {
            s.tiles.push(url);
        } else if f.starts_with("redges_") {
            s.redges.push(url);
        } else if f.starts_with("edges_") {
            s.edges.push(url);
        } else if f.starts_with("concept_catalog") {
            s.catalog.push(url);
        } else if f.starts_with("concepts_") {
            s.concepts.push(url);
        } else if f.starts_with("ordered_") {
            s.ordered.push(url);
        } else if f.starts_with("grid_") {
            s.grid.push(url);
        } else if f.starts_with("resources_") {
            s.resources.push(url);
        }
    }
    Ok(s)
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
    /// The tier's field cut, from the top layer's `manifest.tier.exclude_nodegroups`
    /// (L1 audited deny). A query predicate whose path resolves into one of these is
    /// REFUSED at compile time with a typed "denied — out of scope" error instead of a
    /// silent-empty scan. Empty for a single-head/untiered reader (no manifest read).
    excluded_nodegroups: std::collections::HashSet<String>,
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
            // Single-head reader: no manifest, so no tier (L1 is a layered/signed-head
            // feature). An untiered reader never refuses on scope.
            excluded_nodegroups: std::collections::HashSet::new(),
        })
    }

    /// Open a reader over a REMOTE snapshot served from a Range-honouring host
    /// (R2 / S3 / any CDN), driven by its `manifest.json`. This is the "serve the
    /// whole corpus from the CDN, pull only a slice" path: `httpfs` range-reads, and
    /// the manifest's artifact list is enumerated into explicit `read_parquet([...])`
    /// file lists — necessary because httpfs CANNOT expand a glob over plain HTTP
    /// (R2 has no directory listing), so the Hive-partitioned
    /// `tiles_<g>/…/data_0.parquet` partitions are named explicitly
    /// (`hive_partitioning=true` recovers the `nodegroup_id` column). Every model in
    /// the snapshot unions into one set of views (resource/node ids are globally
    /// unique), so a cross-model hop resolves. The manifest is read THROUGH duckdb's
    /// json reader over httpfs — no extra HTTP client dependency.
    pub fn open_http(base_url: &str) -> Result<Self, DuckError> {
        let conn = Connection::open_in_memory()?;
        configure_httpfs(&conn, &SpatialSource::Auto)?;
        // GridBox (the spatial seed) prunes on the grid's `cell` zone-map and needs
        // no spatial extension; only a Bbox fine-step would. Load it best-effort so
        // an exact Bbox still works online, but never fail the open on it.
        let _ = conn.execute_batch("INSTALL spatial; LOAD spatial;");
        let spatial = probe_spatial(&conn);

        let s = http_layer_stores(&conn, base_url)?;
        if !create_url_view(&conn, "tiles", &s.tiles, true)? {
            return Err(DuckError::Compile(format!(
                "open_http: manifest at {base_url}/manifest.json listed no tiles_* artifacts"
            )));
        }
        let has_edges = create_url_view(&conn, "edges", &s.edges, false)?;
        let _ = create_url_view(&conn, "edges_rev", &s.redges, false)?;
        let has_concept_index = create_url_view(&conn, "concept_index", &s.concepts, false)?;
        let has_ordered = create_url_view(&conn, "ordered_index", &s.ordered, false)?;
        let has_grid = create_url_view(&conn, "grid", &s.grid, false)?;
        let has_resources = create_url_view(&conn, "resources", &s.resources, false)?;
        let has_catalog = create_url_view(&conn, "concepts", &s.catalog, false)?;

        Ok(Self {
            conn,
            has_catalog,
            has_edges,
            has_concept_index,
            has_ordered,
            has_grid,
            has_resources,
            spatial,
            excluded_nodegroups: std::collections::HashSet::new(),
        })
    }

    /// Open a reader over SEVERAL remote layer snapshots (base-first), composed with
    /// overlay precedence — the HTTP analogue of [`open_layers`](Self::open_layers),
    /// reusing the exact same `shadow_set_union`/`ranked_union` composition, only with
    /// URL-list sources instead of local globs. Used to resolve an offline slice's
    /// closure over EXACTLY the chosen alizarin layers: a hop crosses layers (the
    /// composed views union them), so a seed building in the base layer reaches a
    /// keyholder in an overlay. Each layer's RAW rows are excised separately (re-open
    /// with [`open_http`](Self::open_http)), so the exported slice re-composes
    /// dynamically offline rather than being flattened.
    pub fn open_http_layers(base_urls: &[&str]) -> Result<Self, DuckError> {
        if base_urls.is_empty() {
            return Err(DuckError::Compile("open_http_layers: no layers".into()));
        }
        let conn = Connection::open_in_memory()?;
        configure_httpfs(&conn, &SpatialSource::Auto)?;
        let _ = conn.execute_batch("INSTALL spatial; LOAD spatial;");
        let spatial = probe_spatial(&conn);

        // Per store, the (rank, source-expr) of each layer that carries it.
        let (mut tiles, mut edges, mut redges) = (Vec::new(), Vec::new(), Vec::new());
        let (mut concepts, mut ordered, mut grid) = (Vec::new(), Vec::new(), Vec::new());
        let (mut resources, mut catalog) = (Vec::new(), Vec::new());
        for (i, base) in base_urls.iter().enumerate() {
            let s = http_layer_stores(&conn, base)?;
            if !s.tiles.is_empty() {
                tiles.push((i, url_src(&s.tiles, true)));
            }
            if !s.edges.is_empty() {
                edges.push((i, url_src(&s.edges, false)));
            }
            if !s.redges.is_empty() {
                redges.push((i, url_src(&s.redges, false)));
            }
            if !s.concepts.is_empty() {
                concepts.push((i, url_src(&s.concepts, false)));
            }
            if !s.ordered.is_empty() {
                ordered.push((i, url_src(&s.ordered, false)));
            }
            if !s.grid.is_empty() {
                grid.push((i, url_src(&s.grid, false)));
            }
            if !s.resources.is_empty() {
                resources.push((i, url_src(&s.resources, false)));
            }
            if !s.catalog.is_empty() {
                catalog.push((i, url_src(&s.catalog, false)));
            }
        }
        if tiles.is_empty() {
            return Err(DuckError::Compile(
                "open_http_layers: no tiles_* in any layer".into(),
            ));
        }
        conn.execute_batch(&format!(
            "CREATE VIEW tiles AS {};",
            shadow_set_union(&tiles, &["resource_id", "nodegroup_id"], "*")
        ))?;
        // Projections are `*` (not explicit column lists): the composition only needs
        // the KEY columns to exist, and `*` + `UNION ALL BY NAME` tolerates schema
        // drift between a corpus's snapshot version and this reader (the R2 backdrop is
        // an older grid schema), filling absent columns with NULL rather than binding-
        // erroring. (The composed reader is used to resolve the closure — tiles+edges —
        // so a looser projection on the other stores costs nothing.)
        let has_edges = !edges.is_empty();
        if has_edges {
            conn.execute_batch(&format!(
                "CREATE VIEW edges AS {};",
                shadow_set_union(&edges, &["src_resource", "src_nodegroup"], "*")
            ))?;
        }
        if !redges.is_empty() {
            conn.execute_batch(&format!(
                "CREATE VIEW edges_rev AS {};",
                shadow_set_union(&redges, &["src_resource", "src_nodegroup"], "*")
            ))?;
        }
        let has_catalog = !catalog.is_empty();
        if has_catalog {
            conn.execute_batch(&format!(
                "CREATE VIEW concepts AS \
                 SELECT concept_id, dfs_enter, dfs_leave, label, value_ids FROM ({}) \
                 QUALIFY row_number() OVER (PARTITION BY concept_id ORDER BY _layer DESC) = 1;",
                ranked_union(&catalog)
            ))?;
        }
        let has_concept_index = !concepts.is_empty();
        if has_concept_index {
            conn.execute_batch(&format!(
                "CREATE VIEW concept_index AS {};",
                shadow_set_union(&concepts, &["resource_id", "nodegroup_id"], "*")
            ))?;
        }
        let has_ordered = !ordered.is_empty();
        if has_ordered {
            conn.execute_batch(&format!(
                "CREATE VIEW ordered_index AS {};",
                shadow_set_union(&ordered, &["resource_id", "nodegroup_id"], "*")
            ))?;
        }
        let has_grid = !grid.is_empty();
        if has_grid {
            conn.execute_batch(&format!(
                "CREATE VIEW grid AS {};",
                shadow_set_union(&grid, &["resource_id"], "*")
            ))?;
        }
        let has_resources = !resources.is_empty();
        if has_resources {
            conn.execute_batch(&format!(
                "CREATE VIEW resources AS {};",
                shadow_set_union(&resources, &["resource_id"], "*")
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
            excluded_nodegroups: std::collections::HashSet::new(),
        })
    }

    /// Build the local directory of a SLICE of this (typically remote) snapshot,
    /// named so [`open`](Self::open) re-reads it OFFLINE (`tiles_<stem>.parquet`,
    /// `edges_<stem>.parquet`, …). Two grades of membership:
    ///
    /// - **`closure`** (the seed + queryable hops) is **full grade**: tiles, concepts,
    ///   ordered, grid, resources — hydratable and queryable offline.
    /// - **`ref_only`** (e.g. the 2-hop targets) is **reference grade**: only the
    ///   `resources`/`grid` entry rows (id, descriptor, render_summary, geo point) and
    ///   the `edges` that connect to them — so offline you can *name, link and locate*
    ///   a reference, but not open or query it. NOT its tiles/concepts/ordered.
    ///
    /// Edge rows are kept only when BOTH endpoints are present (in `closure ∪ ref_only`),
    /// so the slice has no edges dangling to resources it does not carry. `geo_bbox`
    /// (`[min_lng, min_lat, max_lng, max_lat]`), when given, additionally prunes the
    /// tile read on the `geo_*` zone-map — non-geo tiles (NULL geo, e.g. a keyholder)
    /// are always kept — so excising the spatial seed touches a few row groups, not the
    /// whole backdrop. Returns the written file paths. (The derived-provenance manifest
    /// is layered on by [`export_offline`].)
    pub fn excise(
        &self,
        closure: &[String],
        ref_only: &[String],
        geo_bbox: Option<[f64; 4]>,
        out_dir: &Path,
        stem: &str,
    ) -> Result<Vec<std::path::PathBuf>, DuckError> {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| DuckError::Compile(format!("excise: mkdir {}: {e}", out_dir.display())))?;
        // _closure = full grade; _all = closure ∪ ref_only = everything the slice names.
        self.conn.execute_batch(
            "CREATE OR REPLACE TEMP TABLE _closure(id VARCHAR); \
             CREATE OR REPLACE TEMP TABLE _all(id VARCHAR);",
        )?;
        {
            let mut app = self.conn.appender("_closure")?;
            for id in closure {
                app.append_row([id.as_str()])?;
            }
            app.flush()?;
        }
        {
            let mut app = self.conn.appender("_all")?;
            for id in closure.iter().chain(ref_only.iter()) {
                app.append_row([id.as_str()])?;
            }
            app.flush()?;
        }
        let in_closure = "resource_id IN (SELECT id FROM _closure)";
        let in_all = "resource_id IN (SELECT id FROM _all)";
        let geo = match geo_bbox {
            Some([minx, miny, maxx, maxy]) => format!(
                " AND (geo_min_lng IS NULL OR NOT(geo_max_lng < {minx} OR geo_min_lng > {maxx} \
                 OR geo_max_lat < {miny} OR geo_min_lat > {maxy}))"
            ),
            None => String::new(),
        };
        // Edges: both endpoints present (no dangling references out of the slice).
        let edge_filter = "src_resource IN (SELECT id FROM _all) \
                           AND target_resource IN (SELECT id FROM _all)";
        // (view, output basename, row filter). Full-grade stores filter on _closure;
        // the entry/edge stores filter on _all so a reference is named, linkable, located.
        let plan: [(&str, String, String); 7] = [
            ("tiles", format!("tiles_{stem}.parquet"), format!("{in_closure}{geo}")),
            ("edges", format!("edges_{stem}.parquet"), edge_filter.to_string()),
            ("edges_rev", format!("redges_{stem}.parquet"), edge_filter.to_string()),
            ("concept_index", format!("concepts_{stem}.parquet"), in_closure.to_string()),
            ("ordered_index", format!("ordered_{stem}.parquet"), in_closure.to_string()),
            ("grid", format!("grid_{stem}.parquet"), in_all.to_string()),
            ("resources", format!("resources_{stem}.parquet"), in_all.to_string()),
        ];
        let mut written = Vec::new();
        for (view, fname, filter) in plan {
            if !view_exists(&self.conn, view)? {
                continue;
            }
            let path = out_dir.join(&fname);
            self.conn.execute_batch(&format!(
                "COPY (SELECT * FROM {view} WHERE {filter}) TO '{}' (FORMAT PARQUET);",
                sql_lit(&path.display().to_string())
            ))?;
            written.push(path);
        }
        Ok(written)
    }

    /// Seed helper: distinct resource ids whose tile geometry overlaps `bbox`
    /// (`[min_lng, min_lat, max_lng, max_lat]`), zone-map-pruned, up to `limit`.
    pub fn ids_in_bbox(&self, bbox: [f64; 4], limit: usize) -> Result<Vec<String>, DuckError> {
        let [minx, miny, maxx, maxy] = bbox;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT resource_id FROM tiles \
             WHERE geo_min_lng IS NOT NULL \
               AND NOT(geo_max_lng < {minx} OR geo_min_lng > {maxx} \
                       OR geo_max_lat < {miny} OR geo_min_lat > {maxy}) \
             ORDER BY resource_id LIMIT {limit}"
        ))?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// One-hop neighbours of `ids` over the edge table, BOTH directions (resources
    /// that reference them — e.g. a keyholder → building — and resources they
    /// reference). The first hop of an offline slice's closure.
    pub fn neighbours(&self, ids: &[String]) -> Result<Vec<String>, DuckError> {
        if ids.is_empty() || !self.has_edges {
            return Ok(Vec::new());
        }
        let list = ids
            .iter()
            .map(|u| format!("'{}'", sql_lit(u)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT src_resource AS id FROM edges WHERE target_resource IN ({list}) \
             UNION \
             SELECT DISTINCT target_resource AS id FROM edges WHERE src_resource IN ({list})"
        ))?;
        let out = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(out)
    }

    /// A representative geo point `(lng, lat)` from the corpus (the first tile with
    /// geometry) — for picking a demo AO without knowing the extent up front.
    pub fn sample_point(&self) -> Result<Option<(f64, f64)>, DuckError> {
        match self.conn.query_row(
            "SELECT geo_min_lng, geo_min_lat FROM tiles WHERE geo_min_lng IS NOT NULL LIMIT 1",
            [],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)),
        ) {
            Ok(p) => Ok(Some(p)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Row count of the `tiles` view (whole snapshot, or a slice). Test/diagnostic.
    pub fn count_tiles(&self) -> Result<usize, DuckError> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM tiles", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    /// Distinct resource ids that have TILES (full/hydratable grade). Test/diagnostic.
    pub fn tile_resource_ids(&self) -> Result<Vec<String>, DuckError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT resource_id FROM tiles")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Resource ids present in the `resources` entry table (named/linkable — the
    /// grade a reference reaches even without tiles). Empty if no entry store.
    pub fn entry_resource_ids(&self) -> Result<Vec<String>, DuckError> {
        if !self.has_resources {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare("SELECT resource_id FROM resources")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Raw hydration `data` JSON blobs of a resource's tiles (includes the alizarin
    /// `__cache` of its directly-linked resources). Diagnostic — used to confirm the
    /// cache gives the 2-hop display for free.
    pub fn resource_data_blobs(&self, id: &str) -> Result<Vec<String>, DuckError> {
        let mut stmt = self.conn.prepare("SELECT data FROM tiles WHERE resource_id = ?")?;
        let v = stmt
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(v)
    }

    /// Read ONE node's geojson value (e.g. the footprint FeatureCollection) from the tile
    /// `data` for a set of resources — the VIEWPORT-FETCH path for real footprints at scale.
    /// For large corpora the polygon is NOT promoted onto the grid (that would bloat it to
    /// hundreds of MB); it stays in `data`, and the map fetches just the visible buildings'
    /// footprints with this one batched scan (`resource_id IN (...)`). Returns
    /// `{resource_id: node_json}` (only resources whose tile carries the node).
    pub fn node_geoms(
        &self,
        ids: &[String],
        node_id: &str,
    ) -> Result<std::collections::HashMap<String, String>, DuckError> {
        use std::collections::HashMap;
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let in_list = ids
            .iter()
            .map(|i| format!("'{}'", sql_lit(i)))
            .collect::<Vec<_>>()
            .join(",");
        let path = format!("$.\"{}\"", node_id);
        let sql = format!(
            "SELECT resource_id, json_extract(data, '{p}') AS g FROM tiles \
             WHERE resource_id IN ({ids}) AND json_extract(data, '{p}') IS NOT NULL",
            p = path,
            ids = in_list
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, g) = row?;
            out.insert(id, g);
        }
        Ok(out)
    }

    /// Read the `snapshot_id` of a REMOTE snapshot's `manifest.json` over httpfs on this
    /// reader's connection — used to record a slice's `derived_from`. `None` if
    /// unreadable/absent.
    pub fn read_remote_snapshot_id(&self, base_url: &str) -> Option<String> {
        let url = format!("{}/manifest.json", base_url.trim_end_matches('/'));
        self.conn
            .query_row(
                &format!("SELECT snapshot_id FROM read_json_auto('{}')", sql_lit(&url)),
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// A resource's `render_summary` from the grid edge (the card summary, which embeds
    /// its linked-resource summary). Best-effort: `None` if absent OR the store/column
    /// is missing (older-schema corpora keep render_summary on `grid`, not `resources`).
    /// Diagnostic.
    pub fn render_summary_of(&self, id: &str) -> Result<Option<String>, DuckError> {
        if !self.has_grid {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT render_summary FROM grid WHERE resource_id = ? LIMIT 1",
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
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
        // The tier field cut (L1): the UNION of every layer's
        // `manifest.tier.exclude_nodegroups`. Union is fail-closed — if ANY layer in
        // the stack scoped a field out, a query touching it is refused; the insurer
        // window composes its tiered head with only a non-sensitive basemap, so the
        // union IS the insurer head's exclusions.
        let mut excluded_nodegroups: std::collections::HashSet<String> =
            std::collections::HashSet::new();
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
                    if let Some(t) = &m.tier {
                        excluded_nodegroups.extend(t.exclude_nodegroups.iter().cloned());
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
                // `*` (not an explicit list) so the composition tolerates schema drift
                // between a layer's snapshot version and this reader — above all an
                // offline SLICE carved from an older-schema corpus. Composition keys on
                // column NAMES, downstream reads by name, so `*` is equivalent for a
                // current head and strictly more tolerant for an old one.
                shadow_set_union(&edge_layers, &["src_resource", "src_nodegroup"], "*")
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
                shadow_set_union(&rev_layers, &["src_resource", "src_nodegroup"], "*")
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
                shadow_set_union(&concept_layers, &["resource_id", "nodegroup_id"], "*")
            ))?;
        }
        let ordered_layers = layers_with(&conn, dirs, "ordered_*.parquet");
        let has_ordered = !ordered_layers.is_empty();
        if has_ordered {
            conn.execute_batch(&format!(
                "CREATE VIEW ordered_index AS {};",
                shadow_set_union(&ordered_layers, &["resource_id", "nodegroup_id"], "*")
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
                shadow_set_union(&grid_layers, &["resource_id"], "*")
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
                shadow_set_union(&resource_layers, &["resource_id"], "*")
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
            excluded_nodegroups,
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
    /// for the card — read from the per-resource ENTRY table (one row per resource,
    /// geo or not), so a NON-spatial corpus (a dictionary) gets its render_fields too.
    /// Tuple order `(descriptor, geo_descriptor, render_summary, render_fields)`.
    /// Empty without a resources view; `geo_descriptor` is `None` for a non-geo resource.
    /// (A spatial result reads these columns for free from the cell-pruned `GridBox`
    /// scan over the grid, which carries the SAME columns inline; this by-id method is
    /// the universal, geometry-free path for non-spatial renders + native/python callers.)
    #[allow(clippy::type_complexity)] // (descriptor, geo_descriptor, render_summary, render_fields) by id
    pub fn render_summaries(
        &self,
        uris: &[String],
    ) -> Result<
        HashMap<
            String,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >,
        DuckError,
    > {
        let mut out = HashMap::new();
        if uris.is_empty() || !self.has_resources {
            return Ok(out);
        }
        let placeholders = std::iter::repeat_n("?", uris.len())
            .collect::<Vec<_>>()
            .join(",");
        // Read from the ENTRY table (one row per resource, geo or not) — so a NON-spatial
        // corpus gets its render_fields too. The grid still carries the same columns inline for
        // the cell-pruned spatial scan; this by-id path is the universal, geometry-free one.
        let sql = format!(
            "SELECT resource_id, descriptor, geo_descriptor, render_summary, render_fields FROM resources \
             WHERE resource_id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(duckdb::params_from_iter(uris.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, descriptor, geo_descriptor, summary, fields) = row?;
            out.entry(id)
                .or_insert((descriptor, geo_descriptor, summary, fields));
        }
        Ok(out)
    }

    /// Entry-table rows for a set of resource ids: `(graph_id, legacyid, descriptor,
    /// description)` read from the per-resource `resources` view — the slug/model-scoping/
    /// name/GLOSS spine a consumer lists, counts, slug-backlinks, and FTS-indexes on.
    /// `description` is the resource's gloss (carried non-spatially, for a dictionary's
    /// definition search). Small (no hydration `data`), sorted by `resource_id` so an
    /// id-probe prunes. Empty for a head with no `resources` view.
    #[allow(clippy::type_complexity)] // (graph_id, legacyid, descriptor, description) by id
    pub fn resource_entries(
        &self,
        uris: &[String],
    ) -> Result<HashMap<String, (String, Option<String>, Option<String>, Option<String>)>, DuckError>
    {
        let mut out = HashMap::new();
        if uris.is_empty() || !self.has_resources {
            return Ok(out);
        }
        let placeholders = std::iter::repeat_n("?", uris.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT resource_id, graph_id, legacyid, descriptor, descriptor_description FROM resources \
             WHERE resource_id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(duckdb::params_from_iter(uris.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, graph_id, legacyid, descriptor, description) = row?;
            out.entry(id)
                .or_insert((graph_id, legacyid, descriptor, description));
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
            &self.excluded_nodegroups,
        )?)
    }
}

/// The outcome of an [`export_offline`]: where the per-layer slice landed and how big
/// each membership grade turned out — so the caller can report (and notice a capped
/// 2-hop) rather than guess.
pub struct ExportSummary {
    /// Per-layer slice dirs (base-first), ready to pass to [`DuckReader::open_layers`].
    pub layer_dirs: Vec<std::path::PathBuf>,
    /// The derived-provenance manifest written at the slice root (`<out>/manifest.json`):
    /// which snapshots it was carved from + the slice's own artifact hashes.
    pub manifest: std::path::PathBuf,
    /// Seed resources (the region).
    pub seed: usize,
    /// Full-grade 1-hop resources (queryable offline).
    pub hop1: usize,
    /// Reference-grade 2-hop resources actually written (after any cap).
    pub refs: usize,
    /// Whether the 2-hop was CAPPED (the raw reference set was larger than `hop2_ref_cap`).
    /// A `true` here is the hub-fan-out signal — the 2-hop reached a high-degree entity.
    pub refs_capped: bool,
}

/// A chosen layer for an offline export: a REMOTE snapshot base URL (R2/S3, read via
/// httpfs) or a LOCAL snapshot dir (an already-provisioned head — the ON-DEVICE path,
/// no httpfs). An export's layers must be all-remote OR all-local (not mixed).
pub enum LayerSpec {
    Http(String),
    Local(std::path::PathBuf),
}

/// Compose the chosen layers into one reader for closure resolution (cross-layer hops).
fn compose_specs(layers: &[LayerSpec]) -> Result<DuckReader, DuckError> {
    let all_http = layers.iter().all(|l| matches!(l, LayerSpec::Http(_)));
    let all_local = layers.iter().all(|l| matches!(l, LayerSpec::Local(_)));
    if all_http {
        let urls: Vec<&str> = layers
            .iter()
            .filter_map(|l| match l {
                LayerSpec::Http(u) => Some(u.as_str()),
                _ => None,
            })
            .collect();
        DuckReader::open_http_layers(&urls)
    } else if all_local {
        let dirs: Vec<&Path> = layers
            .iter()
            .filter_map(|l| match l {
                LayerSpec::Local(p) => Some(p.as_path()),
                _ => None,
            })
            .collect();
        DuckReader::open_layers(&dirs)
    } else {
        Err(DuckError::Compile(
            "export: layers must be all-remote or all-local (mixed not supported)".into(),
        ))
    }
}

/// The `snapshot_id` of a LOCAL snapshot dir (its `manifest.json`), for provenance.
fn local_snapshot_id(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("snapshot_id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Open ONE layer's raw reader for excision, plus its provenance label + `snapshot_id`.
fn open_spec(
    composed: &DuckReader,
    spec: &LayerSpec,
) -> Result<(DuckReader, String, String), DuckError> {
    match spec {
        LayerSpec::Http(u) => {
            let r = DuckReader::open_http(u)?;
            let snap = composed.read_remote_snapshot_id(u).unwrap_or_default();
            Ok((r, u.clone(), snap))
        }
        LayerSpec::Local(p) => {
            let r = DuckReader::open_layers(std::slice::from_ref(&p.as_path()))?;
            let snap = local_snapshot_id(p).unwrap_or_default();
            Ok((r, p.to_string_lossy().into_owned(), snap))
        }
    }
}

/// Export a self-contained OFFLINE slice from the chosen alizarin [`LayerSpec`]s — REMOTE
/// (R2 over httpfs) or LOCAL (already-provisioned heads, no httpfs, the ON-DEVICE path).
/// A region around a point (`bbox`), the queryable 1-hop, and OPTIONALLY the 2-hop as
/// linkable references. The closure is resolved over the COMPOSED layers (a hop crosses
/// layers), then each layer's RAW rows are excised into its own `layer-<i>/` sub-dir — so
/// [`DuckReader::open_layers`] re-composes the slice DYNAMICALLY offline rather than
/// flattening it (keeping the open/closed boundary). Writes a derived-provenance manifest.
///
/// `hop2_ref_cap` DEFAULT `0` (recommended): the 2-hop rides free as DISPLAY in the
/// full-grade 1-hop tiles' `__cache` (cache-ON heads) — no edge expansion, no fan-out.
/// `> 0` opts into reference-grade 2-hop ENTITIES via a `neighbours(hop1)` expansion,
/// which **fans out through a hub** — the cap bounds it and [`ExportSummary::refs_capped`]
/// reports when it bit.
pub fn export_offline_layers(
    layers: &[LayerSpec],
    bbox: [f64; 4],
    seed_limit: usize,
    hop2_ref_cap: usize,
    out_dir: &Path,
) -> Result<ExportSummary, DuckError> {
    if layers.is_empty() {
        return Err(DuckError::Compile("export_offline: no layers".into()));
    }
    // 1. compose the chosen layers and resolve the closure (cross-layer hops).
    let composed = compose_specs(layers)?;
    let seed = composed.ids_in_bbox(bbox, seed_limit)?;
    let hop1 = composed.neighbours(&seed)?;
    let mut closure: Vec<String> = seed.iter().chain(hop1.iter()).cloned().collect();
    closure.sort();
    closure.dedup();
    let (refs, refs_capped): (Vec<String>, bool) = if hop2_ref_cap > 0 {
        let full: std::collections::HashSet<&String> = closure.iter().collect();
        let mut r: Vec<String> = composed
            .neighbours(&hop1)?
            .into_iter()
            .filter(|id| !full.contains(id))
            .collect();
        r.sort();
        r.dedup();
        let capped = r.len() > hop2_ref_cap;
        r.truncate(hop2_ref_cap);
        (r, capped)
    } else {
        (Vec::new(), false)
    };
    // 2. excise each layer's RAW rows into its own sub-dir; collect hashes + provenance.
    let mut layer_dirs = Vec::new();
    let mut slice_layers = Vec::new();
    let mut derived_from = Vec::new();
    for (i, spec) in layers.iter().enumerate() {
        let (layer, label, snapshot_id) = open_spec(&composed, spec)?;
        let dir = out_dir.join(format!("layer-{i}"));
        let written = layer.excise(&closure, &refs, Some(bbox), &dir, "slice")?;
        let mut artifacts = Vec::new();
        for p in &written {
            let bytes = std::fs::read(p)
                .map_err(|e| DuckError::Compile(format!("hash {}: {e}", p.display())))?;
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            artifacts.push(ros_madair_format::ArtifactEntry::hashed(name, &bytes));
        }
        slice_layers.push(ros_madair_format::SliceLayer {
            dir: format!("layer-{i}"),
            artifacts,
        });
        derived_from.push(ros_madair_format::SourceSnapshot {
            url: label,
            snapshot_id,
        });
        layer_dirs.push(dir);
    }
    // 3. the derived-provenance manifest at the slice root (claim-only).
    let manifest = ros_madair_format::DerivedManifest {
        format_version: ros_madair_format::FORMAT_VERSION,
        kind: "offline-slice".to_string(),
        derived_from,
        region: Some(bbox),
        seed_limit: seed_limit as u64,
        hop2_ref_cap: hop2_ref_cap as u64,
        layers: slice_layers,
    };
    let manifest_path = out_dir.join("manifest.json");
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| DuckError::Compile(format!("serialize slice manifest: {e}")))?;
    std::fs::write(&manifest_path, json)
        .map_err(|e| DuckError::Compile(format!("write slice manifest: {e}")))?;
    Ok(ExportSummary {
        layer_dirs,
        manifest: manifest_path,
        seed: seed.len(),
        hop1: hop1.len(),
        refs: refs.len(),
        refs_capped,
    })
}

/// [`export_offline_layers`] over REMOTE (R2) layer base URLs — the desktop/browser path.
/// Export a self-contained OFFLINE slice of a remote, LAYERED corpus:
/// `export_offline(layers, bbox, seed_limit, hop2_ref_cap, out)` — a region around a
/// point (`bbox`), the queryable 1-hop, and OPTIONALLY the 2-hop as linkable references,
/// from ONLY the chosen `layer_urls`. The closure is resolved over the COMPOSED layers
/// (so a hop crosses layers — a seed building in the base reaches a keyholder in an
/// overlay), then each layer's RAW rows are excised into its own `layer-<i>/` sub-dir —
/// so [`DuckReader::open_layers`] re-composes the slice DYNAMICALLY offline rather than
/// flattening it (which would collapse the open/closed boundary).
///
/// **`hop2_ref_cap` — the 2-hop policy. The DEFAULT is `0`**, and it is the recommended
/// one: the 2-hop then comes for free as DISPLAY from the alizarin `__cache` /
/// `render_summary` already embedded in the full-grade 1-hop tiles — no edge expansion,
/// no fan-out. **This requires the corpus to have been emitted with `populate_caches`**;
/// a cache-light corpus (e.g. the current R2 `aonach763k`) carries no linked-resource
/// cache, so `0` yields no 2-hop — re-emit with caches to get it.
///
/// `hop2_ref_cap > 0` is the OPT-IN for a 2-hop you can independently tap-through /
/// map offline (reference-grade entities, not just cached display). It does a real
/// `neighbours(hop1)` expansion, which **fans out through a hub** (a shared Ward / org /
/// insurer the 1-hop links to) to tens of thousands — the cap bounds it and
/// [`ExportSummary::refs_capped`] reports when it bit (rather than silently truncating).
/// A high-degree hub should really be EXCLUDED from this expansion, not just capped.
pub fn export_offline(
    layer_urls: &[&str],
    bbox: [f64; 4],
    seed_limit: usize,
    hop2_ref_cap: usize,
    out_dir: &Path,
) -> Result<ExportSummary, DuckError> {
    let specs: Vec<LayerSpec> = layer_urls
        .iter()
        .map(|u| LayerSpec::Http(u.to_string()))
        .collect();
    export_offline_layers(&specs, bbox, seed_limit, hop2_ref_cap, out_dir)
}

/// A `read_parquet()` table-source over a LOCAL glob — the per-layer source the
/// composition helpers (`shadow_set_union`/`ranked_union`) interpolate as `FROM {src}`.
fn local_src(glob: &str) -> String {
    format!("read_parquet('{}')", sql_lit(glob))
}

/// A `read_parquet([...])` table-source over an explicit URL list (a REMOTE layer):
/// httpfs cannot glob plain HTTP, so the partition/model files are named explicitly.
/// `hive` recovers `nodegroup_id` for the partitioned tiles; `union_by_name` lets a
/// layer's several model files coexist in one source.
fn url_src(urls: &[String], hive: bool) -> String {
    let list = urls
        .iter()
        .map(|u| format!("'{}'", sql_lit(u)))
        .collect::<Vec<_>>()
        .join(", ");
    let opts = if hive {
        ", hive_partitioning=true, union_by_name=true"
    } else {
        ", union_by_name=true"
    };
    format!("read_parquet([{list}]{opts})")
}

/// Union per-layer sources tagged with their precedence rank `_layer` (base-first,
/// higher wins). Used for the DFS-interval concept catalog, where the winner is
/// picked by a `QUALIFY row_number() … ORDER BY _layer DESC` window.
fn ranked_union(layers: &[(usize, String)]) -> String {
    layers
        .iter()
        .map(|(i, src)| format!("SELECT *, {i} AS _layer FROM {src}"))
        .collect::<Vec<_>>()
        .join("\nUNION ALL BY NAME\n")
}

/// Open a layer dir as a `DuckReader` (tiles glob + optional concept catalog).
/// For a per-layer filename pattern, the `(rank, source)` of the layers that
/// actually contain matching files — so the composed view never `read_parquet`s
/// a zero-match glob (which errors at bind). Rank = the layer's index in `dirs`;
/// source is the `read_parquet('glob')` expression the composition interpolates.
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
            (n > 0).then_some((i, local_src(&glob)))
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
        .map(|(i, (_r, src))| {
            let higher = &sorted[i + 1..];
            if higher.is_empty() {
                format!("SELECT {cols} FROM {src} base")
            } else {
                let higher_keys = higher
                    .iter()
                    .map(|(_, g)| format!("SELECT {key_proj} FROM {g}"))
                    .collect::<Vec<_>>()
                    .join(" UNION ALL ");
                let on = keys
                    .iter()
                    .map(|k| format!("h.{k} = base.{k}"))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                format!(
                    "SELECT {cols} FROM {src} base \
                     WHERE NOT EXISTS (SELECT 1 FROM ({higher_keys}) h WHERE {on})"
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
/// hydrates. Opening a DuckDB per layer was the dominant per-entry cost (~75ms each,
/// and every layer is opened on every entry open); a resource's tiles are a query,
/// not a reason to re-open. `resource_tiles`/`concept_labels` take `&self`, so one
/// pooled reader serves every query.
///
/// The pool is **per-dir locked**: the GLOBAL lock is held only to get-or-insert the
/// per-dir slot (O(1), no I/O), then released; the slow `open_layer` and the query
/// closure `f` run under the slot's **own** lock. So queries on *different* layers run
/// fully concurrently, and a cold open (or a mass prewarm) blocks only its own dir —
/// not every other read process-wide. Same-dir calls still serialize, which is required
/// (`duckdb::Connection` is `Send` but `!Sync`).
fn with_layer<T>(
    dir: &Path,
    f: impl FnOnce(&DuckReader) -> Result<T, DuckError>,
) -> Result<T, DuckError> {
    use std::sync::{Arc, Mutex, OnceLock};
    type Slot = Arc<Mutex<Option<DuckReader>>>;
    static POOL: OnceLock<Mutex<HashMap<std::path::PathBuf, Slot>>> = OnceLock::new();
    // Global lock: get-or-insert this dir's slot, then drop the lock immediately.
    let slot: Slot = {
        let mut pool = POOL
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("layer reader pool poisoned");
        pool.entry(dir.to_path_buf()).or_default().clone()
    };
    // Per-dir lock: open (if cold) and query here. On open failure the slot stays
    // `None`, so the next call retries rather than caching a broken reader.
    let mut guard = slot.lock().expect("layer reader slot poisoned");
    if guard.is_none() {
        *guard = Some(open_layer(dir)?);
    }
    f(guard.as_ref().expect("just opened"))
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
            (0usize, local_src(&base.display().to_string())),
            (1usize, local_src(&overlay.display().to_string())),
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
