// SPDX-License-Identifier: AGPL-3.0-or-later
//! M1 emitter (Static-Assets-Implementation-Plan): compiles
//! alizarin-governed graphs + business data + SKOS vocabularies into
//! static query artifacts:
//!
//!   head.sqlite       — quantized head ("RM-the-schema in SQLite",
//!                       per RM-Principle-Inventory P0-corollary):
//!                       dict interning table (concepts pre-interned in
//!                       closure DFS order — P18-corollary: interning
//!                       order is locality order), integer spine /
//!                       exact concept tags + vocab DFS intervals (P10:
//!                       hierarchy = `concept BETWEEN dfs_enter AND
//!                       dfs_leave`; no expanded ancestor table) /
//!                       coarse link summaries (P1/P2: chunk→target-range
//!                       in the head, exact pairs resurface from tiles
//!                       client-side) / fragment directory,
//!                       chunk_summary (summary.bin-as-data, P1/P2/P15),
//!                       rollups
//!   chunks/<hash>.msgpack — content-hashed per-nodegroup tile chunks
//!   closure.json      — concept closure (first-class artifact)
//!   manifest.json     — the layout/compatibility contract
//!
//! NO TEXT IN THE HEAD: strings/numbers/dates are detail-only (live in
//! chunks; text search is the Pagefind sidecar). The head indexes
//! concepts and links only; the sole string columns are `dict.term`
//! (the interned UUID/URI dictionary), chunk hashes, and per-resource
//! `display_name`.
//!
//! MVP scope: single tier, no parquet/FTS sidecars, no signing.
//! Deterministic: no timestamps; snapshot id is a hash of artifact hashes.
//!
//! Module layout: `input` (data-layout detection and loading), `closure`
//! (SKOS loading/normalization and the concept closure), `head` (SQLite
//! schema and population), `chunks` (content-hashed tile chunks),
//! `manifest` (contract types and snapshot id). This file is the
//! `emit()` orchestration only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

use alizarin_core::{parse_business_data_bytes, ExtensionTypeRegistry};
use rusqlite::Connection;

mod chunks;
mod closure;
mod head;
mod input;
mod manifest;

pub use closure::{build_closure, Closure, ClosureEntry};
pub use manifest::{
    ArtifactEntry, Budgets, EmitSummary, FieldEntry, Manifest, ModelManifest, TierManifest,
};

pub type EmitError = Box<dyn std::error::Error>;

/// Typed field-class violations (M1 item 3). These are contract errors,
/// not warnings: "filterable" on a datatype the head cannot index
/// without storing text (anything that is not a concept or resource
/// link) would break the head's core invariant — NO TEXT IN THE HEAD.
#[derive(Debug)]
pub enum FieldClassError {
    /// "filterable" declared on a non-concept, non-link datatype.
    NotFilterable { alias: String, datatype: String },
    /// A declared class other than "filterable" | "detail-only".
    UnknownClass { alias: String, class: String },
    /// A declared alias that matched no node in any loaded model.
    UnknownAlias { alias: String },
}

impl std::fmt::Display for FieldClassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldClassError::NotFilterable { alias, datatype } => write!(
                f,
                "field class error: alias '{alias}' (datatype '{datatype}') \
                 cannot be declared filterable — only concept and \
                 resource-link datatypes are head-indexable (no text in \
                 the head)"
            ),
            FieldClassError::UnknownClass { alias, class } => write!(
                f,
                "field class error: alias '{alias}' declares unknown class \
                 '{class}' (expected \"filterable\" or \"detail-only\")"
            ),
            FieldClassError::UnknownAlias { alias } => write!(
                f,
                "field class error: alias '{alias}' matched no node in any \
                 loaded model"
            ),
        }
    }
}

impl std::error::Error for FieldClassError {}

/// Emit-time options (M1 items 3/6): schema-declared field classes and
/// tier exclusions. `Default` reproduces the plain single-tier emit.
#[derive(Default, Clone)]
pub struct EmitOptions {
    /// alias -> declared class ("filterable" | "detail-only"). Overrides
    /// datatype inference; datatype inference is only a proposal the
    /// schema confirms. A node declared detail-only is NOT head-indexed
    /// (no concept_tags rows, no chunk concept/link summaries).
    pub field_classes: BTreeMap<String, String>,
    /// Tier name + exclusions; when set, the caller directs out_dir at
    /// the tier's own subdirectory and this is recorded in the manifest.
    pub tier: Option<TierManifest>,
}

