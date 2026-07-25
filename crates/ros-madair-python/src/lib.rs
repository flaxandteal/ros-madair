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

use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::StaticGraph;
use alizarin_core::{GraphModelAccess, ResourceInstanceWrapperCore};
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

/// Hydrate a resource's tiles from a v2 head directory (fragment_dir + chunks),
/// returned as a JSON array of `StaticTile` — feed straight to
/// `alizarin.build_tree_from_tiles`. No graph needed: `resource_tiles` reads the
/// head's fragment_dir and the content-addressed chunks directly.
#[pyfunction]
fn hydrate_tiles(head_dir: String, uuid: String) -> PyResult<String> {
    let tiles = ros_madair_read::resource_tiles(std::path::Path::new(&head_dir), &uuid)
        .map_err(|e| PyValueError::new_err(format!("hydrate error: {e}")))?;
    serde_json::to_string(&tiles)
        .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))
}

/// Hydrate many resources in one call — opens the head once and shares a chunk
/// cache across the batch (a chunk touched by an earlier resource is not re-read).
/// Returns a JSON object mapping each found uuid to its `StaticTile` array;
/// missing uuids are omitted. Far cheaper than N `hydrate_tiles` calls, which
/// each reopen the head.
#[pyfunction]
fn hydrate_many(head_dir: String, uuids: Vec<String>) -> PyResult<String> {
    let pairs = ros_madair_read::hydrate_many(std::path::Path::new(&head_dir), &uuids)
        .map_err(|e| PyValueError::new_err(format!("hydrate error: {e}")))?;
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (uuid, tiles) in pairs {
        let val = serde_json::to_value(&tiles)
            .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))?;
        map.insert(uuid, val);
    }
    serde_json::to_string(&serde_json::Value::Object(map))
        .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))
}

/// [`hydrate_many`] restricted to specific node-group UUIDs — targeted
/// extraction. Reads only the chunks holding those nodegroups (a wide
/// resource's other nodegroups are never decoded) and returns only their tiles.
/// Empty `nodegroups` means all. Returns `{uuid: tiles}`.
#[pyfunction]
fn hydrate_nodegroups(
    head_dir: String,
    uuids: Vec<String>,
    nodegroups: Vec<String>,
) -> PyResult<String> {
    let pairs =
        ros_madair_read::hydrate_nodegroups(std::path::Path::new(&head_dir), &uuids, &nodegroups)
            .map_err(|e| PyValueError::new_err(format!("hydrate error: {e}")))?;
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (uuid, tiles) in pairs {
        let val = serde_json::to_value(&tiles)
            .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))?;
        map.insert(uuid, val);
    }
    serde_json::to_string(&serde_json::Value::Object(map))
        .map_err(|e| PyValueError::new_err(format!("serialize error: {e}")))
}

/// A lazily-hydrated resource from a v2 head — **Option C**: the resolve → fetch
/// → merge → read loop runs entirely IN RUST, inside this one cdylib (which links
/// BOTH `ros_madair_read` and `alizarin_core`). Touching a path fetches only that
/// path's nodegroup from the head, in-process — no Python round-trip and no
/// per-nodegroup JSON marshalling (tiles stay `Vec<StaticTile>` the whole way).
///
/// Hold one per resource and reuse it: it accumulates nodegroups as paths are
/// touched (the in-loop pattern), exactly what the Python-orchestrated B path did
/// across the cdylib wall, but without paying that wall's cost.
#[pyclass]
pub struct HeadResource {
    head_dir: PathBuf,
    uuid: String,
    inner: ResourceInstanceWrapperCore,
    model: GraphModelAccess,
    fully_loaded: bool,
}

#[pymethods]
impl HeadResource {
    /// Build from a head directory, a parsed [`Graph`], and a resource UUID. No
    /// tiles are loaded yet — they arrive per nodegroup, on demand.
    #[new]
    fn new(head_dir: String, graph: PyRef<Graph>, uuid: String) -> PyResult<Self> {
        let g = graph.inner.clone();
        let graph_id = g.graph_id().to_string();
        let model = GraphModelAccess::new_eager(g, true);
        let inner = ResourceInstanceWrapperCore::new(graph_id);
        Ok(HeadResource {
            head_dir: PathBuf::from(head_dir),
            uuid,
            inner,
            model,
            fully_loaded: false,
        })
    }

    /// The path's display value(s) as JSON `{"is_single": bool, "values": [...]}`.
    /// Lazily loads the nodegroup the path needs (single segment), or the whole
    /// resource once (nested path), before reading — all in Rust.
    fn get_values_at_path(&mut self, path: String) -> PyResult<String> {
        self.ensure_loaded(&path)?;
        let pl = self
            .inner
            .get_values_at_path(&path, &self.model, None)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let values: Vec<serde_json::Value> = pl
            .values
            .iter()
            .map(|v| v.serialize_display("en", None, None, None))
            .collect();
        let out = serde_json::json!({ "is_single": pl.is_single, "values": values });
        Ok(out.to_string())
    }

    /// Nodegroup UUIDs currently hydrated into this resource (for tests/inspection).
    fn loaded_nodegroups(&self) -> Vec<String> {
        self.inner.nodegroup_index.keys().cloned().collect()
    }

    /// Whether the whole resource has been hydrated (a nested-path fallback fired).
    fn is_fully_loaded(&self) -> bool {
        self.fully_loaded
    }
}

impl HeadResource {
    /// Ensure the tiles `path` needs are loaded. Single segment → fetch just that
    /// nodegroup from the head and merge it (lazy); nested path (or an unresolved
    /// one) → full hydrate, once. All fetching is `ros_madair_read`, in-process.
    fn ensure_loaded(&mut self, path: &str) -> PyResult<()> {
        if !path.contains('.') {
            if let Ok(info) = self.inner.resolve_path(path, &self.model) {
                let ng = info.nodegroup_id;
                if self.inner.is_nodegroup_loaded(&ng) {
                    return Ok(());
                }
                let pairs = ros_madair_read::hydrate_nodegroups(
                    &self.head_dir,
                    &[self.uuid.clone()],
                    &[ng.clone()],
                )
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
                if let Some((_, tiles)) = pairs.into_iter().next() {
                    self.inner.merge_tiles(tiles);
                }
                self.inner.mark_nodegroup_loaded(&ng);
                return Ok(());
            }
        }
        if self.fully_loaded {
            return Ok(());
        }
        let tiles = ros_madair_read::resource_tiles(&self.head_dir, &self.uuid)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        self.inner.merge_tiles(tiles);
        let ngs: Vec<String> = self.inner.nodegroup_index.keys().cloned().collect();
        for ng in ngs {
            self.inner.mark_nodegroup_loaded(&ng);
        }
        self.fully_loaded = true;
        Ok(())
    }
}

#[pymodule]
fn ros_madair_v2(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Graph>()?;
    m.add_class::<HeadResource>()?;
    m.add_function(wrap_pyfunction!(compile_query, m)?)?;
    m.add_function(wrap_pyfunction!(hydrate_tiles, m)?)?;
    m.add_function(wrap_pyfunction!(hydrate_many, m)?)?;
    m.add_function(wrap_pyfunction!(hydrate_nodegroups, m)?)?;
    Ok(())
}
