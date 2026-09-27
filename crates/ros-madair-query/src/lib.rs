// SPDX-License-Identifier: AGPL-3.0-or-later
//! The typed, schema-validated query **IR** — and the authoring surface around
//! it.
//!
//! A serde-typed intermediate representation for record-selection queries
//! ([`Query`]/[`Expr`]: `Concept`/`Range`/`Bbox`/`HasLink`/`OnLink`/`OnTile`,
//! combined with `All`/`Any`/`Not`), validated against a [`StaticGraph`] (aliases
//! → nodes, datatype → predicate family). This crate is **backend-agnostic**: it
//! defines the IR and the tools to author and inspect it; `ros-madair-duck`
//! compiles the IR to DuckDB SQL over the Parquet substrate. (The v1 head-SQL
//! compiler that used to live here has been deleted along with the head engine.)
//!
//! What remains here besides the IR types:
//!
//! - [`PathResolver`] — resolve a dotted alias path (`address.location`) to its
//!   node, with repairable `UnknownPath` errors that name the closest siblings.
//! - [`ModelCatalog`] — a compact, serializable description of every queryable
//!   path in a model (path → which [`NodePredicate`] to build), the discovery
//!   surface a natural-language → IR layer (a skill / MCP) reads.
//! - [`explain`] — render a [`Query`] back to English, so a caller can VERIFY an
//!   NL → IR translation (notably the `OnTile` "single record" vs `All`
//!   "independent facts" distinction) before running it.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use alizarin_core::datatype_index::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode};

/// Default row cap for `select_ids` when the query carries no `limit`
/// (mirrors the emitted manifest's default `max_result_rows` budget).
pub const DEFAULT_SELECT_LIMIT: u32 = 1000;

// ---------------------------------------------------------------------------
// IR types
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
    /// Cross-resource PATH predicate: resources whose `path` link points at a
    /// resource (of `model`) that matches `where`. `path` is a
    /// `resource-instance(-list)` node alias (dot-qualified to reach it through
    /// within-resource nodegroups); `model` names the TARGET model the inner
    /// predicate is evaluated against — the link node's config does not carry the
    /// target, so the hop declares it. Nestable: a `where` that is itself an
    /// `OnLink` is a multi-hop chain. Substrate-only — it lowers to an edge-table
    /// semijoin (`ros-madair-duck`), not head SQL.
    OnLink {
        path: String,
        model: String,
        #[serde(rename = "where")]
        r#where: Box<Expr>,
    },
    /// Same-TILE correlation: every child must be satisfied by ONE tile (one
    /// nodegroup instance) of the resource — not merely somewhere in the resource
    /// (that is `All`). "built by Lanyon between 1860–1900" is
    /// `OnTile([date ∈ range, architect → …Lanyon])`: the SAME construction event
    /// must carry both, so a resource whose 1855 build was Lanyon's and whose
    /// 1870 restoration was another's does NOT match. Children must resolve to a
    /// single nodegroup (a tile belongs to one nodegroup); mixing nodegroups is a
    /// typed error. Substrate-only — it lowers to a single-tile predicate with any
    /// link/hop child joined on `edges.src_tile` (`ros-madair-duck`), which the
    /// head schema cannot express.
    OnTile(Vec<Expr>),
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
    /// The IR uses a capability the target backend does not implement. Retained
    /// as a typed, serializable signal for backends/validators that need it.
    Unsupported { feature: String },
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
            QueryError::Unsupported { feature } => {
                write!(f, "feature not supported by this backend: {feature}")
            }
        }
    }
}

impl std::error::Error for QueryError {}

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

// ---------------------------------------------------------------------------
// Model catalog (discovery surface for an LLM / MCP translation layer)
// ---------------------------------------------------------------------------

