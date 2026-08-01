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
//!     (`link_targets`, a JSON array), so membership is precise (the head was
//!     coarse here).
//!   - `path` resolves a bare alias; dot-qualified paths are a later increment.
//!   - Promotion is "first indexed node of a class per tile" (from slice 1), so a
//!     nodegroup with two nodes of the same class is not yet distinguished.

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode};
use duckdb::Connection;
use ros_madair_query::{ConceptOp, Expr, Query};

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

impl DuckReader {
    /// Open a reader over a Parquet path/glob (a single `tiles_<slug>.parquet`,
    /// or a Hive-partitioned glob). Loads the spatial + json extensions so exact
    /// `ST_Intersects` and geometry parsing from the `data` blob work.
    pub fn open(parquet_glob: &str) -> Result<Self, DuckError> {
        let conn = Connection::open_in_memory()?;
        // json is needed to pull the geometry out of the tile `data` blob;
        // spatial for the exact intersection fine step. Best-effort load — a
        // non-spatial query still works if spatial is unavailable.
        let _ = conn.execute_batch("INSTALL json; LOAD json;");
        let _ = conn.execute_batch("INSTALL spatial; LOAD spatial;");
        conn.execute_batch(&format!(
            "CREATE VIEW tiles AS SELECT * FROM read_parquet('{}');",
            sql_lit(parquet_glob)
        ))?;
        Ok(Self { conn, has_catalog: false })
    }

    /// Open with a concept catalog attached (the `concept_catalog.parquet`
    /// projected from `RdmCache`), enabling `DescendantOrSelfOf` — a subtree
    /// range read (`dfs_enter BETWEEN pre_X AND submax_X`). DFS-ordered, so the
    /// zone-map prunes to just the subtree's row groups.
    pub fn open_with_catalog(parquet_glob: &str, catalog_glob: &str) -> Result<Self, DuckError> {
        let mut r = Self::open(parquet_glob)?;
        r.conn.execute_batch(&format!(
            "CREATE VIEW concepts AS SELECT * FROM read_parquet('{}');",
            sql_lit(catalog_glob)
        ))?;
        r.has_catalog = true;
        Ok(r)
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

    /// Resolve a query to the sorted set of matching resource ids.
    pub fn resolve_ids(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: &ExtensionTypeRegistry,
    ) -> Result<Vec<String>, DuckError> {
        let select = match &query.r#where {
            Some(expr) => compile_expr(expr, graph, registry, self.has_catalog)?,
            None => "SELECT DISTINCT resource_id FROM tiles".to_string(),
        };
        let sql = format!("SELECT resource_id FROM ({select}) t ORDER BY resource_id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for r in rows {
            ids.push(r?);
        }
        Ok(ids)
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
            // EXACT, unlike the head's coarse chunk_link_summary: `link_targets`
            // is the tile's actual target ids (a JSON array), so membership is
            // precise. `None` target = "has any link on this node".
            let cond = match target {
                Some(t) => format!(
                    "link_targets IS NOT NULL AND json_contains(link_targets, '\"{}\"')",
                    sql_lit(t)
                ),
                None => "link_targets IS NOT NULL AND link_targets <> '[]'".to_string(),
            };
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles WHERE nodegroup_id = '{}' AND {cond}",
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
}
