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
//! > **The topmost layer that defines a NODEGROUP is authoritative for it.**
//! > "Defines" = the layer carries a tile for it — a `fragment_dir` row —
//! > *including an empty tile*, which retracts rather than abstains.
//!
//! A base verdict on a nodegroup an overlay has redefined is *stale*, and is
//! discarded — not unioned. This matters: an overlay can flip a base resource
//! **into** a filter (edit `type` to `Church`) or **out of** one (edit `Church`
//! to `Chapel`). A composed query that merely unioned the per-layer matches would
//! keep counting the flipped-out resource forever.
//!
//! Read the next section before assuming this is the resource-level rule with
//! extra words. It is not, and the resource-level version was wrong.
//!
//! ## The unit of override is the NODEGROUP, by design
//!
//! An earlier draft asserted a contract: *"an overlay that carries a resource
//! carries it IN FULL."* **It was false, and it is withdrawn.** Layers are
//! inherently partial — a layer that adds an etymology to 180,000 resources
//! cannot restate 180,000 resources, and requiring it to would defeat the point
//! of layering.
//!
//! But a partial *resource* is not the same as a partial *nodegroup*. A nodegroup
//! is the atomic unit of information a layer adds or replaces — the same unit
//! Arches edits as a card — and the composition rule is built on that:
//!
//! > **The topmost layer that defines a NODEGROUP owns it, whole.**
//! > Not the resource (too coarse — a thin overlay would shadow untouched cards),
//! > and not the individual node (needlessly fine, and it would force a per-node
//! > presence index the nodegroup grain gets for free from `fragment_dir`).
//!
//! Under the discarded resource-level rule, a partial overlay that carried a
//! resource became authoritative for *every field of it*, so a filter on a card
//! the overlay never touched answered "no" and the base's verdict was lost, and an
//! `all` across cards owned by different layers matched in neither. The
//! nodegroup rule fixes both: each card is answered by its owner.
//!
//! The cost of the nodegroup grain is a **modelling contract**: an overlay must
//! restate a whole card, never a subset of one — which is what an on-device card
//! edit produces naturally, and what a well-structured enrichment layer does by
//! adding its *own* card rather than injecting a field into someone else's. A
//! partial card in an overlay silently blanks the base's other fields in it; emit
//! can lint that, but the schema is where it is prevented.
//!
//! Query and hydration answer from the *same* rule — whole-nodegroup override,
//! [`TileMergeMode::PerNodegroup`] — so they cannot disagree. `tests/layers.rs`
//! pins it by evaluating filters against composed HYDRATED tiles and demanding the
//! same answer, an oracle that shares no code with the index path.
//!
//! Presence needs no dedicated table: a `fragment_dir` `(rid, nodegroup)` row —
//! which every emitted tile already produces — is exactly "this layer carries this
//! nodegroup". It is the only way to tell **"this layer carries it, empty"** from
//! **"this layer never mentioned it"**; both are otherwise an absent row, and they
//! mean opposite things, which is why a retraction ships an *empty* tile.
//!
//! # Costs, honestly
//!
//! A layered count costs more than a single-layer count; see [`Layers::count`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::graph::{
    merge_resources, unify_cardinality_one_tiles, StaticGraph, StaticResource,
    StaticResourceMetadata, TileMergeMode,
};
use alizarin_core::StaticTile;
use ros_madair_format::Manifest;
use ros_madair_query::{
    compile_match_probe, compile_with_registry, CompiledStatement, Expr, Measure, Param, Query,
};
use rusqlite::{types::Value as SqlValue, Connection};

use crate::{hydrate_tiles, open_head, resource_tiles_with_graph, ReadError};

