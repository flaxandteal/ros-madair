// SPDX-License-Identifier: AGPL-3.0-or-later
//! M2.1–M2.4 (first slice): schema-validated query IR + SQLite compiler.
//!
//! A serde-typed intermediate representation for record-selection
//! queries, validated against a [`StaticGraph`] (aliases → node UUIDs,
//! datatype checks) and compiled to parameterized SQL against the head
//! database emitted by `ros-madair-emit` (see that crate's `lib.rs` for
//! the authoritative schema). This module produces SQL text + params
//! only — it never executes anything and has no SQLite dependency.
//!
//! ## Head schema targeted (from `ros-madair-emit`)
//!
//! ```text
//! spine_<slug>(rid INTEGER PK, term_id INTEGER, display_name TEXT)
//! dict(term_id INTEGER PK, term TEXT UNIQUE)      -- interned UUIDs/URIs
//! concept_tags(rid, node INTEGER, concept INTEGER)
//! vocab(concept INTEGER PK, dfs_enter INTEGER, dfs_leave INTEGER)
//! fragment_dir(rid, nodegroup INTEGER, chunk INTEGER, tile_count)
//! chunk_link_summary(chunk, node, min_target, max_target, n)
//! ```
//!
//! Note the emitted `vocab` table carries **no labels** — labels live in
//! `closure.json`. `Concept.value` is therefore the concept id (URI/UUID)
//! as interned in `dict`; label→id resolution via the closure artifact is
//! the M2.2 follow-up and happens before this compiler.
//!
//! Hierarchy queries (`descendant_or_self_of`) compile to
//! `concept BETWEEN dfs_enter AND dfs_leave` (P10 DFS intervals).
//!
//! Links have **no exact head table** — only the coarse per-chunk
//! `chunk_link_summary`. A [`Expr::HasLink`] predicate therefore compiles
//! to a chunk-granularity over-approximation (it can admit false
//! positives, never false negatives); the compiled statement is marked
//! `coarse: true` and callers must re-verify exact pairs on hydrated
//! tiles (the M2.4 residual channel). Because `NOT` over an
//! over-approximation under-approximates, wrapping a coarse predicate in
//! [`Expr::Not`] is rejected with a typed error rather than silently
//! dropping records.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode};

/// Default row cap for `select_ids` when the query carries no `limit`
/// (mirrors the emitted manifest's default `max_result_rows` budget).
pub const DEFAULT_SELECT_LIMIT: u32 = 1000;

/// The SQL table-name qualifier: none. Every statement this crate compiles runs
/// against ONE layer's schema, and resolves its tables — crucially its `dict` —
/// inside it. Nothing here spans layers: composition is `ros_madair_read`'s job,
/// and it composes RESULTS, never SQL. (A previous `compile_layered_count`
/// ATTACHed the layers and UNIONed per-layer arms. It is gone: its precedence
/// was per-RESOURCE, and the truth is per-NODE.)
const MAIN: &str = "";

// ---------------------------------------------------------------------------
// IR types (M2.1)
// ---------------------------------------------------------------------------

/// A record-selection query against one model's head tables.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Query {
    /// Model reference: the emitted slug (e.g. `"group"`), the graph
    /// UUID, or the graph's display name.
    pub model: String,
    /// Filter tree; `None` selects every record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#where: Option<Expr>,
    /// What to compute; one SQL statement is compiled per measure.
    pub measures: Vec<Measure>,
    /// Row cap for `select_ids` (defaults to [`DEFAULT_SELECT_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Boolean filter tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    /// Conjunction; `all: []` is vacuously true.
    All(Vec<Expr>),
    /// Disjunction; `any: []` is vacuously false.
    Any(Vec<Expr>),
    /// Negation. Rejected over coarse (`has_link`) subtrees — see module docs.
    Not(Box<Expr>),
    /// Concept-valued node predicate. `path` is a node alias (optionally
    /// dot-qualified, e.g. `"basic_info.name"`); `value` is the concept
    /// id (URI/UUID) as interned in `dict`.
    Concept {
        path: String,
        op: ConceptOp,
        value: String,
    },
    /// Resource-link predicate on a `resource-instance(-list)` node.
    /// `target` (a resource UUID) narrows to links plausibly pointing at
    /// that resource; `None` means "has any link on this node".
    /// Always compiles coarse (chunk-granularity) — see module docs.
    HasLink {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Ordered-scalar RANGE predicate on an `Ordered` node (date, A8). `lo`/`hi`
    /// are the QUANTIZED endpoints (inclusive) — the caller quantizes raw values
    /// into the head's key space with the SAME function the emitter used
    /// (`alizarin_core::quantize`), exactly as a Concept filter names an interned
    /// concept id, not a label. Matches on the exact `value_tags` index, so it is
    /// NOT coarse: `qvalue BETWEEN lo AND hi`.
    Range { path: String, lo: i64, hi: i64 },
    /// Spatial bounding-box OVERLAP predicate on a `SpatialBbox` node (geometry,
    /// A8.2). The four fields are the query box's corners in the same lng/lat
    /// space the emitter stored. Selects resources whose geometry's bbox overlaps
    /// the query box — a strict SUPERSET of true `sfIntersects`, so it always
    /// compiles `coarse: true`: the head answers the candidate set (no false
    /// negatives) and the client verifies exact intersection on hydrated tiles,
    /// exactly the residual contract `has_link` already uses.
    Bbox {
        path: String,
        min_lng: f64,
        min_lat: f64,
        max_lng: f64,
        max_lat: f64,
    },
}

/// Operators for [`Expr::Concept`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConceptOp {
    /// Exact concept match.
    Is,
    /// The tagged concept is the named concept or any descendant of it
    /// (DFS-interval `BETWEEN`).
    DescendantOrSelfOf,
}

/// What a compiled statement computes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    /// `SELECT COUNT(*)` over matching records.
    CountRecords,
    /// `SELECT` the matching resource UUIDs (via `dict`), `LIMIT`ed.
    SelectIds,
}

// ---------------------------------------------------------------------------
// Errors (M2.2: typed and repairable)
// ---------------------------------------------------------------------------

/// Validation/compilation errors. Serializable so agents/clients can
/// repair queries mechanically; every variant names what *would* work.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryError {
    /// `Query.model` does not name this graph.
    ModelMismatch {
        requested: String,
        /// The identifiers that would have matched: slug, graph id, name.
        expected: Vec<String>,
    },
    /// A path component matched no node alias. `siblings` names up to
    /// three valid aliases at the failing position (closest first).
    UnknownPath {
        path: String,
        /// The specific component that failed to resolve.
        component: String,
        siblings: Vec<String>,
    },
    /// The node exists but its datatype is not head-indexed: strings,
    /// numbers, dates, semantic nodes, geometry, … live only in the body
    /// chunks. Such predicates are post-filter only (M2.4 residual).
    NotHeadIndexed { path: String, datatype: String },
    /// The node exists and is head-indexed, but under a different
    /// predicate family (e.g. `concept` op on a resource-instance node).
    DatatypeMismatch {
        path: String,
        datatype: String,
        /// Datatype family the predicate requires.
        expected: String,
    },
    /// `Not` wrapping a coarse (`has_link`) subtree would silently drop
    /// records (over-approximation negated = under-approximation).
    NegatedCoarsePredicate { path: String },
    /// `measures` was empty — nothing to compile.
    EmptyMeasures,
    /// A layered operation was handed no layers. Over zero layers the answer is
    /// not 0, it is a caller bug.
    NoLayers,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::ModelMismatch {
                requested,
                expected,
            } => write!(
                f,
                "model '{}' does not match this graph (expected one of: {})",
                requested,
                expected.join(", ")
            ),
            QueryError::UnknownPath {
                path,
                component,
                siblings,
            } => {
                write!(f, "unknown alias '{}' in path '{}'", component, path)?;
                if !siblings.is_empty() {
                    write!(f, "; did you mean one of: {}?", siblings.join(", "))?;
                }
                Ok(())
            }
            QueryError::NotHeadIndexed { path, datatype } => write!(
                f,
                "path '{}' has datatype '{}': not head-indexed; post-filter only",
                path, datatype
            ),
            QueryError::DatatypeMismatch {
                path,
                datatype,
                expected,
            } => write!(
                f,
                "path '{}' has datatype '{}' but this predicate requires a {} datatype",
                path, datatype, expected
            ),
            QueryError::NegatedCoarsePredicate { path } => write!(
                f,
                "cannot negate coarse link predicate on '{}': \
                 chunk-level link summaries over-approximate, so NOT would \
                 drop matching records; evaluate the negation post-hydration",
                path
            ),
            QueryError::EmptyMeasures => write!(f, "query has no measures; nothing to compile"),
            QueryError::NoLayers => write!(
                f,
                "layered count over zero layers: pass the layer schemas, base first"
            ),
        }
    }
}