/// Which IR predicate a node answers — the discovery hint a caller needs to know
/// *which* [`Expr`] to build for a path, without trial-and-error.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodePredicate {
    /// concept / concept-list / domain-value / (ext) reference → [`Expr::Concept`].
    Concept,
    /// date / ordered scalar → [`Expr::Range`].
    Range,
    /// geometry → [`Expr::Bbox`].
    Bbox,
    /// resource-instance(-list) → [`Expr::HasLink`], and [`Expr::OnLink`] for a
    /// cross-model hop (whose inner predicate can nest an [`Expr::OnTile`]).
    Link,
    /// Not substrate-indexed — available only in the hydrated tile (post-filter).
    Detail,
}

/// One queryable path in a model: enough for a caller to form a predicate against
/// it without loading the whole graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathDescriptor {
    /// Dotted alias path from the root (e.g. `"construction.founded"`); also usable
    /// bare (`"founded"`) when the alias is unambiguous.
    pub path: String,
    /// The leaf alias.
    pub alias: String,
    /// The Arches datatype.
    pub datatype: String,
    /// Which IR predicate this node answers.
    pub predicate: NodePredicate,
    /// The node's nodegroup id — the correlation unit for [`Expr::OnTile`]: two
    /// paths sharing a `nodegroup` can be co-required on one tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodegroup: Option<String>,
    /// For a `Link` node: the permitted target **model** graph-ids it may point at,
    /// from the node's `graphs` schema config, so an [`Expr::OnLink`] hop's `model`
    /// can be discovered. This is schema-level (which models a hop *may* reach), NOT
    /// the retired per-tile `link_targets` column (actual target resources, now the
    /// edge table). Empty when the model does not declare them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_models: Vec<String>,
}

/// A compact, serializable description of every queryable path in one model — the
/// discovery surface for a natural-language → IR translation layer (e.g. Clódóir's
/// skill / MCP). Build it once per model; hold it or transmit it as JSON. It
/// answers "what can I filter on, and with which predicate?" so a caller forms a
/// valid query without holding the whole graph or guessing datatypes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalog {
    /// The emitted slug (a valid [`Query::model`]).
    pub model: String,
    /// The graph UUID.
    pub graph_id: String,
    /// The display name.
    pub name: String,
    /// Every aliased path, sorted by path for a stable surface.
    pub paths: Vec<PathDescriptor>,
}

impl ModelCatalog {
    /// Build the catalog for `graph`. Pass the extension registry to classify
    /// extension datatypes (e.g. CLM `reference` → `Concept`); with `None`, only
    /// core datatypes classify and extension nodes read as `Detail` — the same
    /// registry discipline `ros-madair-duck`'s compiler follows.
    pub fn build(graph: &StaticGraph, registry: Option<&ExtensionTypeRegistry>) -> Self {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in graph.edges_slice() {
            children
                .entry(e.domainnode_id.as_str())
                .or_default()
                .push(e.rangenode_id.as_str());
        }
        let by_id: HashMap<&str, &StaticNode> = graph
            .nodes_slice()
            .iter()
            .map(|n| (n.nodeid.as_str(), n))
            .collect();

        let root = graph.get_root();
        let mut paths = Vec::new();
        // DFS from root; `path` is the node's dotted path ("" for the root, which is
        // not itself a queryable component). An aliasless intermediate node is
        // transparent — it contributes no path component and is not emitted.
        let mut stack: Vec<(&StaticNode, String)> = vec![(root, String::new())];
        while let Some((node, path)) = stack.pop() {
            if node.alias.is_some() && !path.is_empty() {
                paths.push(PathDescriptor {
                    path: path.clone(),
                    alias: node.alias.clone().unwrap_or_default(),
                    datatype: node.datatype.clone(),
                    predicate: predicate_of(node, registry),
                    nodegroup: node.nodegroup_id.clone(),
                    target_models: target_models_of(node),
                });
            }
            for cid in children
                .get(node.nodeid.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(child) = by_id.get(cid).copied() else {
                    continue;
                };
                let child_path = match child.alias.as_deref() {
                    Some(a) if path.is_empty() => a.to_string(),
                    Some(a) => format!("{path}.{a}"),
                    None => path.clone(),
                };
                stack.push((child, child_path));
            }
        }
        paths.sort_by(|a, b| a.path.cmp(&b.path));

        let name = graph.display_name();
        ModelCatalog {
            model: emit_slug(&name),
            graph_id: graph.graph_id().to_string(),
            name,
            paths,
        }
    }