/// A filter tree whose leaves know which NODEGROUP they read and carry a compiled
/// one-layer probe.
///
/// It mirrors [`Expr`] rather than reusing it because the whole point is the
/// extra per-leaf information: composition resolves each leaf against the topmost
/// layer that defines *that leaf's nodegroup*, so the nodegroup id has to survive
/// down to the leaf. A per-query nodegroup would not do — two leaves of one filter
/// routinely read nodegroups owned by different layers.
enum Plan {
    All(Vec<Plan>),
    Any(Vec<Plan>),
    Not(Box<Plan>),
    Leaf {
        /// The nodegroup this leaf's node belongs to — the granularity precedence
        /// is decided at (see [`Layers::defines_nodegroup`]).
        nodegroup_id: String,
        /// Does this leaf's nodegroup **accumulate** across layers?
        ///
        /// Cardinality-1 is an OVERRIDE: the topmost layer that defines the
        /// nodegroup replaces the whole thing below it. Cardinality-n is ADDITIVE:
        /// the merge leaves every layer's tiles standing, so the composed value
        /// set is the UNION across layers — and a predicate matches if ANY layer's
        /// values match, including a layer that some higher layer also wrote to.
        ///
        /// Two different rules, and using the override rule on an additive
        /// nodegroup silently DROPS the lower layers' values, which the hydrator
        /// is still showing. The cardinality comes from the graph, which is the
        /// same place the tile merge reads it from — so the two cannot drift.
        additive: bool,
        probe: CompiledStatement,
    },
}

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
    ///   spine table in every layer (the compiled SQL names it once).
    ///
    /// A field's class is NOT checked: it is a pure function of datatype and the
    /// layers share one graph, so it cannot differ between them.
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

    /// Every resource ANY overlay carries: the **candidate set**.
    ///
    /// A resource no overlay touches cannot have been changed by one, so the
    /// base's verdict on it stands and it never needs composing. Everything
    /// expensive below is therefore bounded by the OVERLAYS, not by the corpus —
    /// an on-device edit touches a handful of resources, and this is the set that
    /// makes the corrected count O(overlay) rather than O(corpus).
    fn candidates(
        &self,
        model_layers: &[usize],
        graph: &StaticGraph,
    ) -> Result<BTreeSet<String>, ReadError> {
        let mut set = BTreeSet::new();
        for &i in model_layers.iter().skip(1) {
            set.extend(self.defined_uuids(i, graph)?);
        }
        Ok(set)
    }

    /// Does layer `index` carry `nodegroup` on `uuid`?
    ///
    /// **This is the precedence primitive.** Precedence between layers is
    /// per-NODEGROUP, not per-node and not per-resource, because a nodegroup is
    /// the atomic unit a layer adds or replaces (see the module docs). So a
    /// composed filter on a node is answered by the topmost layer that carries
    /// that node's *nodegroup*, and lower layers are shadowed for it.
    ///
    /// `fragment_dir` already answers this — a `(rid, nodegroup)` row means the
    /// layer emitted a tile for that nodegroup — so no dedicated presence table
    /// is needed. Without such a row, "this layer carries the nodegroup, empty"
    /// and "this layer never mentioned it" would be the same observation, and
    /// they have opposite meanings — which is why a RETRACTION ships an *empty*
    /// tile: an empty tile still produces a `fragment_dir` row.
    fn defines_nodegroup(
        &self,
        index: usize,
        graph: &StaticGraph,
        uuid: &str,
        nodegroup_id: &str,
    ) -> Result<bool, ReadError> {
        let Some(spine) = self.spine_table(index, graph) else {
            return Ok(false);
        };
        let conn = open_head(&self.layers[index].dir)?;
        // One indexed point lookup: dict.term is UNIQUE, the spine is keyed by
        // term_id, and fragment_dir is indexed on (rid, nodegroup).
        let found: i64 = conn.query_row(
            &format!(
                "SELECT EXISTS (
                     SELECT 1 FROM fragment_dir fd
                       JOIN {spine} s ON s.rid = fd.rid
                       JOIN dict dr ON dr.term_id = s.term_id
                       JOIN dict dn ON dn.term_id = fd.nodegroup
                      WHERE dr.term = ?1 AND dn.term = ?2)"
            ),
            (uuid, nodegroup_id),
            |r| r.get(0),
        )?;
        Ok(found != 0)
    }

    /// A filter tree with each leaf compiled to a one-layer probe, and told which
    /// NODEGROUP it reads.
    ///
    /// Compiled once per query and reused across every candidate: the probe SQL
    /// is dictionary-agnostic (it resolves terms via `(SELECT term_id FROM dict
    /// WHERE term = ?)`), so the SAME statement is correct in every layer. That
    /// is what lets one plan be evaluated against whichever layer turns out to own
    /// each leaf's nodegroup.
    fn plan(
        &self,
        query: &Query,
        expr: &Expr,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<Plan, ReadError> {
        let sub = |e: &Expr| self.plan(query, e, graph, registry);
        Ok(match expr {
            Expr::All(v) => Plan::All(v.iter().map(sub).collect::<Result<_, _>>()?),
            Expr::Any(v) => Plan::Any(v.iter().map(sub).collect::<Result<_, _>>()?),
            Expr::Not(x) => Plan::Not(Box::new(sub(x)?)),
            leaf => {
                let node_id = ros_madair_query::leaf_node_id(leaf, graph, registry)
                    .map_err(ReadError::Query)?
                    .expect("a non-boolean Expr always reads a node");
                let (nodegroup_id, additive) = nodegroup_of(graph, &node_id);
                let one = Query {
                    r#where: Some(leaf.clone()),
                    measures: vec![Measure::CountRecords],
                    ..query.clone()
                };
                let probe = compile_match_probe(&one, graph, registry).map_err(ReadError::Query)?;
                Plan::Leaf {
                    nodegroup_id,
                    additive,
                    probe,
                }
            }
        })
    }

    /// Evaluate the filter for ONE resource in the composed view.
    ///
    /// Each leaf is answered by **the topmost layer that defines that leaf's
    /// NODEGROUP** — not the topmost layer that defines the resource, and not the
    /// topmost that defines the individual node. A nodegroup is the atomic unit a
    /// layer adds or replaces (module docs), so:
    ///
    /// - a filter on a nodegroup the overlay does not carry reads from the layer
    ///   below, which still owns it;
    /// - an `all` across two nodegroups owned by DIFFERENT layers matches when the
    ///   composed resource satisfies both, each conjunct answered by its owner.
    ///
    /// A leaf whose nodegroup no layer defines is **false**: the resource has no
    /// value for it, so it cannot match. (No probe issued — the answer is known.)
    ///
    /// # Override vs ADDITIVE — two rules, chosen by cardinality
    ///
    /// "Topmost defining layer wins" is the **cardinality-1** rule. A
    /// **cardinality-n** nodegroup is never collapsed by the merge — every layer's
    /// tiles survive — so the composed value set is the UNION across layers, and
    /// the predicate matches if ANY layer's values match. Applying the override
    /// rule there would DROP the base's values while the hydrator went on showing
    /// them. So cardinality-n ORs the probe across every layer and does not consult
    /// presence at all: nothing is overridden, so nothing is authoritative, and
    /// there is no retraction (the id space makes multi-valued tiles add-only).
    fn matches(
        &self,
        plan: &Plan,
        uuid: &str,
        model_layers: &[usize],
        graph: &StaticGraph,
    ) -> Result<bool, ReadError> {
        match plan {
            Plan::All(v) => {
                for p in v {
                    if !self.matches(p, uuid, model_layers, graph)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Plan::Any(v) => {
                for p in v {
                    if self.matches(p, uuid, model_layers, graph)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Plan::Not(x) => Ok(!self.matches(x, uuid, model_layers, graph)?),
            // ADDITIVE (cardinality-n): the merge keeps every layer's tiles, so
            // the composed values are the union. Match if ANY layer matches.
            Plan::Leaf {
                additive: true,
                probe,
                ..
            } => {
                for &i in model_layers.iter() {
                    if self.probe(i, probe, uuid)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            // OVERRIDE (cardinality-1): the topmost layer that defines the
            // NODEGROUP replaces what is below it, and is the only one asked.
            Plan::Leaf {
                nodegroup_id,
                probe,
                ..
            } => {
                for &i in model_layers.iter().rev() {
                    if self.defines_nodegroup(i, graph, uuid, nodegroup_id)? {
                        return self.probe(i, probe, uuid);
                    }
                }
                Ok(false)
            }
        }
    }

    /// "Does `uuid` match this leaf, according to layer `index`?" — one indexed
    /// point lookup.
    fn probe(
        &self,
        index: usize,
        probe: &CompiledStatement,
        uuid: &str,
    ) -> Result<bool, ReadError> {
        let conn = open_head(&self.layers[index].dir)?;
        let mut bound = bind(&probe.params);
        bound.push(SqlValue::Text(uuid.to_string()));
        let hit: i64 =
            conn.query_row(&probe.sql, rusqlite::params_from_iter(bound.iter()), |r| {
                r.get(0)
            })?;
        Ok(hit != 0)
    }

    /// Resolve a query to the resource UUIDs matching it in the **composed** view.
    ///
    /// ```text
    /// composed = (what the BASE matches, minus everything an overlay touched)
    ///          ∪ (every touched resource that the COMPOSED evaluation matches)
    /// ```
    ///
    /// The first arm is a single-layer query — the base's own plan, its own fast
    /// paths — and is correct precisely because a resource no overlay touched
    /// cannot have been changed by one. The second arm is where composition
    /// happens, and it is bounded by the overlays.
    ///
    /// Ordering is deterministic: the base's matches in rid order, then the
    /// composed candidates in UUID order.
    ///
    /// # The `limit`, precisely
    ///
    /// Applied to the COMPOSED result, after composition — never per layer. (It
    /// therefore cannot hide an overlay match behind a full base window, which a
    /// per-layer limit could.) `query.measures` is ignored: this executes an ids
    /// plan by construction, because a count returns a number and numbers cannot
    /// be precedence-filtered.
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

    /// [`Layers::resolve`] without the row cap — the honest thing to call when you
    /// need the whole composed set, and what [`Layers::count_by_union`] counts.
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
        let base = model_layers[0];
        let candidates = self.candidates(&model_layers, graph)?;

        // Arm 1: the base's own matches, minus anything an overlay touched.
        let ids_stmt = self.compile_ids(query, graph, registry)?;
        let base_conn = open_head(&self.layers[base].dir)?;
        let mut out: Vec<String> = select_ids(&base_conn, &ids_stmt)?
            .into_iter()
            .filter(|u| !candidates.contains(u))
            .collect();

        // Arm 2: every touched resource, evaluated per-nodegroup across the stack.
        if !candidates.is_empty() {
            let plan = match &query.r#where {
                Some(expr) => Some(self.plan(query, expr, graph, registry)?),
                // No filter: every resource matches, so every candidate does.
                None => None,
            };
            for uuid in &candidates {
                let hit = match &plan {
                    Some(plan) => self.matches(plan, uuid, &model_layers, graph)?,
                    None => true,
                };
                if hit {
                    out.push(uuid.clone());
                }
            }
        }
        Ok(out)
    }

    /// Compile the ids plan once, forcing `measures = [select_ids]` and lifting
    /// the row cap: the composed limit is applied after composition, so a
    /// per-layer cap here would silently truncate the input to it.
    fn compile_ids(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<CompiledStatement, ReadError> {
        let ids_query = Query {
            measures: vec![Measure::SelectIds],
            limit: Some(u32::MAX),
            ..query.clone()
        };
        let mut stmts =
            compile_with_registry(&ids_query, graph, registry).map_err(ReadError::Query)?;
        Ok(stmts.remove(0))
    }

    /// `|composed match set|` — a count of the composed view, **never** a sum of
    /// per-layer counts.
    ///
    /// # Why the obvious things are wrong
    ///
    /// - `Σ count_i` double-counts every resource present in two layers.
    /// - "each layer counts only what it originates, then sum" is subtly wrong:
    ///   origination is stable across layers, **match status is not**. The base
    ///   has `B: type = Church`; an overlay edits it to `Chapel`. Counting
    ///   `type = Church`: the base originated `B` so it counts it; the overlay
    ///   originates nothing so it counts nothing — and the total wrongly includes
    ///   `B`, which is a Chapel in the composed view. Silent off-by-N.
    ///
    /// # The plan — O(overlay), not O(result)
    ///
    /// ```text
    /// total = count(F, base)                    // the single-layer FAST path survives
    ///       - |{c ∈ touched : the BASE says c matches}|      // its verdict is now stale
    ///       + |{c ∈ touched : the COMPOSED view says c matches}|
    /// ```
    ///
    /// The base keeps its point-lookup count (`rollup_concept_counts`, or the
    /// index-only `COUNT(DISTINCT rid)`) — the thing a naive union destroys. Both
    /// corrections range over the resources the OVERLAYS touch, so this is a base
    /// count plus a handful of indexed probes, not a haul of every matching UUID
    /// in the corpus. (On a 209K corpus with a filter matching 15,773 resources,
    /// the union path drags 15,773 UUIDs out of the base to de-duplicate them
    /// against an overlay of five. This does not.) It degrades gracefully: an
    /// overlay the size of the corpus makes this no worse than the union.
    ///
    /// Still: a layered count costs strictly more than a single-layer count — a
    /// base probe and a composed evaluation per touched resource. That is the
    /// price of correctness, and it is paid per overlay, not per corpus.
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
        let base = model_layers[0];
        let base_conn = open_head(&self.layers[base].dir)?;

        let count_query = Query {
            measures: vec![Measure::CountRecords],
            ..query.clone()
        };
        let count_stmt = {
            let mut s =
                compile_with_registry(&count_query, graph, registry).map_err(ReadError::Query)?;
            s.remove(0)
        };
        let mut total = select_count(&base_conn, &count_stmt)? as i64;

        let candidates = self.candidates(&model_layers, graph)?;
        if candidates.is_empty() {
            // Single layer, or overlays that carry nothing for this model: this
            // IS a single-layer count, fast paths intact.
            return Ok(total.max(0) as usize);
        }

        // Correction 1: the base's verdict on every touched resource is stale.
        // Probe the base for each and subtract the ones it had counted.
        if let Some(expr) = &query.r#where {
            let probe = compile_match_probe(query, graph, registry).map_err(ReadError::Query)?;
            for uuid in &candidates {
                if self.probe(base, &probe, uuid)? {
                    total -= 1;
                }
            }

            // Correction 2: add back the ones the COMPOSED view matches, each leaf
            // answered by the layer that owns its nodegroup.
            let plan = self.plan(query, expr, graph, registry)?;
            for uuid in &candidates {
                if self.matches(&plan, uuid, &model_layers, graph)? {
                    total += 1;
                }
            }
        } else {
            // No filter: the count is |resources|, so composition is a set union.
            // Every candidate matches; subtract only those the base already had.
            let probe = compile_match_probe(query, graph, registry).map_err(ReadError::Query)?;
            for uuid in &candidates {
                if self.probe(base, &probe, uuid)? {
                    total -= 1;
                }
            }
            total += candidates.len() as i64;
        }

        Ok(total.max(0) as usize)
    }

    /// The count computed the obvious way: resolve the composed id set and take
    /// its size. O(result size) — it drags every matching UUID out of the base.
    ///
    /// This is the **reference**: it is the definition of the answer, and
    /// [`Layers::count`] is an optimisation of it. Kept public because a caller
    /// that already wants the ids should not pay for a second plan, and because a
    /// cross-check against the fast path is worth having.
    ///
    /// It is NOT an independent oracle, mind — it shares [`Layers::matches`] with
    /// the fast path, so a bug in per-nodegroup precedence would fool both. The
    /// independent check is that a composed QUERY agrees with a composed
    /// HYDRATION, and that lives in `tests/layers.rs`.
    pub fn count_by_union(
        &self,
        query: &Query,
        graph: &StaticGraph,
        registry: Option<&ExtensionTypeRegistry>,
    ) -> Result<usize, ReadError> {
        Ok(self.resolve_unlimited(query, graph, registry)?.len())
    }
    /// The tiles of one resource in the **composed** view: gathered from every
    /// layer that has it, merged with precedence (topmost layer wins).
    ///
    /// # This does not implement a merge. Alizarin's does, in `PerNodegroup` mode.
    ///
    /// `merge_resources` + `unify_cardinality_one_tiles` are the same functions the
    /// business-data shard merge uses; the ONLY thing layer composition needs
    /// differently is whole-nodegroup override instead of per-key fill-in, which is
    /// the [`TileMergeMode`] parameter. So this gathers the resource from each
    /// layer **topmost-first** and hands the stack over:
    ///
    /// - **cardinality-1** — the unifier groups tiles by scope
    ///   `(nodegroup_id, parenttile_id)`, reads cardinality from the *graph*, and
    ///   collapses each over-full scope into the FIRST (topmost) tile. In
    ///   `PerNodegroup` mode the others are discarded WHOLE: the overlay's tile
    ///   replaces the base's, and a field the base set but the overlay omitted is
    ///   gone, not inherited. **This is the atomic-nodegroup rule** — a nodegroup
    ///   is the unit a layer adds or replaces — and it is what lets query and
    ///   hydration agree, because the composed query treats the overlay as owning
    ///   the whole nodegroup (`fragment_dir`), so hydration must too.
    /// - **cardinality-n** — left alone: **additive union**. Nothing here can
    ///   silently replace a multi-valued tile.
    /// - **children of a superseded cardinality-1 tile** — re-parented onto the
    ///   survivor (`tile_redirect`), so a cardinality-n nodegroup under an
    ///   overridden cardinality-1 parent **merges rather than vanishing**.
    ///
    /// `merge_resources`' tileid-dedup already does the override for layers whose
    /// cardinality-1 tiles share alizarin's *canonical* ids (topmost-first ⇒ the
    /// topmost is kept). The `unify` pass is the safety net for the residual case
    /// (Arches-minted differing ids), collapsing them whole so a cardinality-1
    /// scope never hydrates two tiles.
    ///
    /// **Retraction** works here: an overlay ships an empty tile for a nodegroup,
    /// its (shared canonical) id wins the dedup, and the composed resource shows
    /// the empty tile — the base's is gone. (Cardinality-n still has no retraction:
    /// the id space makes those add-only.)
    ///
    /// This returns TILES only; it does not touch the descriptor. The composed
    /// `display_name` is re-derived from these tiles at hydration — see
    /// [`hydrate_tiles`], which [`Layers::hydrate_resource`] routes through. That
    /// is what gives a shared entry the layer's real headword instead of a lower
    /// layer's `<Headword>` placeholder: the descriptor is a function of the
    /// COMPOSED tiles, not of any one layer's precomputed `spine.display_name`.
    /// (A corpus-wide descriptor rebuild is still the expensive path this design
    /// avoids — recompute is per-resource, at display time, not at emit-scale.)
    pub fn resource_tiles(
        &self,
        uuid: &str,
        graph: &StaticGraph,
    ) -> Result<Vec<StaticTile>, ReadError> {
        // Gather the resource from every layer that has it, TOPMOST FIRST — which
        // is what turns "first occurrence wins" into "topmost layer wins".
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

        // Collapse any cardinality-1 scope that still holds >1 tile (Arches-minted
        // differing ids the dedup missed), WHOLE — never per-key: a partial overlay
        // tile must not inherit the base's other fields, or hydration would show a
        // field the composed query says the overlay dropped.
        unify_cardinality_one_tiles(&mut tiles, graph, false, TileMergeMode::PerNodegroup)
            .map_err(ReadError::Merge)?;
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

/// A node's `(nodegroup_id, is_additive)`: the nodegroup precedence is decided
/// at, and whether that nodegroup ACCUMULATES across layers (cardinality-n) or is
/// overridden whole (cardinality-1).
///
/// Cardinality is read from the graph, which is where the tile merge reads it too
/// ([`composed_tiles`]): the query's composition rule and the merge's composition
/// rule are then the same rule, from the same source, and cannot drift.
///
/// A node whose nodegroup the graph does not know falls back to `(node_id,
/// not-additive)` — the safe default: override consults `fragment_dir` and answers
/// from exactly one layer, whereas assuming additive would OR across layers and
/// could resurrect a value a higher layer overrode. (Using the node id as its own
/// nodegroup key is harmless here: an unknown node has no `fragment_dir` rows
/// under either id, so the leaf is simply false.)
fn nodegroup_of(graph: &StaticGraph, node_id: &str) -> (String, bool) {
    match graph
        .get_node_by_id(node_id)
        .and_then(|n| n.nodegroup_id.as_deref())
        .and_then(|ng| graph.get_nodegroup_by_id(ng))
    {
        Some(ng) => (
            ng.nodegroupid.clone(),
            ng.cardinality.as_deref() == Some("n"),
        ),
        None => (node_id.to_string(), false),
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
    // Handler sets must match on WHAT indexes a datatype, not on the exact
    // build that did it. A field indexed under a handler the other layer lacks
    // answers zero rows there, silently — that is the real hazard, and it is
    // identified by (datatype, provider). The handler `version` is provenance
    // only (see `HandlerDecl`'s own doc); comparing it made a clm-core patch
    // bump a hard `Incompatible`, so a device overlay built against a newer app
    // could not compose with its OWN shipped base until that base was re-emitted
    // — impossible on a deployed device. Version is therefore excluded from the
    // composition identity. (If a version ever changes a datatype's on-disk
    // format, that must move a format-level identifier, not lean on this string.)
    let handler_ids = |m: &Manifest| {
        let mut ids: Vec<(String, String)> = m
            .handlers
            .iter()
            .map(|h| (h.datatype.clone(), h.provider.clone()))
            .collect();
        ids.sort();
        ids
    };
    let (hb, hl) = (handler_ids(&base.manifest), handler_ids(&layer.manifest));
    if hb != hl {
        let show = |v: &[(String, String)]| {
            v.iter()
                .map(|(d, p)| format!("{d}@{p}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        return incompatible("handlers", show(&hb), show(&hl));
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
        // No per-field class/storage check: a field's class is a pure function
        // of its datatype (via `datatype_index_spec`), and composed layers query
        // against ONE shared graph, so two layers cannot derive different classes
        // for the same field. The old check compared a materialized map that no
        // longer exists — the map was the thing that could drift, not the graph.
    }
    Ok(())
}