impl std::error::Error for QueryError {}

// ---------------------------------------------------------------------------
// Compiled output
// ---------------------------------------------------------------------------

/// A single bind parameter. Bind `params[i]` to placeholder `?{i+1}`.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Param {
    Text(String),
    Int(i64),
    /// A raw f64 corner for a spatial [`Expr::Bbox`] filter (A8.2). Kept distinct
    /// from `Int` so the binder passes it to SQLite as REAL, matching the
    /// `geo_bbox` column type — comparing a REAL column against an integer-bound
    /// value would defeat the covering index.
    Real(f64),
}

/// One parameterized SQL statement (per measure).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CompiledStatement {
    pub measure: Measure,
    /// SQL text with `?N` placeholders only — no interpolated values.
    pub sql: String,
    /// Positional parameters: `params[i]` binds `?{i+1}`.
    pub params: Vec<Param>,
    /// True when the statement contains a chunk-coarse predicate
    /// (`has_link`): results may include false positives and must be
    /// re-verified against hydrated tiles.
    pub coarse: bool,
}

// ---------------------------------------------------------------------------
// Compilation entry point (M2.2 validation + M2.4 SQL generation)
// ---------------------------------------------------------------------------

/// Validate `query` against `graph` and compile one parameterized SQLite
/// statement per measure. Pure: returns SQL text + params, executes nothing.
///
/// Thin wrapper over [`compile_with_registry`] with no extension registry:
/// only the core-owned datatypes (concept/domain-value/resource-instance)
/// resolve through the built-in fallback. Pass a registry via
/// [`compile_with_registry`] to make extension datatypes queryable.
pub fn compile(query: &Query, graph: &StaticGraph) -> Result<Vec<CompiledStatement>, QueryError> {
    compile_with_registry(query, graph, None)
}