    /// Rank paths by closeness to `term` (exact, then substring, then edit distance
    /// over both the alias and the dotted path), returning up to `limit`. A typo /
    /// prefix aid — synonym reasoning ("builder" → "architect") is the caller's job,
    /// over [`paths`](Self::paths) as the vocabulary.
    pub fn search(&self, term: &str, limit: usize) -> Vec<&PathDescriptor> {
        let t = term.to_lowercase();
        let mut scored: Vec<(&PathDescriptor, usize)> = self
            .paths
            .iter()
            .map(|p| {
                let a = p.alias.to_lowercase();
                let path = p.path.to_lowercase();
                let score = if a == t || path == t {
                    0
                } else if a.contains(&t) || path.contains(&t) {
                    1
                } else {
                    2 + levenshtein(&t, &a).min(levenshtein(&t, &path))
                };
                (p, score)
            })
            .collect();
        scored.sort_by(|x, y| x.1.cmp(&y.1).then_with(|| x.0.path.cmp(&y.0.path)));
        scored.into_iter().take(limit).map(|(p, _)| p).collect()
    }
}

/// Map a node's index class to the IR predicate it answers.
fn predicate_of(node: &StaticNode, registry: Option<&ExtensionTypeRegistry>) -> NodePredicate {
    match datatype_index_spec(
        &node.datatype,
        &serde_json::Value::Null,
        node_config_value(node).as_ref(),
        registry,
    )
    .class
    {
        IndexClass::ConceptHierarchical { .. } => NodePredicate::Concept,
        IndexClass::Ordered => NodePredicate::Range,
        IndexClass::SpatialBbox => NodePredicate::Bbox,
        IndexClass::Link => NodePredicate::Link,
        IndexClass::DetailOnly => NodePredicate::Detail,
    }
}

