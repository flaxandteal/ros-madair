// SPDX-License-Identifier: AGPL-3.0-or-later
//! Python binding for the Rós Madair v2 query compiler.
//!
//! # Why this crate exists
//!
//! `compile_query` used to live in `alizarin-python`. It was removed in the
//! split: keeping it there would have dragged the whole query crate into the
//! ORM binding. Its correct home is here, next to `ros-madair-query`, where it
//! can freely depend on `alizarin-clm-core` and carry a registry with the CLM
//! `reference` handler registered. That registry is the whole point — without
//! it, `reference`-typed nodes are invisible to the compiler and a Concept
//! filter over one fails as "not head-indexed". `run_ir` (native) already wires
//! the CLM registry; this crate closes the same gap for Python/MCP callers.
//!
//! # The graph is a handle, not a global
//!
//! Callers build a [`Graph`] from the Arches resource-model export and compile
//! against it:
//!
//! ```python
//! g = ros_madair_v2.Graph(open("Group.json").read())
//! sql = g.compile(ir_json)          # or: ros_madair_v2.compile_query(ir_json, g)
//! ```
//!
//! Two things follow from passing the graph rather than a graph id. First,
//! correctness: two PyO3 cdylibs each statically link their own copy of
//! `alizarin-core`, so each gets its *own* global graph registry — a graph
//! registered through `alizarin.register_graph()` would simply not exist as far
//! as this module is concerned. Second, cost: graphs run to megabytes, so the
//! parse + `build_indices` is paid once, at handle construction, and every
//! subsequent `compile` borrows the already-indexed graph.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::StaticGraph;
use ros_madair_query::{compile_with_registry, Query};

/// The extension-type registry this binding compiles against: the CLM
/// `reference` handler registered under its datatype name. Mirrors
/// `ros_madair_emit::default_registry` — core itself knows nothing of
/// `reference`; the datatype (and its `ConceptHierarchical` index class, which
/// is what makes it queryable) is contributed entirely by this handler.
pub fn default_registry() -> ExtensionTypeRegistry {
    let mut registry = ExtensionTypeRegistry::new();
    registry.register(
        alizarin_clm_core::DATATYPE_NAME,
        alizarin_clm_core::create_reference_handler(),
    );
    registry
}

fn registry() -> &'static ExtensionTypeRegistry {
    static REGISTRY: std::sync::OnceLock<ExtensionTypeRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(default_registry)
}

/// A parsed, indexed resource-model graph: the schema queries compile against.
///
/// Construct once from the Arches resource-model export JSON — either a bare
/// graph object or a `{"graph":[...]}` export — and reuse for every compile.
#[pyclass]
pub struct Graph {
    inner: Arc<StaticGraph>,
}

#[pymethods]
impl Graph {
    /// Parse and index an Arches resource-model export.
    ///
    /// Raises `ValueError` if the text is not JSON, or not a graph.
    #[new]
    fn new(graph_json: String) -> PyResult<Self> {
        let value: serde_json::Value = serde_json::from_str(&graph_json)
            .map_err(|e| PyValueError::new_err(format!("graph JSON is not valid JSON: {e}")))?;
        // Accept a bare Arches graph object or a {"graph":[...]} export.
        let value = match value.get("graph").and_then(|g| g.get(0)) {
            Some(g) => g.clone(),
            None => value,
        };
        let mut graph: StaticGraph = serde_json::from_value(value)
            .map_err(|e| PyValueError::new_err(format!("graph JSON is not an Arches graph: {e}")))?;
        graph.build_indices();
        Ok(Graph {
            inner: Arc::new(graph),
        })
    }

    /// The graph's UUID.
    #[getter]
    fn graph_id(&self) -> String {
        self.inner.graph_id().to_string()
    }

    /// Compile a query IR against this graph, returning the compiled
    /// statements as JSON.
    ///
    /// `ir_json` is a `ros_madair_query::Query` (model / where / measures /
    /// limit). Returns a JSON array of `CompiledStatement` (`measure`, `sql`,
    /// `params`, `coarse`).
    ///
    /// Raises `ValueError` on a malformed IR or a `QueryError`; the message is
    /// the `QueryError`'s `Display` text verbatim, so the typed, repairable
    /// errors reach Python callers (and agents) intact.
    fn compile(&self, ir_json: String) -> PyResult<String> {
        let query: Query = serde_json::from_str(&ir_json)
            .map_err(|e| PyValueError::new_err(format!("invalid query IR: {e}")))?;

        let statements = compile_with_registry(&query, &self.inner, Some(registry()))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;

        serde_json::to_string(&statements)
            .map_err(|e| PyValueError::new_err(format!("could not serialize statements: {e}")))
    }
}

/// Free-function form of [`Graph::compile`], for callers who prefer it:
/// `compile_query(ir_json, graph)`.
#[pyfunction]
fn compile_query(ir_json: String, graph: PyRef<Graph>) -> PyResult<String> {
    graph.compile(ir_json)
}

#[pymodule]
fn ros_madair_v2(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Graph>()?;
    m.add_function(wrap_pyfunction!(compile_query, m)?)?;
    Ok(())
}