/// Like [`compile`], but routes datatype checks through the given extension
/// registry. A registered [`ExtensionTypeHandler`] that claims a datatype as
/// [`IndexClass::ConceptHierarchical`] or [`IndexClass::Link`] makes that
/// datatype queryable (concept / has_link predicates) without core naming it —
/// the extension datatype knowledge stays entirely in its handler.
///
/// [`ExtensionTypeHandler`]: alizarin_core::extension_type_registry::ExtensionTypeHandler
pub fn compile_with_registry(
    query: &Query,
    graph: &StaticGraph,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<Vec<CompiledStatement>, QueryError> {
    let slug = check_model(&query.model, graph)?;
    if query.measures.is_empty() {
        return Err(QueryError::EmptyMeasures);
    }
    let spine_table = format!("spine_{}", slug.replace('-', "_"));
    let resolver = PathResolver::new(graph);

    let mut statements = Vec::with_capacity(query.measures.len());
    for measure in &query.measures {
        // Fresh parameter numbering per statement.
        let mut params = ParamBuilder::default();
        let mut coarse = false;

        // Fast path (P1/P2): a CountRecords whose whole predicate is a
        // single descendant-or-self concept filter is a precomputed
        // (node, ancestor) row in rollup_concept_counts — a two-key point
        // lookup instead of a correlated EXISTS over the entire spine.
        if *measure == Measure::CountRecords {
            if let Some(sql) =
                try_rollup_count(&query.r#where, &resolver, &mut params, MAIN, registry)?
            {
                statements.push(CompiledStatement {
                    measure: *measure,
                    sql,
                    params: params.params,
                    coarse: false,
                });
                continue;
            }
            // Fast path (covering idx_ct): a CountRecords whose whole
            // predicate is one exact `Is` concept filter is an index-only
            // COUNT(DISTINCT rid) over concept_tags — no spine scan.
            if let Some(sql) =
                try_exact_count(&query.r#where, &resolver, &mut params, MAIN, registry)?
            {
                statements.push(CompiledStatement {
                    measure: *measure,
                    sql,
                    params: params.params,
                    coarse: false,
                });
                continue;
            }
        }

        // Fast path (covering idx_ct): a SelectIds whose whole predicate is
        // one concept filter (Is or DescendantOrSelfOf) is driven from
        // concept_tags (rid-ordered index scan), not a full spine scan.
        if *measure == Measure::SelectIds {
            let limit = i64::from(query.limit.unwrap_or(DEFAULT_SELECT_LIMIT));
            if let Some(sql) = try_concept_select(
                &query.r#where,
                &resolver,
                &mut params,
                MAIN,
                &spine_table,
                limit,
                registry,
            )? {
                statements.push(CompiledStatement {
                    measure: *measure,
                    sql,
                    params: params.params,
                    coarse: false,
                });
                continue;
            }
        }

        let where_sql = match &query.r#where {
            Some(expr) => Some(compile_expr(
                expr,
                &resolver,
                &mut params,
                MAIN,
                &mut coarse,
                registry,
            )?),
            None => None,
        };
        let where_clause = where_sql.map(|w| format!(" WHERE {w}")).unwrap_or_default();
        let sql = match measure {
            Measure::CountRecords => {
                format!("SELECT COUNT(*) FROM {spine_table} s{where_clause}")
            }
            Measure::SelectIds => {
                let limit = params.int(i64::from(query.limit.unwrap_or(DEFAULT_SELECT_LIMIT)));
                format!(
                    "SELECT d.term FROM {spine_table} s \
                     JOIN dict d ON d.term_id = s.term_id{where_clause} \
                     ORDER BY s.rid LIMIT {limit}"
                )
            }
        };
        statements.push(CompiledStatement {
            measure: *measure,
            sql,
            params: params.params,
            coarse,
        });
    }
    Ok(statements)
}

// ---------------------------------------------------------------------------
// Layered composition (R1): one statement over several ATTACHed layers
// ---------------------------------------------------------------------------
/// Compile a **match probe**: "does resource `?N` match this filter, *in this
/// one layer*?" — `SELECT EXISTS(…)`, answering 0 or 1.
///
/// This is the correction primitive for the O(overlay-size) layered count: for
/// each resource an overlay redefines, ask the base whether *the base* thought
/// it matched, and subtract the hits. Probing is a point lookup (`dict.term` is
/// unique-indexed, spine is keyed by `term_id`), so the correction costs one
/// indexed probe per overridden resource — bounded by the overlay, not by the
/// corpus.
///
/// **Binding contract**: the returned `params` are the filter's parameters
/// only. The SQL carries ONE further placeholder, `?{params.len()+1}`, for the
/// resource UUID; bind `params ++ [uuid]` per probe and re-use the prepared
/// statement across probes.
/// The node id a **leaf** predicate reads — `None` for the boolean connectives.
///
/// Layer composition needs this and cannot get it any other way. Precedence
/// between layers is per **NODE**, because the tile merge is per key: an overlay
/// that restates a nodegroup but omits one of its nodes leaves the lower layer's
/// value for that node standing. So to evaluate a leaf in the composed view, the
/// composer must know *which node the leaf reads* in order to find the topmost
/// layer that defines it — and only this crate knows how a `path` resolves.
///
/// It is deliberately per-leaf rather than per-query: two leaves of one filter
/// routinely read nodes owned by *different* layers, which is exactly the case a
/// per-resource precedence rule gets wrong.
pub fn leaf_node_id(
    expr: &Expr,
    graph: &StaticGraph,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<Option<String>, QueryError> {
    let resolver = PathResolver::new(graph);
    Ok(match expr {
        Expr::Concept { path, .. } => Some(
            resolve_concept_node(path, &resolver, registry)?
                .nodeid
                .clone(),
        ),
        Expr::HasLink { path, .. } => {
            let node = resolver.resolve(path)?;
            if is_concept_class(node, registry) {
                return Err(QueryError::DatatypeMismatch {
                    path: path.clone(),
                    expected: "resource-instance".to_string(),
                    datatype: node.datatype.clone(),
                });
            }
            Some(node.nodeid.clone())
        }
        Expr::Range { path, .. } => Some(resolver.resolve(path)?.nodeid.clone()),
        Expr::Bbox { path, .. } => Some(resolver.resolve(path)?.nodeid.clone()),
        Expr::All(_) | Expr::Any(_) | Expr::Not(_) => None,
    })
}

pub fn compile_match_probe(
    query: &Query,
    graph: &StaticGraph,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<CompiledStatement, QueryError> {
    let slug = check_model(&query.model, graph)?;
    let spine_table = format!("spine_{}", slug.replace('-', "_"));
    let resolver = PathResolver::new(graph);

    let mut params = ParamBuilder::default();
    let mut coarse = false;
    // The filter compiles against the spine alias `s`, exactly as in `compile`.
    let filter = match &query.r#where {
        Some(expr) => Some(compile_expr(
            expr,
            &resolver,
            &mut params,
            MAIN,
            &mut coarse,
            registry,
        )?),
        None => None,
    };
    // Allocate the UUID placeholder AFTER the filter's params, and do not push
    // a value for it: the caller supplies one per probe.
    let uuid_p = format!("?{}", params.params.len() + 1);
    let filter_clause = filter.map(|f| format!(" AND {f}")).unwrap_or_default();
    Ok(CompiledStatement {
        measure: Measure::CountRecords,
        sql: format!(
            "SELECT EXISTS (SELECT 1 FROM {spine_table} s \
             JOIN dict d ON d.term_id = s.term_id \
             WHERE d.term = {uuid_p}{filter_clause})"
        ),
        params: params.params,
        coarse,
    })
}

// ---------------------------------------------------------------------------
// Model check
// ---------------------------------------------------------------------------

/// Slug derivation matching `ros-madair-emit` (lowercase, runs of
/// non-alphanumerics collapsed to single hyphens, trimmed).
fn emit_slug(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').to_string()
}

fn check_model(model: &str, graph: &StaticGraph) -> Result<String, QueryError> {
    let name = graph.display_name();
    let slug = emit_slug(&name);
    if model == slug || model == graph.graph_id() || model == name {
        return Ok(slug);
    }
    Err(QueryError::ModelMismatch {
        requested: model.to_string(),
        expected: vec![slug, graph.graph_id().to_string(), name],
    })
}

// ---------------------------------------------------------------------------
// Path resolution (M2.2: alias → node UUID, with repairable errors)
// ---------------------------------------------------------------------------

struct PathResolver<'g> {
    graph: &'g StaticGraph,
    /// domain node id -> child node ids (built from edges; independent of
    /// whether `build_indices` ran).
    children: HashMap<&'g str, Vec<&'g str>>,
    nodes_by_id: HashMap<&'g str, &'g StaticNode>,
}

impl<'g> PathResolver<'g> {
    fn new(graph: &'g StaticGraph) -> Self {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in graph.edges_slice() {
            children
                .entry(edge.domainnode_id.as_str())
                .or_default()
                .push(edge.rangenode_id.as_str());
        }
        let nodes_by_id = graph
            .nodes_slice()
            .iter()
            .map(|n| (n.nodeid.as_str(), n))
            .collect();
        Self {
            graph,
            children,
            nodes_by_id,
        }
    }

    /// Resolve a path (bare alias, or dot-separated aliases walked from
    /// the root) to its node.
    fn resolve(&self, path: &str) -> Result<&'g StaticNode, QueryError> {
        let components: Vec<&str> = path.split('.').collect();
        if components.len() == 1 {
            // Bare alias: global lookup; suggestions ranked over every alias.
            let alias = components[0];
            return self.graph.find_node_by_alias(alias).ok_or_else(|| {
                let all: Vec<&str> = self
                    .graph
                    .nodes_slice()
                    .iter()
                    .filter_map(|n| n.alias.as_deref())
                    .collect();
                QueryError::UnknownPath {
                    path: path.to_string(),
                    component: alias.to_string(),
                    siblings: closest(alias, &all),
                }
            });
        }

        // Dotted path: walk the schema tree from the root by alias.
        let mut current: &StaticNode = self.graph.get_root();
        for component in &components {
            let child_ids = self
                .children
                .get(current.nodeid.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let child_nodes: Vec<&StaticNode> = child_ids
                .iter()
                .filter_map(|id| self.nodes_by_id.get(id).copied())
                .collect();
            match child_nodes
                .iter()
                .find(|n| n.alias.as_deref() == Some(*component))
            {
                Some(node) => current = node,
                None => {
                    let sibling_aliases: Vec<&str> = child_nodes
                        .iter()
                        .filter_map(|n| n.alias.as_deref())
                        .collect();
                    return Err(QueryError::UnknownPath {
                        path: path.to_string(),
                        component: (*component).to_string(),
                        siblings: closest(component, &sibling_aliases),
                    });
                }
            }
        }
        Ok(current)
    }
}

/// Rank `candidates` by edit distance to `target` (ties alphabetical)
/// and return up to three.
fn closest(target: &str, candidates: &[&str]) -> Vec<String> {
    let mut ranked: Vec<(&str, usize)> = candidates
        .iter()
        .map(|c| (*c, levenshtein(target, c)))
        .collect();
    ranked.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    ranked.dedup_by(|a, b| a.0 == b.0);
    ranked
        .into_iter()
        .take(3)
        .map(|(c, _)| c.to_string())
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

// ---------------------------------------------------------------------------
// Datatype families (matching ros-madair-emit's head-indexing rules)
// ---------------------------------------------------------------------------

/// A node's config as a JSON object (the wire shape `datatype_index_spec`
/// and extension handlers expect), or `None` when the node has no config.
/// Mirrors the emitter's `head::node_config_value` — it lets a handler
/// (e.g. the CLM reference handler) resolve its own collection; the
/// compiler never inspects config keys directly.
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

/// Concept-like datatype test, routed through the datatype-capability seam.
/// Core names no datatype here: a registered handler owns its own class, and
/// the built-in fallback in `datatype_index_spec` covers the core-owned
/// concept/domain-value datatypes. The tile value is irrelevant to the
/// *class*, so a null value suffices.
fn is_concept_class(node: &StaticNode, registry: Option<&ExtensionTypeRegistry>) -> bool {
    matches!(
        datatype_index_spec(
            &node.datatype,
            &serde_json::Value::Null,
            node_config_value(node).as_ref(),
            registry,
        )
        .class,
        IndexClass::ConceptHierarchical { .. }
    )
}

/// Resource-link datatype test, routed through the datatype-capability seam.
/// Same rationale as [`is_concept_class`].
fn is_link_class(node: &StaticNode, registry: Option<&ExtensionTypeRegistry>) -> bool {
    matches!(
        datatype_index_spec(
            &node.datatype,
            &serde_json::Value::Null,
            node_config_value(node).as_ref(),
            registry,
        )
        .class,
        IndexClass::Link
    )
}

/// True iff the node is head-indexed as an ordered scalar (A8) — the class that
/// answers a `Range` predicate via `value_tags`.
fn is_ordered_class(node: &StaticNode, registry: Option<&ExtensionTypeRegistry>) -> bool {
    matches!(
        datatype_index_spec(
            &node.datatype,
            &serde_json::Value::Null,
            node_config_value(node).as_ref(),
            registry,
        )
        .class,
        IndexClass::Ordered
    )
}

/// True iff the node is head-indexed as a spatial bbox (A8.2) — the class that
/// answers a `Bbox` predicate via `geo_bbox`.
fn is_spatial_class(node: &StaticNode, registry: Option<&ExtensionTypeRegistry>) -> bool {
    matches!(
        datatype_index_spec(
            &node.datatype,
            &serde_json::Value::Null,
            node_config_value(node).as_ref(),
            registry,
        )
        .class,
        IndexClass::SpatialBbox
    )
}

// ---------------------------------------------------------------------------
// SQL generation (M2.4, parameterized only)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ParamBuilder {
    params: Vec<Param>,
    /// Dedup map: identical text params share one `?N` slot.
    by_text: HashMap<String, usize>,
}

impl ParamBuilder {
    /// Intern a text parameter, returning its `?N` placeholder.
    fn text(&mut self, value: &str) -> String {
        let idx = match self.by_text.get(value) {
            Some(&i) => i,
            None => {
                self.params.push(Param::Text(value.to_string()));
                let i = self.params.len();
                self.by_text.insert(value.to_string(), i);
                i
            }
        };
        format!("?{idx}")
    }

    /// Add an integer parameter, returning its `?N` placeholder.
    fn int(&mut self, value: i64) -> String {
        self.params.push(Param::Int(value));
        format!("?{}", self.params.len())
    }

    /// Add a real (f64) parameter, returning its `?N` placeholder (A8.2 bbox
    /// corners).
    fn real(&mut self, value: f64) -> String {
        self.params.push(Param::Real(value));
        format!("?{}", self.params.len())
    }
}

/// Fast path for `CountRecords` whose entire predicate is one
/// `DescendantOrSelfOf` concept filter. The answer is the precomputed
/// `rollup_concept_counts(node, ancestor, n)` row — `n` is already
/// `COUNT(DISTINCT rid)` over the concept's DFS interval, identical to the
/// general EXISTS-over-spine plan but a two-key point lookup. `COALESCE(…,0)`
/// covers the no-matching-tags case (no rollup row), matching EXISTS→0.
///
/// Returns `None` when the shape doesn't qualify (single bare descendant
/// predicate only — not `Is` (rollup counts the subtree, not the exact
/// concept), not inside `All`/`Any`/`Not`, not a link/no predicate); the
/// caller then falls back to the general plan. Propagates the same
/// datatype-validation errors the general Concept path would raise.
fn try_rollup_count(
    where_expr: &Option<Expr>,
    resolver: &PathResolver,
    params: &mut ParamBuilder,
    schema: &str,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<Option<String>, QueryError> {
    let Some(Expr::Concept {
        path,
        op: ConceptOp::DescendantOrSelfOf,
        value,
    }) = where_expr
    else {
        return Ok(None);
    };
    let node = resolver.resolve(path)?;
    if is_link_class(node, registry) {
        return Err(QueryError::DatatypeMismatch {
            path: path.clone(),
            datatype: node.datatype.clone(),
            expected: "concept-like (concept, concept-list, domain-value)".to_string(),
        });
    }
    if !is_concept_class(node, registry) {
        return Err(QueryError::NotHeadIndexed {
            path: path.clone(),
            datatype: node.datatype.clone(),
        });
    }
    let node_p = params.text(&node.nodeid);
    let value_p = params.text(value);
    Ok(Some(format!(
        "SELECT COALESCE((SELECT n FROM {schema}rollup_concept_counts \
         WHERE node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
         AND ancestor = (SELECT term_id FROM {schema}dict WHERE term = {value_p})), 0)"
    )))
}

