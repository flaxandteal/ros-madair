// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Multi-layer composition** (R1): read N ordered snapshots as one composed
//! view — a shipped BASE plus overlays that Gréasán emits **on-device** as the
//! user edits.
//!
//! # The premise, and everything that follows from it
//!
//! An overlay is emitted independently, on the device, long after the base
//! shipped. Re-emitting the base to share its dictionary is not an option —
//! the base is a downloaded artifact, not an input. So:
//!
//! > **LAYERS DO NOT SHARE A DICTIONARY.** Each head interns its own terms
//! > sequentially, so the same URI has DIFFERENT `term_id`s in different
//! > layers.
//!
//! (And it must stay that way: `term_id`s are the compression the whole head
//! is built on, and concepts are interned in closure DFS order precisely so a
//! subtree is a contiguous integer range — that is what makes `concept BETWEEN
//! dfs_enter AND dfs_leave` a range scan. Content-derived, cross-layer-stable
//! ids would destroy the interval hierarchy. The ids are local *by design*;
//! `tests/layers.rs` asserts they differ, as documentation.)
//!
//! Therefore:
//!
//! - **No query may join across layers on anything but the resource UUID
//!   string.** `dict.term` is the only cross-layer identity. Every `term_id`,
//!   `rid`, `chunk` and `concept` id is layer-local nonsense outside its layer.
//! - **Per-layer SQL is fine.** The compiled SQL never contains a `term_id`:
//!   it contains `(SELECT term_id FROM dict WHERE term = ?)`, which resolves
//!   against *whichever layer's dict* it runs in. The SAME compiled statement
//!   is therefore correct on EVERY layer. That is the load-bearing property of
//!   this module.
//!
//! # Precedence — RM principle P13
//!
//! Layers are **ordered, base first, later overrides earlier**. The rule, made
//! explicit rather than left to iteration order:
//!
//! > **The topmost layer that DEFINES a resource is authoritative for it.**
//! > "Defines" = the resource has a row in that layer's spine.
//!
//! A base verdict on a resource an overlay has redefined is *stale*, and is
//! discarded — not unioned. This matters: an overlay can flip a base resource
//! **into** a filter (edit `type` to `Church`) or **out of** one (edit `Church`
//! to `Chapel`). A composed query that merely unioned the per-layer matches
//! would keep counting the flipped-out resource forever.
//!
//! ## LAYERS ARE PARTIAL, and the index does not yet know it — READ THIS
//!
//! An earlier draft of this module asserted a contract: *"an overlay that
//! carries a resource carries it IN FULL."* **That contract is false and has
//! been withdrawn.** Layers are inherently partial — a layer that adds an
//! etymology to 180,000 resources cannot restate 180,000 resources, and
//! requiring it to would defeat the point of layering.
//!
//! [`Layers::resource_tiles`] is correct under partial layers: alizarin's
//! unifier merges cardinality-1 tiles **per node**, so an overlay that sets one
//! field keeps the base's other fields in the same nodegroup.
//!
//! **The QUERY path is not yet correct under partial layers, and this is the
//! open R1 issue.** P13 below says the topmost *defining* layer is authoritative,
//! and evaluates the filter against that layer's index alone. But a partial
//! overlay's head only ever indexed the tiles the overlay carried, so:
//!
//! - a filter on a field the overlay does **not** carry answers "no match" in
//!   the overlay, and P13 then *discards* the base's correct verdict;
//! - an AND across two fields defined in **different** layers matches in neither,
//!   though the composed resource the user sees satisfies both.
//!
//! So query and hydration can disagree about the same resource. Until that is
//! fixed, the safe stack is one where each filtered field is carried by exactly
//! one layer, or where overlays *do* happen to restate what they touch (which an
//! on-device edit naturally does). Do not read P13 as settled.
//!
//! # Costs, honestly
//!
//! A layered count costs more than a single-layer count; see [`Layers::count`].

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::{
    merge_resources, unify_cardinality_one_tiles, StaticGraph, StaticResource,
    StaticResourceMetadata,
};
use alizarin_core::StaticTile;
use ros_madair_format::Manifest;
use ros_madair_query::{
    compile_layered_count, compile_match_probe, compile_with_registry, CompiledStatement, Measure,
    Param, Query,
};
use rusqlite::{types::Value as SqlValue, Connection, OpenFlags};

