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
//!   manifest.json     — the layout/compatibility contract
//!
//! (A2: `closure.json` is no longer emitted — concept labels live in
//! `vocab.label`, hierarchy in `vocab`'s DFS intervals, the value→concept map
//! only ever mattered at emit. The head is self-contained for concept display.)
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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use alizarin_core::{parse_business_data_bytes, ExtensionTypeRegistry};
use rusqlite::Connection;

mod chunks;
mod closure;
mod composability;
mod geo;
mod head;
mod input;
mod locality;
mod manifest;

pub use closure::{build_closure, Closure, ClosureEntry};
/// The artifact FORMAT types (manifest contract + chunk-tile wire shape) live
/// in `ros-madair-format` — WASM-buildable, so a browser/Tauri READER can parse
/// what this crate writes without linking the emitter (and without a
/// hand-mirrored copy of the wire format, which is how they drift). Re-exported
/// here so existing callers keep working.
pub use ros_madair_format::{
    ArtifactEntry, Budgets, ChunkTile, EmitSummary, Manifest, ModelManifest, TierManifest,
};

pub type EmitError = Box<dyn std::error::Error>;

/// Emit-time options (M1 item 6): tier exclusions. `Default` reproduces the
/// plain single-tier emit.
///
/// There is no field-class override. A field's head membership is a pure
/// function of its datatype (concept/link → indexed, else → detail-only),
/// computed by `datatype_index_spec`; the reader derives the same, so nothing
/// needs declaring or recording. To index a strict SUBSET of a corpus's concept
/// fields, prune a search graph (`prune_graph`) — a graph-level operation, not a
/// per-field flag.
#[derive(Default, Clone)]
pub struct EmitOptions {
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
    emit_with_options(
        data_dir,
        out_dir,
        base_uri,
        &EmitOptions::default(),
        &registry,
    )
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

    let collections = closure::load_collections(data_dir, base_uri)?;
    let closure = build_closure(&collections);

    let head_path = out.join("head.sqlite");
    let _ = fs::remove_file(&head_path);
    let mut conn = Connection::open(&head_path)?;
    head::create_schema(&conn)?;