/// The emitter's extension-type registry. Defined ONCE, in
/// `ros-madair-handlers`, which the query side (and any WASM consumer) uses
/// too: an emitter registry that disagreed with the query-side registry does
/// not error, it silently returns zero rows (see that crate's docs). Re-exported
/// here so existing callers keep working.
pub use ros_madair_handlers::default_registry;

pub fn emit(data_dir: &str, out_dir: &str, base_uri: &str) -> Result<EmitSummary, EmitError> {
    let registry = default_registry();
    emit_with_options(data_dir, out_dir, base_uri, &EmitOptions::default(), &registry)
}

/// M1.5 tier splitting (model half): drop excluded models before field
/// plans, so their aliases never reach the plan/matched-alias check and
/// their resources are never streamed. The nodegroup half of the
/// exclusion is applied per-resource in the streaming loop (excluded
/// nodegroups' tiles are filtered before a resource is processed), which
/// is the same single exclusion point conceptually: excluded
/// models/nodegroups reach NO artifact (concept_tags, chunk/link
/// summaries, fragment_dir, chunk files). Per-tier emits stay fully
/// disjoint artifact graphs.
fn retain_tier_models(models: &mut Vec<input::LoadedModel>, tier: &TierManifest) {
    models.retain(|m| {
        !tier
            .exclude_models
            .iter()
            .any(|x| x == &m.slug || x == &m.graph.graphid)
    });
}