use crate::{hydrate_tiles, open_head, resource_tiles_with_graph, ReadError};

/// One layer: a head directory and its (version-checked) manifest.
#[derive(Debug, Clone)]
pub struct Layer {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// An ordered stack of snapshots read as one composed view. **Base first.**
///
/// See the module docs for the no-shared-dictionary premise and the P13
/// precedence rule; both are load-bearing, not commentary.
#[derive(Debug, Clone)]
pub struct Layers {
    layers: Vec<Layer>,
}

impl Layers {
    /// Open an ordered layer stack, **base first**, and check that the layers
    /// are mutually composable.
    ///
    /// A manifest is REQUIRED per layer (unlike the single-snapshot read path,
    /// where it is an optional fast path): without it there is nothing to check
    /// composability against, and silently composing two incompatible snapshots
    /// is the failure mode this whole crate exists to prevent.
    ///
    /// Checks:
    /// - **base_uri** — layers minting URIs under different bases are not the
    ///   same corpus;
    /// - **handler set** — a layer emitted without a handler never indexed the
    ///   fields that handler owns, so the identical query would answer zero
    ///   rows there and non-zero here, with nothing to show for it;
    /// - **per-model spine table** — a shared `graph_id` must map to the same
    ///   spine table in every layer (the compiled SQL names it once);
    /// - **per-field class/storage** — a shared alias whose class differs
    ///   between layers is head-indexed in one and detail-only in the other:
    ///   same SQL, silently different meaning.
    ///
    /// Layers need not carry the same *set* of models: an overlay may carry a
    /// subset (typically it carries only what was edited). Models absent from a
    /// layer are simply not defined there.
    pub fn open(dirs: &[&Path]) -> Result<Layers, ReadError> {
        if dirs.is_empty() {
            return Err(ReadError::NoLayers);
        }
        let mut layers = Vec::with_capacity(dirs.len());
        for dir in dirs {
            let path = dir.join("manifest.json");
            if !path.is_file() {
                return Err(ReadError::MissingManifest(dir.to_path_buf()));
            }
            let manifest: Manifest = serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|source| ReadError::Manifest { path, source })?;
            layers.push(Layer {
                dir: dir.to_path_buf(),
                manifest,
            });
        }
        let base = &layers[0];
        for layer in &layers[1..] {
            check_composable(base, layer)?;
        }
        Ok(Layers { layers })
    }

    /// The layers, base first.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Indices of the layers that carry `graph` at all — a layer whose manifest
    /// does not name the model has no spine table for it and must not be
    /// queried (the SQL would name a table that does not exist).
    fn model_layers(&self, graph: &StaticGraph) -> Vec<usize> {
        self.layers
            .iter()
            .enumerate()
            .filter(|(_, l)| l.manifest.model_for_graph(graph.graph_id()).is_some())
            .map(|(i, _)| i)
            .collect()
    }

    fn spine_table(&self, index: usize, graph: &StaticGraph) -> Option<&str> {
        self.layers[index]
            .manifest
            .model_for_graph(graph.graph_id())
            .map(|m| m.spine_table.as_str())
    }