    let mut interner = head::Interner::default();
    let vocab_rows = head::preintern_concepts(&mut interner, &collections, &closure);
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
    let ctxs: Vec<head::ModelCtx> = models
        .iter()
        .map(|m| head::ModelCtx::new(&m.graph))
        .collect();
    let by_graph: HashMap<&str, usize> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.graph.graphid.as_str(), i))
        .collect();
    let mut resource_counts = vec![0usize; models.len()];

    // A8-locality: classify each model's ordering field (geo Hilbert / date /
    // none) ONCE. Applied to both passes below so chunk order == interning order
    // (A9's invariant). See `locality`.
    let localities = locality::model_localities(&models, registry);

    // Tier nodegroup exclusion set (the other half of retain_tier_models):
    // filtered per-resource before processing.
    let exclude_ngs: HashSet<&str> = options
        .tier
        .as_ref()
        .map(|t| t.exclude_nodegroups.iter().map(String::as_str).collect())
        .unwrap_or_default();

    // A9 — PRE-INTERN RESOURCE IDS IN STREAM ORDER, before any tile is
    // processed. A link target is a resource id; interning it inline (at first
    // encounter — as a target, or as its own resource, whichever came first)
    // scattered target ids across the id space with no relation to resource
    // identity. So `chunk_link_summary` target ranges spanned ~1/3 of the id
    // space (measured: median 33%, mean 38% on wiktionary), and coarse link
    // routing was 16–31× weaker than designed — silently, and worse with scale.
    //
    // Interning every resource id up front, in the SAME order the streaming pass
    // below will visit them, makes a target's id its stream position. A chunk's
    // link tiles come from a contiguous run of source resources, so their target
    // ranges then tighten to the extent the source data has locality. This is the
    // P18-corollary ("interning order is locality order") applied to resources —
    // exactly as it already is to concepts (DFS order) above; only concepts got
    // it before. The same sorted walk, so the pre-intern order matches the
    // stream order exactly (determinism, and target id == spine position).
    //
    // (Cost: a second parse of the business data to read ids. Memory stays
    // bounded — one file at a time, dropped. An id-only parse would avoid the
    // full re-parse; deferred, correctness first.)
    let files = input::business_data_files(data_dir)?;
    for path in &files {
        let bytes = fs::read(path)?;
        if let Ok(mut resources) = parse_business_data_bytes(&bytes) {
            // A8-locality: intern in the SAME order the streaming pass will chunk
            // (locality order for geo/date models), so a target's id remains its
            // chunk position (A9). Identical sort in both passes = lockstep.
            locality::sort_by_locality(&mut resources, &by_graph, &localities);
            for r in &resources {
                if by_graph.contains_key(r.resourceinstance.graph_id.as_str()) {
                    interner.intern(&r.resourceinstance.resourceinstanceid);
                }
            }
        }
    }

    // STREAMING PASS: one sorted walk over the business-data files. Each
    // file is read, parsed, and its resources are processed one at a time
    // through the head + chunk sink, then dropped. Peak memory is bounded
    // by a single file's parse plus the persistent interner/closure/sink
    // (the sink already flushes chunks at CHUNK_MAX_TILES), NOT by corpus
    // size. Resource encounter order (sorted files, then in-file order) is
    // deterministic, so the snapshot id is run-stable.
    let tx = conn.transaction()?;
    for path in files {
        let bytes = fs::read(&path)?;
        let mut parsed = match parse_business_data_bytes(&bytes) {
            Ok(resources) => resources,
            Err(e) => {
                eprintln!("skipping {} ({e})", path.display());
                continue;
            }
        };
        // A8-locality: chunk resources in locality order (same sort as the
        // pre-intern pass above), tightening chunk_geo_summary / chunk_value_summary.
        locality::sort_by_locality(&mut parsed, &by_graph, &localities);
        for mut resource in parsed {
            let Some(&idx) = by_graph.get(resource.resourceinstance.graph_id.as_str()) else {
                continue;
            };
            // Tier nodegroup exclusion (single exclusion point, per P: the
            // excluded tiles reach no artifact — head, summaries, chunks).
            if !exclude_ngs.is_empty() {
                if let Some(tiles) = resource.tiles.as_mut() {
                    tiles.retain(|t| !exclude_ngs.contains(t.nodegroup_id.as_str()));
                }
            }
            // Refuse to emit layers that cannot compose. Cardinality-1 tiles are
            // supposed to carry alizarin's DERIVABLE ids (so an independently
            // built layer can address them); several differently-id'd tiles in
            // one (parent, nodegroup) scope means they do not. We DETECT this —
            // we never rewrite ids. See `composability`.
            composability::validate_composable_tile_ids(&resource, &models[idx].graph)?;

            head::process_resource(
                &tx,
                &spine_tables[idx],
                &ctxs[idx],
                resource,
                &closure,
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
    for (i, model) in models.iter().enumerate() {
        manifest_models.push(ModelManifest {
            slug: model.slug.clone(),
            graph_id: model.graph.graphid.clone(),
            spine_table: spine_tables[i].clone(),
            resource_count: resource_counts[i],
        });
    }

    sink.flush_remaining(&mut interner)?;
    head::insert_bulk(&mut conn, &interner, &vocab_rows, &sink)?;
    head::finalize(&conn)?;
    drop(conn);

    // A2: `closure.json` is no longer shipped. It carried concept labels (now in
    // `vocab.label`), the DFS ancestors (derivable from `vocab`'s intervals), and
    // the value→concept map (only ever needed at emit, to resolve tile value-ids
    // into `concept_tags` — see `process_resource`). All of that is now either in
    // the head or an emit-only intermediate, so the head is self-contained for
    // concept display: `concept_tags → vocab.label`. `build_closure` still runs
    // above; its output just stays in memory.

    // Manifest: hash every artifact, then hash the manifest itself, and derive
    // the snapshot id from the whole set. The id is left EMPTY while the
    // manifest is built — it is an input to its own digest, and the
    // self-reference is resolved by hashing the id-less form (see
    // manifest::manifest_digest_bytes). A manifest-only change (handler set,
    // tier) therefore MOVES the id: two deployments that answer differently
    // cannot share an identity.
    let artifacts = manifest::hash_artifacts(out)?;

    let head_db_bytes = fs::metadata(out.join("head.sqlite"))?.len();
    let mut manifest = Manifest {
        snapshot_id: String::new(),
        // P17: the reader gates on this; it must be the version the emitter
        // actually wrote across the head + chunks.
        format_version: ros_madair_format::FORMAT_VERSION,
        base_uri: base_uri.to_string(),
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
    let snapshot_id = manifest::snapshot_id(
        &manifest.artifacts,
        &manifest::manifest_digest_bytes(&manifest)?,
    );
    manifest.snapshot_id = snapshot_id.clone();
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
