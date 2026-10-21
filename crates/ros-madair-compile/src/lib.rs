// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pure `Query` IR → DuckDB SQL lowering, with **no** libduckdb dependency.
//!
//! This is the ONE compiler: `ros-madair-duck` (native) links libduckdb and
//! *executes* the SQL this crate produces; a browser build (`wasm32`) runs the
//! identical lowering and hands the SQL to duckdb-wasm. Keeping it here — free of
//! any `Connection` — is what lets the same `compile_expr` serve both without the
//! SQL ever being hand-ported to TypeScript (the drift hazard it replaces).
//!
//! The backend *capabilities* a query depends on (concept catalog, edge table,
//! melted concept/ordered stores, spatial extension) are passed in as explicit
//! booleans, because the caller — not the compiler — knows which artifacts are
//! present: native reads them off the `DuckReader`'s registered views; the browser
//! passes which Parquet files it registered.

use std::collections::HashMap;

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode};
use ros_madair_query::{ConceptOp, Expr, Query};

/// The query could not be lowered to DuckDB SQL: a typed/schema error — an unknown
/// path alias, a datatype not indexed for the predicate, a coarse predicate under a
/// negation, a required store/edge table absent, etc. Carries the human-facing
/// message; `ros-madair-duck` wraps it into its own `DuckError::Compile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError(pub String);

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compile: {}", self.0)
    }
}
impl std::error::Error for CompileError {}

/// Join set-operation operands, each PARENTHESIZED. DuckDB ranks `INTERSECT`
/// above `UNION`/`EXCEPT`, so concatenating bare `SELECT`s lets a later term bind
/// to an earlier `EXCEPT`'s right side (a sibling constraint silently narrowing the
/// subtrahend instead of the result — an `All[…]` that grows when you add a `Not`).
/// Wrapping every operand makes the grouping exactly what the IR says.
fn set_join(parts: Vec<String>, op: &str) -> String {
    parts
        .iter()
        .map(|p| format!("({p})"))
        .collect::<Vec<_>>()
        .join(&format!("\n{op}\n"))
}