    /// Every resource UUID this layer **defines** for `graph` — i.e. its spine,
    /// resolved through its own dict.
    ///
    /// This is the precedence primitive: "defined in a higher layer" is what
    /// makes a lower layer's verdict stale. Overlays are small (an on-device
    /// edit touches a handful of resources), so enumerating an overlay's
    /// defined set is cheap — which is exactly what makes the corrected count in
    /// [`Layers::count`] cost O(overlay), not O(corpus). Enumerating the BASE's
    /// defined set is NOT cheap, and nothing here ever does.
    pub fn defined_uuids(
        &self,
        index: usize,
        graph: &StaticGraph,
    ) -> Result<BTreeSet<String>, ReadError> {
        let Some(spine) = self.spine_table(index, graph) else {
            return Ok(BTreeSet::new());
        };
        let conn = open_head(&self.layers[index].dir)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT d.term FROM {spine} s JOIN dict d ON d.term_id = s.term_id"
        ))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The union of the defined sets of every layer ABOVE `index` (only layers
    /// carrying this model contribute). Bounded by the overlays' sizes.
    fn defined_above(
        &self,
        index: usize,
        model_layers: &[usize],
        graph: &StaticGraph,
    ) -> Result<HashSet<String>, ReadError> {
        let mut set = HashSet::new();
        for &i in model_layers.iter().filter(|&&i| i > index) {
            set.extend(self.defined_uuids(i, graph)?);
        }
        Ok(set)
    }

    /// Resolve a query to the resource UUIDs matching it in the **composed**
    /// view: run the same compiled SQL against EVERY layer, then apply P13 —
    /// a layer's hit survives only if no HIGHER layer defines that resource.
    ///
    /// Ordering is deterministic: layer order (base first), and within a layer
    /// the order the SQL returned (rid order).
    ///
    /// # The `limit`, precisely
    ///
    /// `query.limit` is applied **per layer** and the composed result is then
    /// truncated to it. A layered query whose base alone fills the limit can
    /// therefore miss overlay matches beyond that window — the same truncation
    /// hazard a single-layer `limit` already has, one layer per copy. Raise the
    /// limit if you are composing and counting on completeness (or use
    /// [`Layers::count`], which never truncates).
    ///
    /// `query.measures` is IGNORED: this executes an ids plan by construction,
    /// because a count plan returns a number and numbers cannot be
    /// precedence-filtered.
    pub fn resolve(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<Vec<String>, ReadError> {
        let limit = query
            .limit
            .unwrap_or(ros_madair_query::DEFAULT_SELECT_LIMIT) as usize;
        let ids = self.resolve_unlimited(query, graph, registry)?;
        Ok(ids.into_iter().take(limit).collect())
    }

    /// [`Layers::resolve`] without the row cap — the oracle the counts are
    /// checked against, and the honest thing to call when you need the whole
    /// composed set.
    fn resolve_unlimited(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<Vec<String>, ReadError> {
        let model_layers = self.model_layers(graph);
        if model_layers.is_empty() {
            return Err(ReadError::ModelInNoLayer(graph.graph_id().to_string()));
        }
        // ONE compile: the SQL is dictionary-agnostic (it resolves terms via
        // `(SELECT term_id FROM dict WHERE term = ?)`), so the same text is
        // correct in every layer. Nothing about this statement is layer-bound.
        let stmt = self.compile_ids(query, graph, registry)?;

        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for &i in &model_layers {
            let above = self.defined_above(i, &model_layers, graph)?;
            let conn = open_head(&self.layers[i].dir)?;
            for uuid in select_ids(&conn, &stmt)? {
                // P13: a higher layer redefines this resource, so THIS layer's
                // verdict on it is stale — the higher layer's own arm decides.
                if above.contains(&uuid) {
                    continue;
                }
                if seen.insert(uuid.clone()) {
                    out.push(uuid);
                }
            }
        }
        Ok(out)
    }

    /// Compile the ids plan once, forcing `measures = [select_ids]` and lifting
    /// the row cap to "no cap" for the internal, un-truncated resolve.
    fn compile_ids(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<CompiledStatement, ReadError> {
        let ids_query = Query {
            measures: vec![Measure::SelectIds],
            limit: Some(query.limit.unwrap_or(u32::MAX)),
            ..query.clone()
        };
        let mut stmts = compile_with_registry(&ids_query, graph, registry).map_err(ReadError::Query)?;
        Ok(stmts.remove(0))
    }

    /// `|composed match set|` — a count of the composed view, **never** a sum
    /// of per-layer counts.
    ///
    /// # Why the obvious things are wrong
    ///
    /// - `Σ count_i` double-counts every resource present in two layers.
    /// - "each layer counts only what it originates, then sum" is subtly wrong:
    ///   origination is stable across layers, **match status is not**. Base has
    ///   `B: type = Church`; the overlay overrides `B` to `Chapel`. Counting
    ///   `type = Church`: the base originated `B` so it counts it; the overlay
    ///   originates nothing so it counts nothing — and the total wrongly
    ///   includes `B`, which is a Chapel in the composed view. Silent off-by-N.
    ///
    /// # The plan actually used — O(overlay), not O(result)
    ///
    /// ```text
    /// total = count(F, base)                       // the single-layer FAST path survives
    ///       - |{u ∈ redefined-above : base said u matched F}|   // one indexed probe each
    ///       + Σ_{overlays} |{u ∈ matches(F, overlay) : u not redefined above overlay}|
    /// ```
    ///
    /// The base keeps its point-lookup count (`rollup_concept_counts`, or the
    /// index-only `COUNT(DISTINCT rid)`) — the thing a naive union destroys. The
    /// corrections are bounded by the OVERLAY sizes: an on-device overlay holds
    /// a handful of resources, so this is a base count plus a handful of probes,
    /// not a haul of every matching UUID in the corpus. (On a 209K corpus with a
    /// filter matching 15,773 resources, the union path drags 15,773 UUIDs out
    /// of the base to de-duplicate them against an overlay of five. This does
    /// not.) It degrades gracefully: an overlay the size of the corpus makes
    /// this no worse than the union.
    ///
    /// Still: a layered count costs strictly more than a single-layer count —
    /// one extra id-scan per overlay, plus one probe per overridden resource.
    /// That is the price of correctness, and it is paid per overlay.
    ///
    /// [`Layers::count_by_union`] is the same number computed the slow, obvious
    /// way; the tests assert they agree, and the union is the oracle.
    pub fn count(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<usize, ReadError> {
        let model_layers = self.model_layers(graph);
        if model_layers.is_empty() {
            return Err(ReadError::ModelInNoLayer(graph.graph_id().to_string()));
        }
        // Single layer: this IS a single-layer count. Fast paths intact.
        let count_query = Query {
            measures: vec![Measure::CountRecords],
            ..query.clone()
        };
        let count_stmt = {
            let mut s = compile_with_registry(&count_query, graph, registry)
                .map_err(ReadError::Query)?;
            s.remove(0)
        };
        let base = model_layers[0];
        let base_conn = open_head(&self.layers[base].dir)?;
        let mut total = select_count(&base_conn, &count_stmt)?;
        if model_layers.len() == 1 {
            return Ok(total);
        }

        // Correction 1: the base's verdict on every resource an overlay
        // redefines is stale. Probe the base for each (indexed point lookup)
        // and subtract the ones the base had counted.
        let above_base = self.defined_above(base, &model_layers, graph)?;
        if !above_base.is_empty() {
            let probe = compile_match_probe(query, graph, registry).map_err(ReadError::Query)?;
            let mut prepared = base_conn.prepare(&probe.sql)?;
            for uuid in &above_base {
                let mut bound = bind(&probe.params);
                bound.push(SqlValue::Text(uuid.clone()));
                let matched: i64 = prepared
                    .query_row(rusqlite::params_from_iter(bound.iter()), |r| r.get(0))?;
                if matched != 0 {
                    total -= 1;
                }
            }
        }

        // Correction 2: each overlay contributes its own matches, minus any
        // resource a still-higher layer redefines. Overlays are small: an ids
        // scan over one is cheap, and no probe is needed.
        let ids_stmt = self.compile_ids(query, graph, registry)?;
        for &i in &model_layers[1..] {
            let above = self.defined_above(i, &model_layers, graph)?;
            let conn = open_head(&self.layers[i].dir)?;
            for uuid in select_ids(&conn, &ids_stmt)? {
                if !above.contains(&uuid) {
                    total += 1;
                }
            }
        }
        Ok(total)
    }

    /// The count computed the obvious way: resolve the composed id set and take
    /// its size. O(result size) — it drags every matching UUID out of the base.
    ///
    /// This is the **oracle**: it is the definition of the answer, and
    /// [`Layers::count`] is an optimisation of it. Kept public because a caller
    /// that already wants the ids should not pay for a second plan, and because
    /// a cross-check against the fast path is worth having.
    pub fn count_by_union(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<usize, ReadError> {
        // No row cap: a count must not be truncated by `limit`.
        let unlimited = Query {
            limit: Some(u32::MAX),
            ..query.clone()
        };
        Ok(self.resolve_unlimited(&unlimited, graph, registry)?.len())
    }

    /// The count computed **in SQL**, over the layers ATTACHed into one
    /// connection: `COUNT(DISTINCT uuid)` over a `UNION ALL` of per-layer arms,
    /// each arm anti-joined (on the UUID **string**) against the dictionaries of
    /// the layers above it. See [`ros_madair_query::compile_layered_count`].
    ///
    /// Nothing here joins on a `term_id` across a layer — each arm resolves its
    /// terms in its own schema's `dict`, and the only cross-schema comparison is
    /// `dd.term = d.term`, a string.
    ///
    /// Cost is O(result size), like [`Layers::count_by_union`], but SQLite does
    /// the de-duplication and the ids never enter Rust. Offered as the
    /// single-connection path and as a second cross-check.
    ///
    /// **Not the default**, for two reasons: it forfeits the base's point-lookup
    /// count (see [`Layers::count`]), and ATTACH under browser wa-sqlite is
    /// UNPROVEN — the spike was never run — so the native path must not become
    /// the only path.
    pub fn count_by_attached_sql(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<usize, ReadError> {
        let model_layers = self.model_layers(graph);
        if model_layers.is_empty() {
            return Err(ReadError::ModelInNoLayer(graph.graph_id().to_string()));
        }
        // A scratch in-memory main, with every layer ATTACHed read-only beside
        // it: no layer is "main", so no layer is privileged or writable.
        let conn = Connection::open_with_flags(
            ":memory:",
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.execute_batch("PRAGMA query_only = 1;")?;
        let names: Vec<String> = model_layers.iter().map(|i| format!("l{i}")).collect();
        for (name, &i) in names.iter().zip(&model_layers) {
            let path = self.layers[i].dir.join("head.sqlite");
            // `mode=ro` needs URI filenames, which ATTACH honours because the
            // connection was opened with SQLITE_OPEN_URI.
            let uri = format!("file:{}?mode=ro", path.display());
            conn.execute(&format!("ATTACH DATABASE ?1 AS {name}"), [uri])?;
        }
        let schemas: Vec<&str> = names.iter().map(String::as_str).collect();
        let stmt = compile_layered_count(query, graph, registry, &schemas)
            .map_err(ReadError::Query)?;
        select_count(&conn, &stmt)
    }

    /// The tiles of one resource in the **composed** view: gathered from every
    /// layer that has it, merged with precedence (topmost layer wins).
    ///
    /// # This does not implement a merge. Alizarin already has one.
    ///
    /// `merge_resources` + `unify_cardinality_one_tiles` were written to merge
    /// **co-equal shards** of a resource across business-data files, and they
    /// turn out to be exactly layer composition — the same problem. So this
    /// gathers the resource from each layer, hands the stack to alizarin
    /// **topmost-first**, and gets the semantics back:
    ///
    /// - **cardinality-1** — the unifier groups tiles by scope
    ///   `(nodegroup_id, parenttile_id)`, reads the nodegroup's cardinality out
    ///   of the *graph*, and collapses each over-full scope into the FIRST tile,
    ///   merging the others' data into it **per key**. Topmost-first therefore
    ///   means: **the topmost layer wins each node it sets, and lower layers
    ///   fill in the nodes it omits.** This is a *field-level* override, which is
    ///   strictly better than the whole-nodegroup replace this function used to
    ///   do — a thin overlay that sets one node no longer blanks the rest of its
    ///   nodegroup. **Layers are inherently partial** (an overlay adding
    ///   etymology to 180k resources cannot restate them), so field-level is the
    ///   only correct rule.
    /// - **cardinality-n** — left alone, so the concatenation stands: **additive
    ///   union**. Nothing here can silently replace a multi-valued tile.
    /// - **children of a superseded cardinality-1 tile** — the unifier re-parents
    ///   them onto the surviving tile (`tile_redirect`), so a cardinality-n
    ///   nodegroup hanging off an overridden cardinality-1 parent **merges rather
    ///   than vanishing**.
    ///
    /// It collapses on **cardinality**, not on `tileid`, so it does not depend on
    /// composable tile ids: two layers whose cardinality-1 tiles carry different
    /// (e.g. Arches-minted) ids are still unified. Canonical tile ids make the
    /// dedup cheaper and work without a graph; the graph is the safety net.
    ///
    /// **There is still no retraction.** An overlay cannot delete a base tile
    /// from a cardinality-n nodegroup — the format has no tombstone.
    ///
    /// `graph` is load-bearing, not a hint: the unifier needs the cardinalities.
    ///
    /// Descriptors are NOT recomputed here. A head carries no resource
    /// descriptors (hydration synthesises the identity fields it needs), so
    /// there is nothing stale to rebuild on this path — and a corpus-wide
    /// descriptor rebuild is the expensive operation this design exists to avoid.
    /// Descriptors are computed at *emit*, by the layer that defines the
    /// resource, over its own composed view.
    pub fn resource_tiles(
        &self,
        uuid: &str,
        graph: &StaticGraph,
    ) -> Result<Vec<StaticTile>, ReadError> {
        // Gather the resource from every layer that has it, TOPMOST FIRST —
        // which is what turns alizarin's "first occurrence wins" into
        // "topmost layer wins".
        let mut stack: Vec<StaticResource> = Vec::new();
        for layer in self.layers.iter().rev() {
            let tiles = match resource_tiles_with_graph(&layer.dir, uuid, Some(graph)) {
                Ok(tiles) => tiles,
                // Not in this layer: the normal case for an overlay.
                Err(ReadError::UnknownResource(_)) => continue,
                Err(e) => return Err(e),
            };
            if tiles.is_empty() {
                continue;
            }
            stack.push(as_resource(uuid, graph, tiles));
        }
        if stack.is_empty() {
            return Err(ReadError::UnknownResource(uuid.to_string()));
        }

        // Concatenate + dedup by tileid (cardinality-1 tiles minted by alizarin
        // collide by design across layers; cardinality-n cannot).
        let merged = merge_resources(stack).map_err(ReadError::Merge)?;
        let mut tiles = merged.resource.tiles.unwrap_or_default();

        // Then collapse any cardinality-1 scope that still holds more than one
        // tile — the case tile ids alone cannot catch (Arches-minted ids, or a
        // layer that recreated the tile). `strict: false`: a data conflict
        // between layers is what an override IS, so it warns rather than fails;
        // the topmost layer's value is the one kept.
        unify_cardinality_one_tiles(&mut tiles, graph, false).map_err(ReadError::Merge)?;
        Ok(tiles)
    }

    /// Hydrate one resource from the composed view: [`Layers::resource_tiles`]
    /// (gather from every layer, merge with precedence) then a single
    /// `alizarin_core::resource_tiles_to_tree` over the merged tiles. The
    /// hydrator never learns there were layers — it is handed one resource's
    /// tiles, as always.
    pub fn hydrate_resource(
        &self,
        uuid: &str,
        graph: &StaticGraph,
    ) -> Result<serde_json::Value, ReadError> {
        let tiles = self.resource_tiles(uuid, graph)?;
        hydrate_tiles(&tiles, uuid, graph)
    }
}

/// Wrap one layer's tiles as a `StaticResource` so alizarin's merge can take
/// them. A head carries no resource metadata, so only the identity fields are
/// real; everything merge_resources would merge *besides tiles* (metadata,
/// cache, scopes, descriptors) is empty here by construction, and so cannot be
/// clobbered by a thin overlay. That is why this path needs no per-field
/// precedence rules — it has no fields to rank.
fn as_resource(uuid: &str, graph: &StaticGraph, tiles: Vec<StaticTile>) -> StaticResource {
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

fn bind(params: &[Param]) -> Vec<SqlValue> {
    params
        .iter()
        .map(|p| match p {
            Param::Text(s) => SqlValue::Text(s.clone()),
            Param::Int(i) => SqlValue::Integer(*i),
        })
        .collect()
}

fn select_ids(conn: &Connection, stmt: &CompiledStatement) -> Result<Vec<String>, ReadError> {
    let bound = bind(&stmt.params);
    let mut prepared = conn.prepare(&stmt.sql)?;
    let rows = prepared.query_map(rusqlite::params_from_iter(bound.iter()), |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<String>, _>>()?)
}

fn select_count(conn: &Connection, stmt: &CompiledStatement) -> Result<usize, ReadError> {
    let bound = bind(&stmt.params);
    let n: i64 = conn.query_row(&stmt.sql, rusqlite::params_from_iter(bound.iter()), |r| {
        r.get(0)
    })?;
    Ok(n.max(0) as usize)
}

/// The composability checks named in [`Layers::open`].
fn check_composable(base: &Layer, layer: &Layer) -> Result<(), ReadError> {
    let incompatible = |what: &str, a: String, b: String| {
        Err(ReadError::Incompatible {
            base: base.dir.clone(),
            layer: layer.dir.clone(),
            what: what.to_string(),
            base_value: a,
            layer_value: b,
        })
    };
    if base.manifest.base_uri != layer.manifest.base_uri {
        return incompatible(
            "base_uri",
            base.manifest.base_uri.clone(),
            layer.manifest.base_uri.clone(),
        );
    }
    // Handler sets must be identical: a field indexed under a handler the other
    // layer lacks is a field that answers zero rows there, silently.
    let handlers = |m: &Manifest| {
        let mut names: Vec<String> = m
            .handlers
            .iter()
            .map(|h| serde_json::to_string(h).unwrap_or_default())
            .collect();
        names.sort();
        names
    };
    let (hb, hl) = (handlers(&base.manifest), handlers(&layer.manifest));
    if hb != hl {
        return incompatible("handlers", hb.join(","), hl.join(","));
    }
    for model in &layer.manifest.models {
        let Some(base_model) = base.manifest.model_for_graph(&model.graph_id) else {
            // A model the base does not carry at all is fine — the overlay is
            // simply the only layer defining it.
            continue;
        };
        if base_model.spine_table != model.spine_table {
            return incompatible(
                &format!("spine_table for model {}", model.graph_id),
                base_model.spine_table.clone(),
                model.spine_table.clone(),
            );
        }
        for (alias, field) in &model.fields {
            let Some(base_field) = base_model.fields.get(alias) else {
                continue;
            };
            if base_field.class != field.class || base_field.storage != field.storage {
                return incompatible(
                    &format!("field class/storage for '{alias}'"),
                    format!("{}/{}", base_field.class, base_field.storage),
                    format!("{}/{}", field.class, field.storage),
                );
            }
        }
    }
    Ok(())
}
