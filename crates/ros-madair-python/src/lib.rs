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
//! sql = g.compile(ir_json)                       # default registry
//! sql = g.compile(ir_json, manifest_json=man)    # registry the snapshot declares
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
use ros_madair_handlers::HandlerDecl;
use ros_madair_query::{compile_with_registry, Query};

/// The fallback registry: `ros_madair_handlers::default_registry` — the ONE
/// definition, shared with the emitter. Used only when the caller supplies no
/// manifest; when they do, the registry is derived from the manifest's declared
/// handler set instead of assumed (see `Graph::compile`).
fn registry() -> &'static ExtensionTypeRegistry {
    static REGISTRY: std::sync::OnceLock<ExtensionTypeRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(ros_madair_handlers::default_registry)
}

/// Rebuild the registry the snapshot was EMITTED with, from its manifest's
/// `handlers` block. Raises `ValueError` — loudly, naming the datatype — if the
/// snapshot declares a handler this build cannot provide, rather than compiling
/// SQL that would run fine and return zero rows against an index that never
/// contained those fields.
fn registry_from_manifest(manifest_json: &str) -> PyResult<ExtensionTypeRegistry> {
    let manifest: serde_json::Value = serde_json::from_str(manifest_json)
        .map_err(|e| PyValueError::new_err(format!("manifest JSON is not valid JSON: {e}")))?;
    let handlers = manifest.get("handlers").ok_or_else(|| {
        PyValueError::new_err(
            "manifest declares no `handlers`, so the registry the snapshot was emitted with \
cannot be derived from it — re-emit the snapshot, or omit manifest_json to use the default registry",
        )
    })?;
    let decls: Vec<HandlerDecl> = serde_json::from_value(handlers.clone()).map_err(|e| {
        PyValueError::new_err(format!(
            "manifest `handlers` is not a handler declaration list: {e}"
        ))
    })?;
    ros_madair_handlers::registry_from_declarations(&decls).map_err(PyValueError::new_err)
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
        let mut graph: StaticGraph = serde_json::from_value(value).map_err(|e| {
            PyValueError::new_err(format!("graph JSON is not an Arches graph: {e}"))
        })?;
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
    ///
    /// `manifest_json` (optional) is the snapshot's `manifest.json`. PASS IT
    /// when you have it: the compiler then plans with the registry the snapshot
    /// declares it was emitted with, instead of assuming the default. The two
    /// disagreeing is a silent-wrong-answer bug — a `reference` field emitted
    /// WITHOUT the clm handler is never indexed, yet a compiler WITH the handler
    /// compiles a valid query over it and gets zero rows back. With the manifest,
    /// an unprovidable handler raises here instead. Omitted, the default registry
    /// is used (back-compat / convenience).
    #[pyo3(signature = (ir_json, manifest_json=None))]
    fn compile(&self, ir_json: String, manifest_json: Option<String>) -> PyResult<String> {
        let query: Query = serde_json::from_str(&ir_json)
            .map_err(|e| PyValueError::new_err(format!("invalid query IR: {e}")))?;

        let from_manifest = match &manifest_json {
            Some(text) => Some(registry_from_manifest(text)?),
            None => None,
        };
        let registry = from_manifest.as_ref().unwrap_or_else(|| registry());

        let statements = compile_with_registry(&query, &self.inner, Some(registry))
            .map_err(|e| PyValueError::new_err(e.to_string()))?;

        serde_json::to_string(&statements)
            .map_err(|e| PyValueError::new_err(format!("could not serialize statements: {e}")))
    }
}

/// Free-function form of [`Graph::compile`], for callers who prefer it:
/// `compile_query(ir_json, graph)`.
#[pyfunction]
#[pyo3(signature = (ir_json, graph, manifest_json=None))]
fn compile_query(
    ir_json: String,
    graph: PyRef<Graph>,
    manifest_json: Option<String>,
) -> PyResult<String> {
    graph.compile(ir_json, manifest_json)
}

#[pymodule]
fn ros_madair_v2(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Graph>()?;
    m.add_function(wrap_pyfunction!(compile_query, m)?)?;
    Ok(())
}