/// Compile a whole [`Query`] to a `SELECT DISTINCT resource_id`. This is the
/// top-level entry both consumers call: `ros-madair-duck` natively and the
/// browser wasm wrapper. An absent `where` means "every resource" (the bare
/// all-tiles select); otherwise lower the predicate via [`compile_expr`]. The
/// capability flags say which backend artifacts are present (see [`compile_expr`]).
#[allow(clippy::too_many_arguments)]
pub fn compile_query(
    query: &Query,
    graph: &StaticGraph,
    graphs: &[&StaticGraph],
    registry: &ExtensionTypeRegistry,
    has_catalog: bool,
    has_edges: bool,
    has_concept_index: bool,
    has_ordered: bool,
    has_grid: bool,
    spatial: bool,
) -> Result<String, CompileError> {
    match &query.r#where {
        Some(expr) => compile_expr(
            expr,
            graph,
            graphs,
            registry,
            has_catalog,
            has_edges,
            has_concept_index,
            has_ordered,
            has_grid,
            spatial,
        ),
        None => Ok("SELECT DISTINCT resource_id FROM tiles".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Expr → a SELECT of DISTINCT resource_id
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn compile_expr(
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
    has_grid: bool,
    spatial: bool,
) -> Result<String, CompileError> {
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
                        has_grid,
                        spatial,
                    )
                })
                .collect();
            Ok(set_join(parts?, "INTERSECT"))
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
                        has_grid,
                        spatial,
                    )
                })
                .collect();
            Ok(set_join(parts?, "UNION"))
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
                return Err(CompileError(format!(
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
                has_grid,
                spatial,
            )?;
            // Parenthesize the subtrahend: DuckDB gives INTERSECT higher precedence
            // than EXCEPT, so a bare `… EXCEPT a INTERSECT b` would regroup as
            // `(…) EXCEPT (a INTERSECT b)` and silently swallow a sibling term. The
            // whole Not is itself wrapped by the enclosing All/Any (see `set_join`).
            Ok(format!(
                "SELECT DISTINCT resource_id FROM tiles EXCEPT ({inner_sql})"
            ))
        }
        Expr::Concept { path, op, value } => {
            if !has_concept_index {
                return Err(CompileError(format!(
                    "concept filter on '{path}' needs the melted concept store — this \
                     reader has no concepts_*.parquet (this head carries no concept fields)"
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
                        return Err(CompileError(format!(
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
                return Err(CompileError(format!(
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
        // Spatial prune over the cell-sorted grid edge: the box → its overlapping
        // cells (the SHARED `cells_in_box` contract emit's `grid_cell` agrees with),
        // then a `cell IN (…)` semijoin that reads only those cells' row groups. A
        // cell-granular SUPERSET (see `first_coarse_path`), so no exact refine here.
        Expr::GridBox {
            path,
            min_lng,
            min_lat,
            max_lng,
            max_lat,
        } => {
            if !has_grid {
                return Err(CompileError(format!(
                    "grid-box on '{path}' needs the grid edge — this reader has no \
                     grid_*.parquet (this head carries no geo resources)"
                )));
            }
            // Validate the path names a geometry node (good errors), even though the
            // grid edge is per-resource (one centroid cell), not per-node.
            let (node, _) = resolve(graph, path)?;
            expect_class(
                node,
                registry,
                |c| matches!(c, IndexClass::SpatialBbox),
                path,
                "grid_box",
            )?;
            let cells = ros_madair_handlers::cells_in_box(*min_lng, *min_lat, *max_lng, *max_lat)
                .map_err(CompileError)?;
            let list = cells
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "SELECT DISTINCT resource_id FROM grid WHERE cell IN ({list})"
            ))
        }
        Expr::HasLink { path, target } => {
            if !has_edges {
                return Err(CompileError(format!(
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
        // Untyped reverse lookup: every resource that links to `target` through
        // ANY predicate ("what references this resource"). Over the REVERSE edge
        // index (`edges_rev`, sorted target-first) so this target-anchored lookup
        // zone-map prunes — the src-first forward `edges` could not. EXACT, so it
        // negates soundly (unlike Bbox). `redges_` co-emits with `edges_`, so the
        // edge-table availability flag gates it.
        Expr::Backlink { target } => {
            if !has_edges {
                return Err(CompileError(format!(
                    "backlink to '{target}' needs the reverse edge index — this reader \
                     has no redges_*.parquet / edges_rev view (open a snapshot emitted \
                     with the reverse edge slice)"
                )));
            }
            Ok(format!(
                "SELECT DISTINCT src_resource AS resource_id FROM edges_rev \
                 WHERE target_resource = '{}'",
                sql_lit(target)
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
                return Err(CompileError(format!(
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
                    CompileError(format!(
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
                has_grid,
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
                CompileError(
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
                has_grid,
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
) -> Result<(), CompileError> {
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
                    return Err(CompileError(format!(
                        "on_tile children must share one nodegroup (a tile is one \
                         nodegroup instance), but '{path}' is in '{ng}' while a sibling \
                         is in '{existing}'"
                    )))
                }
            }
            Ok(())
        }
        // A backlink is not node-scoped (it spans the whole resource graph), so it
        // cannot be one of an on_tile's single-nodegroup children.
        Expr::Backlink { .. } => Err(CompileError(
            "backlink cannot be an on_tile child — a tile is one nodegroup instance, \
             but a backlink is resource-graph-wide, not node-scoped"
                .into(),
        )),
        // A grid-box is a resource-level spatial prune (over the per-resource grid
        // edge), not a single-tile condition.
        Expr::GridBox { .. } => Err(CompileError(
            "grid_box cannot be an on_tile child — it is a resource-level spatial \
             prune, not node-scoped to one tile"
                .into(),
        )),
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
    has_grid: bool,
    spatial: bool,
) -> Result<String, CompileError> {
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
                        has_grid,
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
                        has_grid,
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
                return Err(CompileError(format!(
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
                    has_grid,
                    spatial
                )?
            ))
        }
        Expr::Concept { path, op, value } => {
            if !has_concept_index {
                return Err(CompileError(format!(
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
                        return Err(CompileError(format!(
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
                return Err(CompileError(format!(
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
                return Err(CompileError(format!(
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
                return Err(CompileError(format!(
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
                    CompileError(format!(
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
                has_grid,
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
        Expr::OnTile(_) => Err(CompileError(
            "nested on_tile is not supported; flatten the conditions into one on_tile".to_string(),
        )),
        // A backlink is resource-graph-wide, not a single-tile condition.
        Expr::Backlink { .. } => Err(CompileError(
            "backlink cannot be an on_tile child — it is not node-scoped".to_string(),
        )),
        // A grid-box is a resource-level spatial prune, not a single-tile condition.
        Expr::GridBox { .. } => Err(CompileError(
            "grid_box cannot be an on_tile child — it is a resource-level spatial prune"
                .to_string(),
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
        // A GridBox is ALWAYS a cell-granular superset (no exact in-substrate
        // refine — the spatial extension does not sharpen the grid), so it can
        // never be soundly negated, regardless of `spatial`.
        Expr::GridBox { path, .. } => Some(path.as_str()),
        Expr::All(children) | Expr::Any(children) | Expr::OnTile(children) => {
            children.iter().find_map(|c| first_coarse_path(c, spatial))
        }
        Expr::Not(inner) => first_coarse_path(inner, spatial),
        Expr::OnLink { r#where, .. } => first_coarse_path(r#where, spatial),
        Expr::Concept { .. }
        | Expr::Range { .. }
        | Expr::HasLink { .. }
        | Expr::Backlink { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a bare-alias path to its node and nodegroup id.
fn resolve<'g>(
    graph: &'g StaticGraph,
    path: &str,
) -> Result<(&'g StaticNode, String), CompileError> {
    let node = resolve_node(graph, path)?;
    let ng = node
        .nodegroup_id
        .clone()
        .ok_or_else(|| CompileError(format!("node '{path}' has no nodegroup")))?;
    Ok((node, ng))
}

/// Resolve a path to its node: a bare alias (global lookup), or a dot-qualified
/// path walked from the root by alias through the schema tree (following edges) —
/// e.g. `address.location`. Mirrors `ros-madair-query`'s `PathResolver` so both
/// compilers resolve paths identically; independent of `build_indices`.
fn resolve_node<'g>(graph: &'g StaticGraph, path: &str) -> Result<&'g StaticNode, CompileError> {
    let components: Vec<&str> = path.split('.').collect();
    if components.len() == 1 {
        return graph
            .find_node_by_alias(components[0])
            .ok_or_else(|| CompileError(format!("unknown path alias '{path}'")));
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
                return Err(CompileError(format!(
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
) -> Result<(), CompileError> {
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
        Err(CompileError(format!(
            "'{path}' (datatype '{}') is not indexed for a {predicate} predicate",
            node.datatype
        )))
    }
}

/// Escape a single-quoted SQL string literal (double the quotes). Node/ng ids are
/// UUIDs and paths are trusted, but concept values / file paths get defended.
pub fn sql_lit(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_join_parenthesizes_every_operand() {
        // Without the parens, `a INTERSECT b EXCEPT c INTERSECT d` regroups under
        // DuckDB precedence to `(a ∩ b) EXCEPT (c ∩ d)` — the bug that made an
        // All[…] grow when a Not was added. Each operand must be wrapped.
        let sql = set_join(
            vec!["a".into(), "b EXCEPT x".into(), "c".into()],
            "INTERSECT",
        );
        assert_eq!(sql, "(a)\nINTERSECT\n(b EXCEPT x)\nINTERSECT\n(c)");
    }
}
