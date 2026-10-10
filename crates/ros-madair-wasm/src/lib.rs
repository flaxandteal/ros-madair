// SPDX-License-Identifier: AGPL-3.0-or-later
//! The browser's half of "one compiler, two consumers" (Slice 4).
//!
//! A thin wasm-bindgen wrapper over [`ros_madair_compile`]: it takes the model
//! graph(s) and the typed `Query` as JSON, builds the alizarin `StaticGraph`s and
//! the handler registry in-wasm, and returns the DuckDB SQL string — which the
//! page hands straight to duckdb-wasm. No execution here (duckdb-wasm does that)
//! and no hand-written SQL (the drift hazard this replaces): the SQL is the exact
//! output of the same `compile_query` that `ros-madair-duck` runs natively.
//!
//! The capability booleans mirror the ones `DuckReader` derives natively from its
//! registered views — but here the caller supplies them, because only the page
//! knows which Parquet artifacts it registered with duckdb-wasm:
//! - `has_catalog` — a `concept_catalog.parquet` is registered (enables `DescendantOrSelfOf`)
//! - `has_edges` — an `edges_*.parquet` is registered (enables `HasLink`/`OnLink`)
//! - `has_concept_index` — a `concepts_*.parquet` melt is registered (enables `Concept`)
//! - `has_ordered` — an `ordered_*.parquet` melt is registered (enables `Range`)
//! - `spatial` — the DuckDB spatial extension is loaded (exact `ST_Intersects`); in the
//!   browser this is normally `false` → coarse bbox-overlap.

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::StaticGraph;
use ros_madair_query::Query;
use wasm_bindgen::prelude::*;

/// A reusable compiler bound to one model set. **Construct once, compile many.**
///
/// The expensive work — deserializing the (large) Arches model graph(s) and
/// `build_indices()` — happens once in [`Compiler::new`]; each [`Compiler::compile`]
/// then crosses only the small query-IR string. This is the design that matters
/// for a UI firing many queries at one model (facets, autocomplete): without it,
/// the free [`compile`] one-shot re-parses and re-indexes the whole graph on every
/// call. Mirrors native `DuckReader` (open once, query many).
#[wasm_bindgen]
pub struct Compiler {
    graphs: Vec<StaticGraph>,
    registry: ExtensionTypeRegistry,
}

#[wasm_bindgen]
impl Compiler {
    /// Parse the model graph(s) once. `graphs_json` is a JSON array of Arches model
    /// documents (or a single document); each element may be a bare graph object or
    /// the `{"graph":[{…}]}` envelope. Throws on malformed JSON.
    #[wasm_bindgen(constructor)]
    pub fn new(graphs_json: &str) -> Result<Compiler, JsError> {
        let graphs = parse_graphs(graphs_json).map_err(|e| JsError::new(&e))?;
        Ok(Compiler {
            graphs,
            registry: ros_madair_handlers::default_registry(),
        })
    }

    /// Lower one `Query` IR to a `SELECT DISTINCT resource_id` DuckDB SQL string,
    /// reusing this compiler's already-parsed graphs. Only the query string crosses
    /// the boundary. The capability booleans say which Parquet artifacts the caller
    /// registered with duckdb-wasm (see the module docs). Throws on malformed query
    /// JSON, a `model` absent from the graph set, or a typed compile error.
    #[wasm_bindgen]
    #[allow(clippy::too_many_arguments)]
    pub fn compile(
        &self,
        query_json: &str,
        has_catalog: bool,
        has_edges: bool,
        has_concept_index: bool,
        has_ordered: bool,
        spatial: bool,
    ) -> Result<String, JsError> {
        // Thin FFI shell over the pure core (a `#[wasm_bindgen]` method returning
        // `Result<_, JsError>` runs JS-interop ABI glue that can't execute on native,
        // so the testable core must not touch JsError). Map String → JsError.
        self.lower(
            query_json,
            has_catalog,
            has_edges,
            has_concept_index,
            has_ordered,
            spatial,
        )
        .map_err(|e| JsError::new(&e))
    }
}

impl Compiler {
    /// The pure core — no wasm-bindgen types, so it runs on native (the round-trip
    /// tests exercise it). Errors are plain messages.
    #[allow(clippy::too_many_arguments)]
    fn lower(
        &self,
        query_json: &str,
        has_catalog: bool,
        has_edges: bool,
        has_concept_index: bool,
        has_ordered: bool,
        spatial: bool,
    ) -> Result<String, String> {
        let query: Query =
            serde_json::from_str(query_json).map_err(|e| format!("query JSON: {e}"))?;
        let graph_refs: Vec<&StaticGraph> = self.graphs.iter().collect();
        let primary = self
            .graphs
            .iter()
            .find(|g| g.graphid == query.model)
            .ok_or_else(|| {
                format!(
                    "query model '{}' is not among the {} supplied graph(s)",
                    query.model,
                    self.graphs.len()
                )
            })?;
        ros_madair_compile::compile_query(
            &query,
            primary,
            &graph_refs,
            &self.registry,
            has_catalog,
            has_edges,
            has_concept_index,
            has_ordered,
            spatial,
        )
        .map_err(|e| e.0)
    }
}