/// Resolve a path expected to carry a concept-like datatype, raising the
/// same errors the general `Expr::Concept` path would (link mismatch,
/// not-head-indexed). Shared by the concept fast paths.
fn resolve_concept_node<'g>(
    path: &str,
    resolver: &PathResolver<'g>,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<&'g StaticNode, QueryError> {
    let node = resolver.resolve(path)?;
    if is_link_class(node, registry) {
        return Err(QueryError::DatatypeMismatch {
            path: path.to_string(),
            datatype: node.datatype.clone(),
            expected: "concept-like (concept, concept-list, domain-value)".to_string(),
        });
    }
    if !is_concept_class(node, registry) {
        return Err(QueryError::NotHeadIndexed {
            path: path.to_string(),
            datatype: node.datatype.clone(),
        });
    }
    Ok(node)
}

/// Fast path for `CountRecords` whose entire predicate is one exact `Is`
/// concept filter. With the covering `idx_ct(node, concept, rid)` this is an
/// index-only `COUNT(DISTINCT rid)` over `concept_tags` — identical answer to
/// the general correlated-EXISTS-over-spine plan (both count distinct spine
/// rids carrying the tag), but no spine scan. `DISTINCT` guards the
/// concept-list case where one rid can carry the same node/concept twice.
///
/// Returns `None` unless the whole `where` is a single bare `Is` predicate
/// (not `DescendantOrSelfOf`, not inside `All`/`Any`/`Not`, not a link);
/// the caller then falls back. Propagates the same datatype errors.
fn try_exact_count(
    where_expr: &Option<Expr>,
    resolver: &PathResolver,
    params: &mut ParamBuilder,
    schema: &str,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<Option<String>, QueryError> {
    let Some(Expr::Concept {
        path,
        op: ConceptOp::Is,
        value,
    }) = where_expr
    else {
        return Ok(None);
    };
    let node = resolve_concept_node(path, resolver, registry)?;
    let node_p = params.text(&node.nodeid);
    let value_p = params.text(value);
    Ok(Some(format!(
        "SELECT COUNT(DISTINCT rid) FROM {schema}concept_tags \
         WHERE node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
         AND concept = (SELECT term_id FROM {schema}dict WHERE term = {value_p})"
    )))
}

/// Fast path for `SelectIds` whose entire predicate is one concept filter.
/// Drives from `concept_tags` (covering `idx_ct`) instead of scanning the
/// whole spine and probing EXISTS per row.
///
/// - `Is`: join `concept_tags` (rid-ordered) → spine → dict, `LIMIT`. The
///   node/concept lookup rides the covering index; `ct.rid` is already in
///   rid order so `ORDER BY ct.rid LIMIT` reads only the first N tags.
/// - `DescendantOrSelfOf`: a rid can carry several descendant concepts, so
///   collapse to `DISTINCT rid` (rid-ordered, limited) *before* the spine
///   join — otherwise the same record could fill the limit multiple times.
///
/// Both return the identical record set to the general spine plan (first N
/// matching rids in rid order). Returns `None` unless the whole `where` is a
/// single bare concept predicate; the caller then falls back.
fn try_concept_select(
    where_expr: &Option<Expr>,
    resolver: &PathResolver,
    params: &mut ParamBuilder,
    schema: &str,
    spine_table: &str,
    limit: i64,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<Option<String>, QueryError> {
    let Some(Expr::Concept { path, op, value }) = where_expr else {
        return Ok(None);
    };
    let node = resolve_concept_node(path, resolver, registry)?;
    let node_p = params.text(&node.nodeid);
    let value_p = params.text(value);
    let limit_p = params.int(limit);
    let sql = match op {
        ConceptOp::Is => format!(
            "SELECT d.term FROM {schema}concept_tags ct \
             JOIN {schema}{spine_table} s ON s.rid = ct.rid \
             JOIN {schema}dict d ON d.term_id = s.term_id \
             WHERE ct.node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
             AND ct.concept = (SELECT term_id FROM {schema}dict WHERE term = {value_p}) \
             ORDER BY ct.rid LIMIT {limit_p}"
        ),
        ConceptOp::DescendantOrSelfOf => format!(
            "SELECT d.term FROM \
             (SELECT DISTINCT rid FROM {schema}concept_tags \
              WHERE node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
              AND concept BETWEEN \
              (SELECT vv.dfs_enter FROM {schema}vocab vv \
               JOIN {schema}dict dd ON dd.term_id = vv.concept WHERE dd.term = {value_p}) \
              AND \
              (SELECT vv.dfs_leave FROM {schema}vocab vv \
               JOIN {schema}dict dd ON dd.term_id = vv.concept WHERE dd.term = {value_p}) \
              ORDER BY rid LIMIT {limit_p}) x \
             JOIN {schema}{spine_table} s ON s.rid = x.rid \
             JOIN {schema}dict d ON d.term_id = s.term_id"
        ),
    };
    Ok(Some(sql))
}

fn compile_expr(
    expr: &Expr,
    resolver: &PathResolver,
    params: &mut ParamBuilder,
    schema: &str,
    coarse: &mut bool,
    registry: Option<&ExtensionTypeRegistry>,
) -> Result<String, QueryError> {
    match expr {
        Expr::All(exprs) => {
            if exprs.is_empty() {
                return Ok("1=1".to_string());
            }
            let parts = exprs
                .iter()
                .map(|e| compile_expr(e, resolver, params, schema, coarse, registry))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", parts.join(" AND ")))
        }
        Expr::Any(exprs) => {
            if exprs.is_empty() {
                return Ok("0=1".to_string());
            }
            let parts = exprs
                .iter()
                .map(|e| compile_expr(e, resolver, params, schema, coarse, registry))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("({})", parts.join(" OR ")))
        }
        Expr::Not(inner) => {
            if let Some(path) = first_coarse_path(inner) {
                return Err(QueryError::NegatedCoarsePredicate {
                    path: path.to_string(),
                });
            }
            let inner_sql = compile_expr(inner, resolver, params, schema, coarse, registry)?;
            Ok(format!("NOT ({inner_sql})"))
        }
        Expr::Concept { path, op, value } => {
            let node = resolver.resolve(path)?;
            if is_link_class(node, registry) {
                return Err(QueryError::DatatypeMismatch {
                    path: path.clone(),
                    datatype: node.datatype.clone(),
                    expected: "concept-like (concept, concept-list, domain-value)".to_string(),
                });
            }
            if !is_concept_class(node, registry) {
                return Err(QueryError::NotHeadIndexed {
                    path: path.clone(),
                    datatype: node.datatype.clone(),
                });
            }
            let node_p = params.text(&node.nodeid);
            let value_p = params.text(value);
            match op {
                ConceptOp::Is => Ok(format!(
                    "EXISTS (SELECT 1 FROM {schema}concept_tags ct \
                     WHERE ct.rid = s.rid \
                     AND ct.node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
                     AND ct.concept = (SELECT term_id FROM {schema}dict WHERE term = {value_p}))"
                )),
                // P10 DFS intervals: descendant-or-self is a single BETWEEN
                // against the exact concept_tags rows — no expanded table.
                ConceptOp::DescendantOrSelfOf => Ok(format!(
                    "EXISTS (SELECT 1 FROM {schema}concept_tags ct \
                     WHERE ct.rid = s.rid \
                     AND ct.node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
                     AND ct.concept BETWEEN \
                     (SELECT vv.dfs_enter FROM {schema}vocab vv \
                      JOIN {schema}dict dd ON dd.term_id = vv.concept \
                      WHERE dd.term = {value_p}) \
                     AND \
                     (SELECT vv.dfs_leave FROM {schema}vocab vv \
                      JOIN {schema}dict dd ON dd.term_id = vv.concept \
                      WHERE dd.term = {value_p}))"
                )),
            }
        }
        Expr::HasLink { path, target } => {
            let node = resolver.resolve(path)?;
            if is_concept_class(node, registry) {
                return Err(QueryError::DatatypeMismatch {
                    path: path.clone(),
                    datatype: node.datatype.clone(),
                    expected: "resource-instance-like (resource-instance, \
                               resource-instance-list)"
                        .to_string(),
                });
            }
            if !is_link_class(node, registry) {
                return Err(QueryError::NotHeadIndexed {
                    path: path.clone(),
                    datatype: node.datatype.clone(),
                });
            }
            // Coarse: links have no exact head table (P1 — summary
            // granularity is the chunk). We prune via the resource's own
            // chunks (fragment_dir) intersected with chunks whose link
            // summary covers this node (and target range, if given).
            *coarse = true;
            let node_p = params.text(&node.nodeid);
            let target_clause = match target {
                Some(t) => {
                    let target_p = params.text(t);
                    format!(
                        " AND (SELECT term_id FROM {schema}dict WHERE term = {target_p}) \
                         BETWEEN cls.min_target AND cls.max_target"
                    )
                }
                None => String::new(),
            };
            Ok(format!(
                "EXISTS (SELECT 1 FROM {schema}fragment_dir fd \
                 JOIN {schema}chunk_link_summary cls ON cls.chunk = fd.chunk \
                 WHERE fd.rid = s.rid \
                 AND cls.node = (SELECT term_id FROM {schema}dict WHERE term = {node_p})\
                 {target_clause})"
            ))
        }
        Expr::Range { path, lo, hi } => {
            let node = resolver.resolve(path)?;
            if !is_ordered_class(node, registry) {
                return Err(QueryError::NotHeadIndexed {
                    path: path.clone(),
                    datatype: node.datatype.clone(),
                });
            }
            // EXACT, not coarse: the ordered index has a per-value table, so a
            // range is an index scan over value_tags (mirrors concept `Is`), no
            // chunk-summary over-approximation and no residual re-check.
            let node_p = params.text(&node.nodeid);
            let lo_p = params.int(*lo);
            let hi_p = params.int(*hi);
            Ok(format!(
                "EXISTS (SELECT 1 FROM {schema}value_tags vt \
                 WHERE vt.rid = s.rid \
                 AND vt.node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
                 AND vt.qvalue BETWEEN {lo_p} AND {hi_p})"
            ))
        }
        Expr::Bbox {
            path,
            min_lng,
            min_lat,
            max_lng,
            max_lat,
        } => {
            let node = resolver.resolve(path)?;
            if !is_spatial_class(node, registry) {
                return Err(QueryError::NotHeadIndexed {
                    path: path.clone(),
                    datatype: node.datatype.clone(),
                });
            }
            // Coarse: bbox-overlap is a strict SUPERSET of true intersection, so
            // this admits false positives (a geometry whose box overlaps the query
            // box but whose actual shape does not), never false negatives. Mark
            // coarse and let the caller verify exact intersection on hydrated
            // tiles — the same residual contract has_link uses.
            *coarse = true;
            let node_p = params.text(&node.nodeid);
            // Two boxes overlap iff they overlap on BOTH axes: the stored box's
            // max corner is not left/below the query's min, and its min corner is
            // not right/above the query's max.
            let q_min_lng = params.real(*min_lng);
            let q_min_lat = params.real(*min_lat);
            let q_max_lng = params.real(*max_lng);
            let q_max_lat = params.real(*max_lat);
            Ok(format!(
                "EXISTS (SELECT 1 FROM {schema}geo_bbox g \
                 WHERE g.rid = s.rid \
                 AND g.node = (SELECT term_id FROM {schema}dict WHERE term = {node_p}) \
                 AND g.max_lng >= {q_min_lng} AND g.min_lng <= {q_max_lng} \
                 AND g.max_lat >= {q_min_lat} AND g.min_lat <= {q_max_lat})"
            ))
        }
    }
}

