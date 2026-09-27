// SPDX-License-Identifier: AGPL-3.0-or-later
// pyo3's `#[pymethods]` expands each fallible method into module-level wrapper
// functions that add an `Into<PyErr>` on the return; when the body already yields
// `PyErr` those `.into()`s are useless, and clippy flags them (mis-spanned onto our
// return types). The wrappers live outside the `impl`, so an item-level allow does
// not reach them under `--all-targets` (the `lib test` build) — hence crate-level.
#![allow(clippy::useless_conversion)]
//! Python binding for the Rós Madair DuckDB + Parquet substrate.
//!
//! A [`Graph`] is the schema handle — parse the Arches resource-model export once
//! (megabytes; `build_indices` is paid at construction) and reuse it. A [`Reader`]
//! opens a Parquet snapshot and answers the `Query` IR against it, all through
//! `ros-madair-duck`:
//!
//! ```python
//! g = ros_madair_v2.Graph(open("Group.json").read())
//! r = ros_madair_v2.Reader("/path/to/snapshot")   # its tiles_*/edges_*/concept_catalog
//! ids  = r.resolve(ir_json, g)                     # matching resource ids
//! n    = r.count(ir_json, g)                       # COUNT(*)
//! tree = r.hydrate(uuid, g)                        # schema-shaped JSON tree
//! ```
//!
//! The graph is passed as a handle, not a graph id: two PyO3 cdylibs each link
//! their own `alizarin-core`, so a graph registered through `alizarin` would not
//! exist here. The manifest → registry logic is kept — pass `manifest_json` and
//! the query plans with the handler set the snapshot declares it was emitted with
//! (a `reference` field emitted WITHOUT the clm handler was never indexed, so a
//! compiler WITH it would silently return zero rows; with the manifest that raises
//! instead).
//!
//! This replaces the former head-SQL compiler binding (`compile → SQL`) and the
//! head/chunk hydration: both went with the v1 head engine. The substrate runs the
//! query itself (`Reader.resolve`), rather than handing back SQL for the caller to
//! run against a head.

use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::StaticGraph;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::HandlerDecl;
use ros_madair_query::Query;

/// The fallback registry: `ros_madair_handlers::default_registry` — the ONE
/// definition, shared with the emitter. Used only when the caller supplies no
/// manifest; with one, the registry is derived from its declared handler set.
fn registry() -> &'static ExtensionTypeRegistry {
    static REGISTRY: std::sync::OnceLock<ExtensionTypeRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(ros_madair_handlers::default_registry)
}

/// Rebuild the registry the snapshot was EMITTED with, from its manifest's
/// `handlers` block. Raises `ValueError` — naming the datatype — if the snapshot
/// declares a handler this build cannot provide, rather than planning a query over
/// an index that never contained those fields.
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

/// A parsed, indexed resource-model graph: the schema queries compile against and
/// resources hydrate into. Construct once from the Arches resource-model export
/// JSON (a bare graph object or a `{"graph":[...]}` export) and reuse it.
#[pyclass]
pub struct Graph {
    inner: Arc<StaticGraph>,
}

#[pymethods]
impl Graph {
    /// Parse and index an Arches resource-model export. Raises `ValueError` if the
    /// text is not JSON, or not a graph.
    #[new]
    fn new(graph_json: String) -> PyResult<Self> {
        let value: serde_json::Value = serde_json::from_str(&graph_json)
            .map_err(|e| PyValueError::new_err(format!("graph JSON is not valid JSON: {e}")))?;
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
}

/// A Parquet snapshot reader: opens the tile-row Parquet (plus `edges_*.parquet`
/// and `concept_catalog.parquet` when present) under `dir`, and answers the
/// `Query` IR + hydrates through `ros-madair-duck`. `unsendable`: it holds a
/// DuckDB connection, accessed from the calling thread.
#[pyclass(unsendable)]
pub struct Reader {
    inner: DuckReader,
}

impl Reader {
    fn resolve_registry(manifest_json: &Option<String>) -> PyResult<Option<ExtensionTypeRegistry>> {
        match manifest_json {
            Some(text) => Ok(Some(registry_from_manifest(text)?)),
            None => Ok(None),
        }
    }
}

#[pymethods]
impl Reader {
    /// Open a snapshot directory: `tiles_*.parquet` (required), plus
    /// `edges_*.parquet` and `concept_catalog.parquet` if they are present.
    #[new]
    fn new(dir: String) -> PyResult<Self> {
        let dir = PathBuf::from(dir);
        let glob = format!("{}/tiles_*.parquet", dir.display());
        let mut inner =
            DuckReader::open(&glob).map_err(|e| PyValueError::new_err(e.to_string()))?;
        let catalog = dir.join("concept_catalog.parquet");
        if catalog.is_file() {
            inner = inner
                .with_catalog(&catalog.to_string_lossy())
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
        }
        Ok(Reader { inner })
    }

    /// Resolve the query IR to the matching resource ids. `manifest_json`
    /// (optional) plans with the registry the snapshot declares (see module docs).
    #[pyo3(signature = (ir_json, graph, manifest_json=None))]
    fn resolve(
        &self,
        ir_json: String,
        graph: PyRef<Graph>,
        manifest_json: Option<String>,
    ) -> PyResult<Vec<String>> {
        let query: Query = serde_json::from_str(&ir_json)
            .map_err(|e| PyValueError::new_err(format!("invalid query IR: {e}")))?;
        let from_manifest = Self::resolve_registry(&manifest_json)?;
        let registry = from_manifest.as_ref().unwrap_or_else(|| registry());
        self.inner
            .resolve_ids(&query, &graph.inner, registry)
            .map_err(|e| PyValueError::new_err(e.to_string()))
    }

    /// Count the resources matching the query IR.
    #[pyo3(signature = (ir_json, graph, manifest_json=None))]
    fn count(
        &self,
        ir_json: String,
        graph: PyRef<Graph>,
        manifest_json: Option<String>,
    ) -> PyResult<usize> {
        let query: Query = serde_json::from_str(&ir_json)
            .map_err(|e| PyValueError::new_err(format!("invalid query IR: {e}")))?;
        let from_manifest = Self::resolve_registry(&manifest_json)?;
        let registry = from_manifest.as_ref().unwrap_or_else(|| registry());
        self.inner
            .count_records(&query, &graph.inner, registry)
            .map_err(|e| PyValueError::new_err(e.to_string()))
    }

    /// A resource's tiles as a JSON array of `StaticTile` — feed straight to
    /// `alizarin.build_tree_from_tiles`.
    fn resource_tiles(&self, uuid: String) -> PyResult<String> {
        let tiles = self
            .inner
            .resource_tiles(&uuid)
            .map_err(|e| PyValueError::new_err(format!("read error: {e}")))?;
        serde_json::to_string(&tiles)
            .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))
    }

    /// Hydrate a resource into a schema-shaped JSON tree. `languages` defaults to
    /// `["en"]`.
    #[pyo3(signature = (uuid, graph, languages=None))]
    fn hydrate(
        &self,
        uuid: String,
        graph: PyRef<Graph>,
        languages: Option<Vec<String>>,
    ) -> PyResult<String> {
        let langs: Vec<&str> = languages
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect())
            .unwrap_or_else(|| vec!["en"]);
        let tree = self
            .inner
            .hydrate(&uuid, &graph.inner, &langs)
            .map_err(|e| PyValueError::new_err(format!("hydrate error: {e}")))?;
        serde_json::to_string(&tree)
            .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))
    }
}

#[pymodule]
fn ros_madair_v2(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Graph>()?;
    m.add_class::<Reader>()?;
    Ok(())
}