/// One-shot convenience: parse the graph(s) AND compile one query. Prefer
/// [`Compiler`] when issuing more than one query against the same model — this
/// re-parses and re-indexes the graph every call.
#[wasm_bindgen]
#[allow(clippy::too_many_arguments)]
pub fn compile(
    graphs_json: &str,
    query_json: &str,
    has_catalog: bool,
    has_edges: bool,
    has_concept_index: bool,
    has_ordered: bool,
    spatial: bool,
) -> Result<String, JsError> {
    Compiler::new(graphs_json)?.compile(
        query_json,
        has_catalog,
        has_edges,
        has_concept_index,
        has_ordered,
        spatial,
    )
}

/// Parse one-or-many Arches model documents into indexed `StaticGraph`s. Mirrors
/// the native loader (`ros-madair-emit`'s `input`): deserialize, then the
/// mandatory `build_indices()` so alias/edge lookups resolve.
fn parse_graphs(graphs_json: &str) -> Result<Vec<StaticGraph>, String> {
    let val: serde_json::Value =
        serde_json::from_str(graphs_json).map_err(|e| format!("graphs JSON: {e}"))?;
    let docs = match val {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    let mut graphs = Vec::with_capacity(docs.len());
    for doc in docs {
        // Accept a bare graph object OR the Arches `{"graph":[{…}]}` envelope.
        let graph_val = doc
            .get("graph")
            .and_then(|g| g.get(0))
            .cloned()
            .unwrap_or(doc);
        let mut graph: StaticGraph =
            serde_json::from_value(graph_val).map_err(|e| format!("graph deserialize: {e}"))?;
        graph.build_indices();
        graphs.push(graph);
    }
    Ok(graphs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The `#[wasm_bindgen]` entry is an ordinary Rust fn on native (the rlib), so
    // these exercise the real JSON → StaticGraph → registry → compile_query glue
    // without a JS runtime. The actual wasm-boundary smoke test is a node script
    // run against the wasm-pack artifact.

    fn graph_json() -> String {
        json!([{
            "graphid": "test-g",
            "name": "TestGraph",
            "root": { "nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "test-g" },
            "nodes": [
                { "nodeid": "root", "name": "Root", "datatype": "semantic", "graph_id": "test-g" },
                { "nodeid": "cat-node", "name": "Category", "alias": "category",
                  "datatype": "reference", "nodegroup_id": "cat-ng",
                  "graph_id": "test-g", "is_collector": true },
            ],
            "nodegroups": [ {"nodegroupid": "cat-ng", "cardinality": "1", "parentnodegroup_id": null} ],
            "edges": [ {"domainnode_id": "root", "rangenode_id": "cat-node", "graph_id": "test-g"} ],
        }])
        .to_string()
    }

    #[test]
    fn compiles_a_concept_query_to_the_expected_sql() {
        let query = json!({
            "model": "test-g",
            "where": { "concept": { "path": "category", "op": "is", "value": "my-concept" } },
            "measures": []
        })
        .to_string();
        // has_concept_index = true (the melt is registered); nothing else.
        let sql = Compiler::new(&graph_json())
            .unwrap()
            .lower(&query, false, false, true, false, false)
            .expect("compile should succeed");
        assert_eq!(
            sql,
            "SELECT DISTINCT resource_id FROM concept_index \
             WHERE node_id = 'cat-node' AND concept_id = 'my-concept'"
        );
    }

    #[test]
    fn empty_where_is_all_resources() {
        let query = json!({ "model": "test-g", "measures": [] }).to_string();
        let sql = Compiler::new(&graph_json())
            .unwrap()
            .lower(&query, false, false, false, false, false)
            .unwrap();
        assert_eq!(sql, "SELECT DISTINCT resource_id FROM tiles");
    }

    #[test]
    fn unknown_model_is_a_loud_error() {
        let query = json!({ "model": "no-such-graph", "measures": [] }).to_string();
        let err = Compiler::new(&graph_json())
            .unwrap()
            .lower(&query, false, false, false, false, false);
        assert!(
            err.is_err(),
            "a query naming an absent model must fail, not mis-compile"
        );
    }

    #[test]
    fn missing_concept_melt_is_refused() {
        let query = json!({
            "model": "test-g",
            "where": { "concept": { "path": "category", "op": "is", "value": "x" } },
            "measures": []
        })
        .to_string();
        // has_concept_index = false → the Concept predicate cannot be served.
        let err = Compiler::new(&graph_json())
            .unwrap()
            .lower(&query, false, false, false, false, false);
        assert!(
            err.is_err(),
            "a Concept filter without the melt must be a typed error"
        );
    }
}