/// Find the path of the first coarse (`has_link`) predicate in a subtree.
fn first_coarse_path(expr: &Expr) -> Option<&str> {
    match expr {
        // has_link (chunk summary) and bbox (superset of intersection) both
        // over-approximate, so negating either would drop matching records.
        Expr::HasLink { path, .. } | Expr::Bbox { path, .. } => Some(path),
        Expr::All(exprs) | Expr::Any(exprs) => exprs.iter().find_map(first_coarse_path),
        Expr::Not(inner) => first_coarse_path(inner),
        // Concept and Range are EXACT (concept_tags / value_tags) — not coarse,
        // so `Not` over them is safe.
        Expr::Concept { .. } | Expr::Range { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Same load pattern as json_conversion.rs tests.
    fn load_group_graph() -> StaticGraph {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let test_file = crate_root.join("tests/data/models/Group.json");
        let json_str = fs::read_to_string(&test_file).expect("Failed to read Group.json");
        let json: serde_json::Value =
            serde_json::from_str(&json_str).expect("Failed to parse Group.json");
        let graph_data = json["graph"][0].clone();
        let mut graph: StaticGraph =
            serde_json::from_value(graph_data).expect("Failed to deserialize StaticGraph");
        graph.build_indices();
        graph
    }

    const CONCEPT_ID: &str = "d8c60bf4-e786-11e6-905a-b756ec83dad5";

    fn concept_query(op: ConceptOp, measures: Vec<Measure>) -> Query {
        Query {
            model: "group".to_string(),
            r#where: Some(Expr::Concept {
                path: "group_type".to_string(),
                op,
                value: CONCEPT_ID.to_string(),
            }),
            measures,
            limit: None,
        }
    }

    #[test]
    fn valid_concept_query_compiles_to_expected_sql_shape() {
        // Is + CountRecords takes the exact-count fast path (covering
        // idx_ct): an index-only COUNT(DISTINCT rid) over concept_tags, no
        // spine scan. Identical answer to the old correlated-EXISTS plan.
        let graph = load_group_graph();
        let stmts = compile(
            &concept_query(ConceptOp::Is, vec![Measure::CountRecords]),
            &graph,
        )
        .expect("should compile");
        assert_eq!(stmts.len(), 1);
        let stmt = &stmts[0];
        assert_eq!(stmt.measure, Measure::CountRecords);
        assert!(!stmt.coarse);
        assert!(stmt
            .sql
            .starts_with("SELECT COUNT(DISTINCT rid) FROM concept_tags WHERE "));
        assert!(!stmt.sql.contains("spine_group"));
        assert!(!stmt.sql.contains("EXISTS"));
        assert!(stmt
            .sql
            .contains("node = (SELECT term_id FROM dict WHERE term = ?1)"));
        assert!(stmt
            .sql
            .contains("concept = (SELECT term_id FROM dict WHERE term = ?2)"));
        // Params: node UUID resolved from the alias, then the concept id.
        assert_eq!(
            stmt.params,
            vec![
                // group_type's node UUID from Group.json
                Param::Text("97f2c366-b323-11e9-98a8-a4d18cec433a".to_string()),
                Param::Text(CONCEPT_ID.to_string()),
            ]
        );
        // Never interpolate values: no quotes or literal ids in the SQL.
        assert!(!stmt.sql.contains('\''));
        assert!(!stmt.sql.contains(CONCEPT_ID));
    }

    #[test]
    fn general_exact_is_exists_shape_via_compound() {
        // The general correlated-EXISTS plan for exact `Is` is still emitted
        // when the predicate is not the sole where (here wrapped in All), so
        // the fast path does not swallow the general shape.
        let graph = load_group_graph();
        let inner = Expr::Concept {
            path: "group_type".to_string(),
            op: ConceptOp::Is,
            value: CONCEPT_ID.to_string(),
        };
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::All(vec![inner])),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        let sql = &compile(&query, &graph).expect("should compile")[0].sql;
        assert!(sql.starts_with("SELECT COUNT(*) FROM spine_group s WHERE "));
        assert!(sql.contains("EXISTS (SELECT 1 FROM concept_tags ct"));
        assert!(sql.contains("ct.rid = s.rid"));
        assert!(sql.contains("ct.concept = (SELECT term_id FROM dict WHERE term = ?2)"));
    }

    #[test]
    fn descendant_select_drives_from_distinct_rid_over_concept_tags() {
        // DescendantOrSelfOf + SelectIds takes the concept-driven fast path:
        // a DISTINCT-rid, rid-ordered, limited scan of concept_tags over the
        // DFS interval, joined to the spine after the limit (a rid can carry
        // several descendant concepts, so DISTINCT-before-limit is required).
        let graph = load_group_graph();
        let stmts = compile(
            &concept_query(ConceptOp::DescendantOrSelfOf, vec![Measure::SelectIds]),
            &graph,
        )
        .expect("should compile");
        let sql = &stmts[0].sql;
        assert!(sql.starts_with("SELECT d.term FROM (SELECT DISTINCT rid FROM concept_tags"));
        assert!(sql.contains("concept BETWEEN"));
        assert!(sql.contains("vv.dfs_enter"));
        assert!(sql.contains("vv.dfs_leave"));
        assert!(sql.contains("ORDER BY rid LIMIT ?3"));
        assert!(sql.contains(") x JOIN spine_group s ON s.rid = x.rid"));
        // The concept param is deduplicated: bound once, referenced twice.
        assert_eq!(sql.matches("?2").count(), 2);
    }

    #[test]
    fn descendant_between_shape_still_reachable_via_compound() {
        // The general compile_expr BETWEEN plan is still emitted when the
        // descendant predicate is not the sole where (here wrapped in All).
        let graph = load_group_graph();
        let inner = match concept_query(ConceptOp::DescendantOrSelfOf, vec![]).r#where {
            Some(e) => e,
            None => unreachable!(),
        };
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::All(vec![inner])),
            measures: vec![Measure::SelectIds],
            limit: None,
        };
        let sql = &compile(&query, &graph).expect("should compile")[0].sql;
        assert!(sql.starts_with("SELECT d.term FROM spine_group s JOIN dict"));
        assert!(sql.contains("ct.concept BETWEEN"));
        assert!(sql.contains("ORDER BY s.rid LIMIT"));
    }

    #[test]
    fn count_records_over_descendant_uses_rollup() {
        let graph = load_group_graph();
        let stmts = compile(
            &concept_query(ConceptOp::DescendantOrSelfOf, vec![Measure::CountRecords]),
            &graph,
        )
        .expect("should compile");
        let sql = &stmts[0].sql;
        // Point lookup on the precomputed rollup, not a spine scan.
        assert!(sql.contains("rollup_concept_counts"));
        assert!(sql.contains("COALESCE"));
        assert!(!sql.contains("COUNT(*) FROM"));
        assert!(!sql.contains("ct.concept BETWEEN"));
    }

    #[test]
    fn count_records_over_exact_is_uses_covering_count() {
        // `Is` is exact-concept: the rollup (subtree) fast path must NOT
        // fire, but the exact-count fast path does — COUNT(DISTINCT rid)
        // over the covering idx_ct, not a rollup and not a spine scan.
        let graph = load_group_graph();
        let stmts = compile(
            &concept_query(ConceptOp::Is, vec![Measure::CountRecords]),
            &graph,
        )
        .expect("should compile");
        let sql = &stmts[0].sql;
        assert!(sql.contains("COUNT(DISTINCT rid) FROM concept_tags"));
        assert!(!sql.contains("rollup_concept_counts"));
        assert!(!sql.contains("EXISTS"));
        assert!(!sql.contains("COUNT(*) FROM"));
    }

    #[test]
    fn count_records_over_compound_where_stays_general() {
        // A descendant predicate wrapped in All() is no longer the sole
        // predicate — rollup can't answer it; general plan must be used.
        let graph = load_group_graph();
        let inner = match concept_query(ConceptOp::DescendantOrSelfOf, vec![]).r#where {
            Some(e) => e,
            None => unreachable!(),
        };
        let query = Query {
            model: graph.graph_id().to_string(),
            r#where: Some(Expr::All(vec![inner])),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        let stmts = compile(&query, &graph).expect("should compile");
        let sql = &stmts[0].sql;
        assert!(sql.contains("COUNT(*) FROM"));
        assert!(!sql.contains("rollup_concept_counts"));
    }

    #[test]
    fn select_ids_is_drives_from_concept_tags_and_parameterizes_limit() {
        // Is + SelectIds takes the concept-driven fast path: join from
        // concept_tags (rid-ordered on the covering idx_ct) to the spine,
        // not a full spine scan with per-row EXISTS.
        let graph = load_group_graph();
        let mut query = concept_query(ConceptOp::Is, vec![Measure::SelectIds]);
        query.limit = Some(25);
        let stmts = compile(&query, &graph).expect("should compile");
        let stmt = &stmts[0];
        assert!(stmt.sql.starts_with(
            "SELECT d.term FROM concept_tags ct \
             JOIN spine_group s ON s.rid = ct.rid \
             JOIN dict d ON d.term_id = s.term_id WHERE "
        ));
        assert!(stmt
            .sql
            .contains("ct.node = (SELECT term_id FROM dict WHERE term = ?1)"));
        assert!(stmt
            .sql
            .contains("ct.concept = (SELECT term_id FROM dict WHERE term = ?2)"));
        assert!(stmt.sql.ends_with("ORDER BY ct.rid LIMIT ?3"));
        assert!(!stmt.sql.contains("EXISTS"));
        assert_eq!(stmt.params[2], Param::Int(25));
    }

    #[test]
    fn select_ids_compound_where_stays_on_spine() {
        // A concept predicate wrapped in All() is no longer the sole
        // predicate — the concept-driven fast path must NOT fire; the
        // general spine-scan + per-row EXISTS plan stands.
        let graph = load_group_graph();
        let inner = Expr::Concept {
            path: "group_type".to_string(),
            op: ConceptOp::Is,
            value: CONCEPT_ID.to_string(),
        };
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::All(vec![inner])),
            measures: vec![Measure::SelectIds],
            limit: None,
        };
        let sql = &compile(&query, &graph).expect("should compile")[0].sql;
        assert!(sql.starts_with("SELECT d.term FROM spine_group s JOIN dict"));
        assert!(sql.contains("EXISTS (SELECT 1 FROM concept_tags ct"));
        assert!(sql.contains("ORDER BY s.rid LIMIT"));
    }

    #[test]
    fn one_statement_per_measure_with_independent_params() {
        let graph = load_group_graph();
        let stmts = compile(
            &concept_query(
                ConceptOp::Is,
                vec![Measure::CountRecords, Measure::SelectIds],
            ),
            &graph,
        )
        .expect("should compile");
        assert_eq!(stmts.len(), 2);
        // Second statement restarts parameter numbering (default limit).
        assert_eq!(stmts[1].params.len(), 3);
        assert_eq!(
            stmts[1].params[2],
            Param::Int(i64::from(DEFAULT_SELECT_LIMIT))
        );
    }

    #[test]
    fn unknown_alias_names_up_to_three_siblings() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::Concept {
                path: "group_typ".to_string(),
                op: ConceptOp::Is,
                value: CONCEPT_ID.to_string(),
            }),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        let err = compile(&query, &graph).unwrap_err();
        match &err {
            QueryError::UnknownPath {
                path,
                component,
                siblings,
            } => {
                assert_eq!(path, "group_typ");
                assert_eq!(component, "group_typ");
                assert!(siblings.len() <= 3 && !siblings.is_empty());
                assert_eq!(
                    siblings[0], "group_type",
                    "closest alias first: {siblings:?}"
                );
            }
            other => panic!("expected UnknownPath, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("group_type"), "Display names siblings: {msg}");
    }

    #[test]
    fn unknown_dotted_component_names_tree_siblings() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::Concept {
                path: "basic_info.nam".to_string(),
                op: ConceptOp::Is,
                value: CONCEPT_ID.to_string(),
            }),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        match compile(&query, &graph).unwrap_err() {
            QueryError::UnknownPath {
                component,
                siblings,
                ..
            } => {
                assert_eq!(component, "nam");
                // Siblings are basic_info's children (name, image, source).
                assert_eq!(siblings[0], "name");
                assert!(
                    siblings
                        .iter()
                        .all(|s| ["name", "image", "source"].contains(&s.as_str())),
                    "siblings restricted to the failing level: {siblings:?}"
                );
            }
            other => panic!("expected UnknownPath, got {other:?}"),
        }
    }

    #[test]
    fn string_path_gets_typed_not_head_indexed_error() {
        let graph = load_group_graph();
        for path in ["name", "basic_info.name"] {
            let query = Query {
                model: "group".to_string(),
                r#where: Some(Expr::Concept {
                    path: path.to_string(),
                    op: ConceptOp::Is,
                    value: CONCEPT_ID.to_string(),
                }),
                measures: vec![Measure::CountRecords],
                limit: None,
            };
            let err = compile(&query, &graph).unwrap_err();
            assert_eq!(
                err,
                QueryError::NotHeadIndexed {
                    path: path.to_string(),
                    datatype: "string".to_string(),
                }
            );
            let msg = err.to_string();
            assert!(msg.contains("not head-indexed; post-filter only"));
            assert!(msg.contains("string"), "error names the datatype: {msg}");
        }
    }

    #[test]
    fn concept_op_on_link_node_is_datatype_mismatch() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::Concept {
                path: "members".to_string(),
                op: ConceptOp::Is,
                value: CONCEPT_ID.to_string(),
            }),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        match compile(&query, &graph).unwrap_err() {
            QueryError::DatatypeMismatch { datatype, .. } => {
                assert_eq!(datatype, "resource-instance-list");
            }
            other => panic!("expected DatatypeMismatch, got {other:?}"),
        }
    }

    #[test]
    fn has_link_compiles_coarse_against_chunk_link_summary() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::HasLink {
                path: "members".to_string(),
                target: Some("11111111-2222-3333-4444-555555555555".to_string()),
            }),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        let stmts = compile(&query, &graph).expect("should compile");
        let stmt = &stmts[0];
        assert!(stmt.coarse, "link predicates are chunk-coarse");
        assert!(stmt
            .sql
            .contains("JOIN chunk_link_summary cls ON cls.chunk = fd.chunk"));
        assert!(stmt.sql.contains("fd.rid = s.rid"));
        assert!(stmt
            .sql
            .contains("BETWEEN cls.min_target AND cls.max_target"));
        // has_link on a concept node is a mismatch the other way.
        let bad = Query {
            r#where: Some(Expr::HasLink {
                path: "group_type".to_string(),
                target: None,
            }),
            ..query
        };
        assert!(matches!(
            compile(&bad, &graph).unwrap_err(),
            QueryError::DatatypeMismatch { .. }
        ));
    }

    #[test]
    fn negated_coarse_predicate_is_rejected() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::Not(Box::new(Expr::HasLink {
                path: "members".to_string(),
                target: None,
            }))),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        assert_eq!(
            compile(&query, &graph).unwrap_err(),
            QueryError::NegatedCoarsePredicate {
                path: "members".to_string()
            }
        );
    }

    #[test]
    fn boolean_combinators_and_model_check() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: Some(Expr::All(vec![
                Expr::Concept {
                    path: "group_type".to_string(),
                    op: ConceptOp::Is,
                    value: CONCEPT_ID.to_string(),
                },
                Expr::Not(Box::new(Expr::Any(vec![Expr::Concept {
                    path: "action".to_string(),
                    op: ConceptOp::Is,
                    value: CONCEPT_ID.to_string(),
                }]))),
            ])),
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        let stmts = compile(&query, &graph).expect("should compile");
        let sql = &stmts[0].sql;
        assert!(sql.contains(" AND NOT ("));
        // Shared concept id deduplicates to one param; two node uuids.
        assert_eq!(stmts[0].params.len(), 3);

        // Model is checked against slug/graphid/name.
        let mismatch = Query {
            model: "person".to_string(),
            ..concept_query(ConceptOp::Is, vec![Measure::CountRecords])
        };
        match compile(&mismatch, &graph).unwrap_err() {
            QueryError::ModelMismatch { expected, .. } => {
                assert!(expected.contains(&"group".to_string()));
            }
            other => panic!("expected ModelMismatch, got {other:?}"),
        }
        // Graph UUID is also accepted.
        let by_id = Query {
            model: "07883c9e-b25c-11e9-975a-a4d18cec433a".to_string(),
            ..concept_query(ConceptOp::Is, vec![Measure::CountRecords])
        };
        assert!(compile(&by_id, &graph).is_ok());
    }

    #[test]
    fn ir_round_trips_through_serde_json() {
        let json = serde_json::json!({
            "model": "group",
            "where": {
                "all": [
                    {"concept": {"path": "group_type",
                                  "op": "descendant_or_self_of",
                                  "value": CONCEPT_ID}},
                    {"has_link": {"path": "members"}}
                ]
            },
            "measures": ["count_records", "select_ids"],
            "limit": 10
        });
        let query: Query = serde_json::from_value(json.clone()).expect("IR deserializes");
        assert_eq!(
            query.measures,
            vec![Measure::CountRecords, Measure::SelectIds]
        );
        match query.r#where.as_ref().unwrap() {
            Expr::All(exprs) => {
                assert!(matches!(
                    &exprs[0],
                    Expr::Concept {
                        op: ConceptOp::DescendantOrSelfOf,
                        ..
                    }
                ));
                assert!(matches!(&exprs[1], Expr::HasLink { target: None, .. }));
            }
            other => panic!("expected All, got {other:?}"),
        }
        let back = serde_json::to_value(&query).expect("IR serializes");
        assert_eq!(back, json);

        // Errors serialize with a machine-readable kind tag.
        let graph = load_group_graph();
        let err = compile(&query, &graph);
        // (This query is valid — force an error to check serialization.)
        assert!(err.is_ok());
        let err_val = serde_json::to_value(QueryError::NotHeadIndexed {
            path: "name".to_string(),
            datatype: "string".to_string(),
        })
        .unwrap();
        assert_eq!(err_val["kind"], "not_head_indexed");
        assert_eq!(err_val["datatype"], "string");
    }

    #[test]
    fn no_where_and_empty_measures() {
        let graph = load_group_graph();
        let query = Query {
            model: "group".to_string(),
            r#where: None,
            measures: vec![Measure::CountRecords],
            limit: None,
        };
        let stmts = compile(&query, &graph).unwrap();
        assert_eq!(stmts[0].sql, "SELECT COUNT(*) FROM spine_group s");
        assert!(stmts[0].params.is_empty());

        let empty = Query {
            measures: vec![],
            ..query
        };
        assert_eq!(
            compile(&empty, &graph).unwrap_err(),
            QueryError::EmptyMeasures
        );
    }

    // --- Extension-datatype routing (proves the capability seam) ---------

    use alizarin_core::extension_type_registry::{
        ExtensionError, ExtensionTypeHandler, HandlerCapabilities, IndexSpec,
    };
    use std::sync::Arc;

    /// A handler that claims *any* value of its registered datatype as
    /// concept-hierarchical. Core knows nothing of this datatype; only the
    /// registry makes it concept-queryable.
    struct MockConceptHandler;
    impl ExtensionTypeHandler for MockConceptHandler {
        fn capabilities(&self) -> HandlerCapabilities {
            HandlerCapabilities {
                can_index: true,
                ..Default::default()
            }
        }
        fn index_spec(
            &self,
            _tile_data: &serde_json::Value,
            _config: Option<&serde_json::Value>,
        ) -> Result<Option<IndexSpec>, ExtensionError> {
            Ok(Some(IndexSpec {
                class: IndexClass::ConceptHierarchical { collection: None },
                keys: Vec::new(),
            }))
        }
    }

    /// Load the Group graph but rewrite `group_type`'s datatype to a name
    /// core has never heard of, so its indexability is decided purely by
    /// whether a registry claims it.
    fn load_group_graph_with_mock_datatype() -> StaticGraph {
        let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let test_file = crate_root.join("tests/data/models/Group.json");
        let json_str = fs::read_to_string(&test_file).expect("Failed to read Group.json");
        let mut json: serde_json::Value =
            serde_json::from_str(&json_str).expect("Failed to parse Group.json");
        for node in json["graph"][0]["nodes"].as_array_mut().unwrap() {
            if node["alias"] == serde_json::json!("group_type") {
                node["datatype"] = serde_json::json!("mock-concept");
            }
        }
        let graph_data = json["graph"][0].clone();
        let mut graph: StaticGraph =
            serde_json::from_value(graph_data).expect("Failed to deserialize StaticGraph");
        graph.build_indices();
        graph
    }

    #[test]
    fn extension_datatype_queryable_only_through_registry() {
        // `mock-concept` is invisible to core's built-in fallback
        // (DetailOnly), so a Concept filter on it is NotHeadIndexed with no
        // registry — proving core names no extension datatype.
        let graph = load_group_graph_with_mock_datatype();
        let query = concept_query(ConceptOp::Is, vec![Measure::CountRecords]);
        assert_eq!(
            compile(&query, &graph).unwrap_err(),
            QueryError::NotHeadIndexed {
                path: "group_type".to_string(),
                datatype: "mock-concept".to_string(),
            },
            "no registry: extension datatype is not head-indexed"
        );

        // With a registry whose handler claims `mock-concept` as
        // concept-hierarchical, the very same Concept filter compiles: the
        // datatype became queryable through the seam, not through core.
        let mut registry = ExtensionTypeRegistry::new();
        registry.register("mock-concept", Arc::new(MockConceptHandler));
        let stmts = compile_with_registry(&query, &graph, Some(&registry))
            .expect("registry makes the extension datatype concept-queryable");
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].sql.contains("concept_tags"));
    }
}