/// The permitted target model graph-ids of a resource-instance node, from its
/// `graphs` config (the Arches convention). Empty when absent — the hop's `model`
/// then has to be supplied explicitly.
fn target_models_of(node: &StaticNode) -> Vec<String> {
    let Some(cfg) = node_config_value(node) else {
        return Vec::new();
    };
    let Some(graphs) = cfg.get("graphs").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    graphs
        .iter()
        .filter_map(|g| g.get("graphid").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

// ---------------------------------------------------------------------------
// Explain (IR → English, for verifying a NL → IR translation)
// ---------------------------------------------------------------------------

/// Render a query back to English prose so a caller — or an LLM/skill — can VERIFY
/// a natural-language → IR translation before running it. The distinction it exists
/// to surface: `OnTile` reads as "a single <nodegroup> record where …" (correlated),
/// while `All` reads as a plain conjunction (independent across the resource) — the
/// tell that catches a "built by Lanyon in 1860" which silently became two separate
/// facts.
///
/// Values render as-is (concept/resource ids, quantized range endpoints); structure,
/// not labels, is what catches a mistranslation. A caller holding a label map can
/// substitute human labels afterward.
pub fn explain(query: &Query, graph: &StaticGraph) -> String {
    let resolver = PathResolver::new(graph);
    let verb = match (
        query.measures.contains(&Measure::CountRecords),
        query.measures.contains(&Measure::SelectIds),
    ) {
        (true, true) => "Count and list",
        (true, false) => "Count",
        _ => "List",
    };
    let clause = match &query.r#where {
        Some(e) => format!("where {}", explain_expr(e, &resolver, graph)),
        None => "(no filter)".to_string(),
    };
    format!("{verb} '{}' records {clause}.", query.model)
}

fn explain_expr(expr: &Expr, resolver: &PathResolver, graph: &StaticGraph) -> String {
    match expr {
        Expr::All(cs) if cs.is_empty() => "anything".to_string(),
        Expr::Any(cs) if cs.is_empty() => "nothing".to_string(),
        Expr::All(cs) => explain_join(cs, " and ", resolver, graph),
        Expr::Any(cs) => explain_join(cs, " or ", resolver, graph),
        Expr::Not(inner) => format!("not ({})", explain_expr(inner, resolver, graph)),
        Expr::Concept { path, op, value } => match op {
            ConceptOp::Is => format!("{path} is {value}"),
            ConceptOp::DescendantOrSelfOf => format!("{path} is {value} or a narrower concept"),
        },
        Expr::Range { path, lo, hi } => format!("{path} is between {lo} and {hi} (quantized)"),
        Expr::Bbox {
            path,
            min_lng,
            min_lat,
            max_lng,
            max_lat,
        } => {
            format!("{path} lies within ({min_lng}, {min_lat})–({max_lng}, {max_lat})")
        }
        Expr::HasLink { path, target } => match target {
            Some(t) => format!("{path} links to {t}"),
            None => format!("{path} has any link"),
        },
        Expr::OnLink {
            path,
            model,
            r#where,
        } => format!(
            "{path} points to a '{model}' record where {}",
            explain_expr(r#where, resolver, graph)
        ),
        Expr::OnTile(cs) => format!(
            "a single {} record where {}",
            tile_group_name(cs, resolver, graph),
            explain_join(cs, " and ", resolver, graph)
        ),
    }
}

/// Join child clauses, parenthesizing each when there is more than one.
fn explain_join(
    children: &[Expr],
    sep: &str,
    resolver: &PathResolver,
    graph: &StaticGraph,
) -> String {
    if children.len() == 1 {
        return explain_expr(&children[0], resolver, graph);
    }
    children
        .iter()
        .map(|c| format!("({})", explain_expr(c, resolver, graph)))
        .collect::<Vec<_>>()
        .join(sep)
}

/// A human name for the nodegroup an `OnTile`'s children share — the collector
/// node's alias, else the nodegroup id, else "record". Best-effort (explain never
/// errors); the compiler is what enforces a single nodegroup.
fn tile_group_name(children: &[Expr], resolver: &PathResolver, graph: &StaticGraph) -> String {
    for c in children {
        if let Some(ng) = explain_leaf_ng(c, resolver) {
            if let Some(node) = graph.get_node_by_id(&ng) {
                return node.alias.clone().unwrap_or(ng);
            }
            return ng;
        }
    }
    "record".to_string()
}

fn explain_leaf_ng(expr: &Expr, resolver: &PathResolver) -> Option<String> {
    match expr {
        Expr::Concept { path, .. }
        | Expr::Range { path, .. }
        | Expr::Bbox { path, .. }
        | Expr::HasLink { path, .. }
        | Expr::OnLink { path, .. } => resolver
            .resolve(path)
            .ok()
            .and_then(|n| n.nodegroup_id.clone()),
        Expr::All(cs) | Expr::Any(cs) | Expr::OnTile(cs) => {
            cs.iter().find_map(|c| explain_leaf_ng(c, resolver))
        }
        Expr::Not(inner) => explain_leaf_ng(inner, resolver),
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

    /// A minimal inline model exercising the three promoted predicate kinds plus a
    /// resource-instance link with a declared target — the discovery surface a skill
    /// reads. Same graph shape the duck integration tests deserialize.
    fn catalog_graph() -> StaticGraph {
        const G: &str = "aaaaaaaa-0000-4000-8000-000000000000";
        const ROOT: &str = "00000000-0000-4000-8000-000000000000";
        const KIND: &str = "11111111-0000-4000-8000-000000000000";
        const FOUNDED: &str = "22222222-0000-4000-8000-000000000000";
        const MAKER: &str = "33333333-0000-4000-8000-000000000000";
        const TARGET: &str = "99999999-0000-4000-8000-000000000000";
        let root = serde_json::json!({
            "nodeid": ROOT, "name": "Thing", "alias": "thing", "datatype": "semantic",
            "graph_id": G, "istopnode": true,
        });
        let node = |id: &str, alias: &str, dt: &str, cfg: serde_json::Value| {
            serde_json::json!({
                "nodeid": id, "nodegroup_id": id, "name": alias, "alias": alias, "datatype": dt,
                "graph_id": G, "istopnode": false, "is_collector": true, "config": cfg,
            })
        };
        let edge = |from: &str, to: &str| serde_json::json!({ "edgeid": to, "domainnode_id": from, "rangenode_id": to, "graph_id": G });
        let doc = serde_json::json!({ "graph": [{
            "graphid": G, "name": "Thing", "root": root.clone(),
            "nodes": [
                root,
                node(KIND, "kind", "concept", serde_json::json!({})),
                node(FOUNDED, "founded", "date", serde_json::json!({})),
                node(MAKER, "maker", "resource-instance-list",
                     serde_json::json!({ "graphs": [{ "graphid": TARGET }] })),
            ],
            "nodegroups": [
                { "nodegroupid": KIND, "cardinality": "1", "parentnodegroup_id": null },
                { "nodegroupid": FOUNDED, "cardinality": "1", "parentnodegroup_id": null },
                { "nodegroupid": MAKER, "cardinality": "1", "parentnodegroup_id": null },
            ],
            "edges": [ edge(ROOT, KIND), edge(ROOT, FOUNDED), edge(ROOT, MAKER) ],
        }]});
        let mut g: StaticGraph = serde_json::from_value(doc["graph"][0].clone()).unwrap();
        g.build_indices();
        g
    }

    #[test]
    fn catalog_describes_predicates_and_target_models() {
        let cat = ModelCatalog::build(&catalog_graph(), None);
        let by_alias = |a: &str| {
            cat.paths
                .iter()
                .find(|p| p.alias == a)
                .expect("path present")
        };

        // Each node advertises which IR predicate to build against it.
        assert_eq!(by_alias("kind").predicate, NodePredicate::Concept);
        assert_eq!(by_alias("founded").predicate, NodePredicate::Range);
        let maker = by_alias("maker");
        assert_eq!(maker.predicate, NodePredicate::Link);
        // The on_link target model is discovered from the node's `graphs` config.
        assert_eq!(
            maker.target_models,
            vec!["99999999-0000-4000-8000-000000000000".to_string()]
        );

        // The root (semantic, aliased "thing") is not itself a queryable component.
        assert!(cat.paths.iter().all(|p| p.alias != "thing"));

        // Fuzzy search tolerates a typo and ranks the intended alias first.
        assert_eq!(cat.search("foundde", 3)[0].alias, "founded");

        // Serializes as a compact snake_case surface for an MCP call.
        let json = serde_json::to_string(&cat).unwrap();
        assert!(
            json.contains("\"predicate\":\"link\""),
            "predicate hint present: {json}"
        );
    }

    #[test]
    fn explain_surfaces_on_tile_vs_all_distinction() {
        let g = catalog_graph();
        let founded = Expr::Range {
            path: "founded".into(),
            lo: 100,
            hi: 200,
        };
        let maker = Expr::HasLink {
            path: "maker".into(),
            target: Some("x".into()),
        };
        let q = |w: Expr| Query {
            model: "thing".into(),
            r#where: Some(w),
            measures: vec![Measure::CountRecords],
            limit: None,
        };

        // `All`: a plain conjunction — must NOT read as one record.
        let all = explain(&q(Expr::All(vec![founded.clone(), maker.clone()])), &g);
        assert!(all.contains("founded is between 100 and 200"), "{all}");
        assert!(all.contains("maker links to x"), "{all}");
        assert!(
            !all.contains("a single"),
            "All must not read as one record: {all}"
        );

        // `OnTile`: the same-record tell that catches the mistranslation.
        let on_tile = explain(&q(Expr::OnTile(vec![founded, maker])), &g);
        assert!(
            on_tile.contains("a single"),
            "OnTile must read as one record: {on_tile}"
        );
        assert!(on_tile.contains("record where"), "{on_tile}");

        // Concept descendant renders the hierarchy hint.
        let c = explain(
            &q(Expr::Concept {
                path: "kind".into(),
                op: ConceptOp::DescendantOrSelfOf,
                value: "z".into(),
            }),
            &g,
        );
        assert!(c.contains("kind is z or a narrower concept"), "{c}");

        // The verb reflects the measure.
        assert!(all.starts_with("Count 'thing' records where"), "{all}");
    }

    const CONCEPT_ID: &str = "d8c60bf4-e786-11e6-905a-b756ec83dad5";

    #[test]
    fn on_link_serde_roundtrips() {
        // The binding/human layer emits this; it round-trips as `on_link` with the
        // inner predicate under `where`, and nests for multi-hop.
        let ir = Expr::OnLink {
            path: "location".to_string(),
            model: "Place".to_string(),
            r#where: Box::new(Expr::Bbox {
                path: "geospatial_coordinates".to_string(),
                min_lng: 0.0,
                min_lat: 0.0,
                max_lng: 1.0,
                max_lat: 1.0,
            }),
        };
        let json = serde_json::to_string(&ir).unwrap();
        assert!(json.contains("\"on_link\""), "tagged snake_case: {json}");
        assert!(
            json.contains("\"where\""),
            "inner serialized as `where`: {json}"
        );
        let back: Expr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ir, "round-trips");
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
        let err_val = serde_json::to_value(QueryError::NotHeadIndexed {
            path: "name".to_string(),
            datatype: "string".to_string(),
        })
        .unwrap();
        assert_eq!(err_val["kind"], "not_head_indexed");
        assert_eq!(err_val["datatype"], "string");
    }

    // --- Extension-datatype routing (proves the capability seam) ---------

    use alizarin_core::extension_type_registry::{
        ExtensionError, ExtensionTypeHandler, HandlerCapabilities, IndexSpec,
    };
    use std::sync::Arc;

    /// A handler that claims *any* value of its registered datatype as
    /// concept-hierarchical. Core knows nothing of this datatype; only the
    /// registry makes it concept-classified.
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
    /// core has never heard of, so its classification is decided purely by
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
    fn extension_datatype_classified_only_through_registry() {
        // `mock-concept` is invisible to core's built-in fallback (DetailOnly), so
        // the catalog marks it detail-only with no registry — proving core names no
        // extension datatype. The predicate family IS the classification the head
        // backend used to gate on; now it drives IR authoring.
        let graph = load_group_graph_with_mock_datatype();
        let by_alias = |cat: &ModelCatalog, a: &str| {
            cat.paths
                .iter()
                .find(|p| p.alias == a)
                .expect("group_type path present")
                .predicate
        };
        assert_eq!(
            by_alias(&ModelCatalog::build(&graph, None), "group_type"),
            NodePredicate::Detail,
            "no registry: extension datatype is not indexed"
        );

        // With a registry whose handler claims `mock-concept` as
        // concept-hierarchical, the very same node classifies as a Concept
        // predicate: it became queryable through the seam, not through core.
        let mut registry = ExtensionTypeRegistry::new();
        registry.register("mock-concept", Arc::new(MockConceptHandler));
        assert_eq!(
            by_alias(&ModelCatalog::build(&graph, Some(&registry)), "group_type"),
            NodePredicate::Concept,
            "registry makes the extension datatype concept-classified"
        );
    }
}