pub fn emit_with_options(
    data_dir: &str,
    out_dir: &str,
    base_uri: &str,
    options: &EmitOptions,
    registry: &ExtensionTypeRegistry,
) -> Result<EmitSummary, EmitError> {
    // Validate declared classes upfront (typed errors, M1 item 3).
    for (alias, class) in &options.field_classes {
        if class != "filterable" && class != "detail-only" {
            return Err(Box::new(FieldClassError::UnknownClass {
                alias: alias.clone(),
                class: class.clone(),
            }));
        }
    }

    let data_dir = Path::new(data_dir);
    let out = Path::new(out_dir);
    fs::create_dir_all(out.join("chunks"))?;

    // Graphs load up front (small); resources stream file-by-file below.
    let mut models = input::load_graphs(data_dir)?;

    // Tier model exclusions apply immediately after load — BEFORE field
    // plans, so excluded models never reach alias matching. The nodegroup
    // half is applied per-resource in the streaming loop.
    if let Some(tier) = &options.tier {
        retain_tier_models(&mut models, tier);
    }

    // Field plans next (fail fast): validates declared classes against
    // datatypes and catches declared aliases that match no node in any
    // model, before any population work.
    let mut matched_aliases = std::collections::BTreeSet::new();
    let mut plans = Vec::with_capacity(models.len());
    for model in &models {
        plans.push(head::field_plan(
            &model.graph,
            &options.field_classes,
            &mut matched_aliases,
            registry,
        )?);
    }
    // A declared alias that matched nothing is a typo, not a no-op —
    // the schema declaration is a contract.
    for alias in options.field_classes.keys() {
        if !matched_aliases.contains(alias) {
            return Err(Box::new(FieldClassError::UnknownAlias {
                alias: alias.clone(),
            }));
        }
    }

    let collections = closure::load_collections(data_dir, base_uri)?;
    let closure = build_closure(&collections);

    let head_path = out.join("head.sqlite");
    let _ = fs::remove_file(&head_path);
    let mut conn = Connection::open(&head_path)?;
    head::create_schema(&conn)?;

    let mut interner = head::Interner::default();
    let vocab_rows = head::preintern_concepts(&mut interner, &collections);
    let mut sink = chunks::ChunkSink::new(out.join("chunks"));
    let mut next_rid: i64 = 1;
    let mut total_resources = 0usize;
    let mut total_tiles = 0usize;

    // Per-model spine tables, routing contexts and counters. Spine tables
    // are created up front (before the streaming transaction) in model
    // order — same sqlite_master order as the batch emit.
    let spine_tables: Vec<String> = models
        .iter()
        .map(|m| format!("spine_{}", m.slug.replace('-', "_")))
        .collect();
    for spine_table in &spine_tables {
        head::create_spine_table(&conn, spine_table)?;
    }
    let ctxs: Vec<head::ModelCtx> = models.iter().map(|m| head::ModelCtx::new(&m.graph)).collect();
    let by_graph: HashMap<&str, usize> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.graph.graphid.as_str(), i))
        .collect();
    let mut resource_counts = vec![0usize; models.len()];

    // Tier nodegroup exclusion set (the other half of retain_tier_models):
    // filtered per-resource before processing.
    let exclude_ngs: HashSet<&str> = options
        .tier
        .as_ref()
        .map(|t| t.exclude_nodegroups.iter().map(String::as_str).collect())
        .unwrap_or_default();

    // STREAMING PASS: one sorted walk over the business-data files. Each
    // file is read, parsed, and its resources are processed one at a time
    // through the head + chunk sink, then dropped. Peak memory is bounded
    // by a single file's parse plus the persistent interner/closure/sink
    // (the sink already flushes chunks at CHUNK_MAX_TILES), NOT by corpus
    // size. Resource encounter order (sorted files, then in-file order) is
    // deterministic, so the snapshot id is run-stable.
    let files = input::business_data_files(data_dir)?;
    let tx = conn.transaction()?;
    for path in files {
        let bytes = fs::read(&path)?;
        let parsed = match parse_business_data_bytes(&bytes) {
            Ok(resources) => resources,
            Err(e) => {
                eprintln!("skipping {} ({e})", path.display());
                continue;
            }
        };
        for mut resource in parsed {
            let Some(&idx) = by_graph
                .get(resource.resourceinstance.graph_id.as_str())
            else {
                continue;
            };
            // Tier nodegroup exclusion (single exclusion point, per P: the
            // excluded tiles reach no artifact — head, summaries, chunks).
            if !exclude_ngs.is_empty() {
                if let Some(tiles) = resource.tiles.as_mut() {
                    tiles.retain(|t| !exclude_ngs.contains(t.nodegroup_id.as_str()));
                }
            }
            head::process_resource(
                &tx,
                &spine_tables[idx],
                &ctxs[idx],
                resource,
                &closure,
                &plans[idx].1,
                &mut interner,
                &mut sink,
                &mut next_rid,
                &mut total_resources,
                &mut total_tiles,
                registry,
            )?;
            resource_counts[idx] += 1;
        }
    }
    tx.commit()?;

    let mut manifest_models = Vec::new();
    for (i, (model, (fields, _detail_only))) in models.iter().zip(plans).enumerate() {
        manifest_models.push(ModelManifest {
            slug: model.slug.clone(),
            graph_id: model.graph.graphid.clone(),
            spine_table: spine_tables[i].clone(),
            fields,
            resource_count: resource_counts[i],
        });
    }

    sink.flush_remaining(&mut interner)?;
    head::insert_bulk(&mut conn, &interner, &vocab_rows, &sink)?;
    head::finalize(&conn)?;
    drop(conn);

    // Closure artifact
    let closure_bytes = serde_json::to_vec_pretty(&closure)?;
    fs::write(out.join("closure.json"), &closure_bytes)?;

    // Manifest: hash every artifact, snapshot id from the hash set.
    let (artifacts, snapshot_id) = manifest::hash_artifacts(out)?;

    let head_db_bytes = fs::metadata(out.join("head.sqlite"))?.len();
    let manifest = Manifest {
        manifest_version: manifest::MANIFEST_VERSION,
        snapshot_id: snapshot_id.clone(),
        base_uri: base_uri.to_string(),
        min_client_version: "0.1.0".to_string(),
        tier: options.tier.clone(),
        // I6: the artifact explains its own handler set. Derived from the
        // registry ACTUALLY used for this emit, so the query side can rebuild
        // exactly it (ros_madair_handlers::registry_from_declarations) instead
        // of guessing a default that may not match what was indexed.
        handlers: ros_madair_handlers::describe_registry(registry),
        models: manifest_models,
        artifacts,
        budgets: Budgets {
            max_result_rows: 1000,
            max_group_count: 500,
        },
    };
    fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    Ok(EmitSummary {
        snapshot_id,
        models: manifest.models.len(),
        resources: total_resources,
        tiles: total_tiles,
        chunks: sink.chunk_rows.len(),
        concepts: closure.concepts.len(),
        dict_terms: interner.len(),
        head_db_bytes,
    })
}
