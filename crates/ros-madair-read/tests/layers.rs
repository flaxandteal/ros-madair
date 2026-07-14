// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Multi-layer composition (R1), end to end.** Two REAL snapshots, emitted by
//! the real emitter from two real corpora, read back as one composed view.
//!
//! No hand-built head, and no hand-built expectations: every number here is
//! cross-checked against [`Layers::count_by_union`], which computes the same
//! answer the slow, obvious way (resolve the composed id set, take its size).
//! The union is the ORACLE — it is the definition of the answer — and
//! `Layers::count` is an optimisation of it. If they ever disagree, the
//! optimisation is wrong.
//!
//! # The corpus, and why it is shaped like this
//!
//! The demo Talk model has one concept-list node (`topics`) and every nodegroup
//! is cardinality-1, which is precisely the interesting case.
//!
//! | resource | BASE topics | OVERLAY topics |
//! |---|---|---|
//! | Talk A (`3 W's of UI`) | `UI`, `MISC` | `UI` — **drops MISC** |
//! | Talk B (`Javascript…`) | `JS` | `UI` — **flipped out of JS, into UI** |
//! | Talk C (synthetic) | `UI` | *(absent — the overlay never mentions it)* |
//!
//! That single overlay exercises all three ways a naive composition breaks:
//!
//! 1. **Double counting.** Talk A matches `UI` in BOTH layers. `Σ per-layer
//!    count` says 4 for `UI`; the truth is 3.
//! 2. **Flipping OUT.** Talk B matched `JS` in the base and no longer does. A
//!    union of per-layer matches would keep counting it forever. The truth is 0
//!    — and the base, alone, still says 1. Same for Talk A and `MISC`.
//! 3. **The base's verdict SURVIVING.** Talk C is not redefined by the overlay,
//!    so the base remains authoritative for it. A composition that only trusted
//!    the topmost layer would lose it.
//!
//! The overlay restates only the `topics` tile — not `title`, not `abstract`.
//! **Layers are partial**, and its topics tiles carry FRESH, Arches-style
//! tileids that collide with nothing. Composition must still override.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_query::{ConceptOp, Expr, Measure, Query};
use ros_madair_read::{Layers, ReadError};
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";

const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TOPICS_NG: &str = "3784d67d-1ae9-11f0-86d0-a32be8fb5c91";
const TITLE_NG: &str = "7d8e443d-1ae0-11f0-8c5c-8fd6f4eb1a02";

const TALK_A: &str = "179b8583-3140-437e-bd0b-34d5aa2f1550";
const TALK_B: &str = "97cc9a1b-ee42-412e-9fa5-203d98bff815";
const TALK_C: &str = "0d1e2f3a-4b5c-6d7e-8f90-a1b2c3d4e5f6";

// A `concept-list` tile stores concept *value* ids; the emitter resolves them
// through SKOS to *concept* ids, and it is the CONCEPT id a filter names. Two
// different id spaces, and mixing them yields a silent zero.
const VALUE_UI: &str = "f71cff66-5421-4420-b06e-397376eb2f56";
const VALUE_MISC: &str = "152fe3a0-f1de-43e4-9897-026587cee523";
const VALUE_JS: &str = "08d28d3b-9c92-4004-b7ee-ed131a348e92";

const CONCEPT_UI: &str = "7c3226cf-4d13-41b4-8a9d-7c26a058fd44";
const CONCEPT_MISC: &str = "de87aac2-8560-4609-9f54-5e27c990f9ce";
const CONCEPT_JS: &str = "3cce47ca-eaec-4718-aca4-ba55902466c6";
/// Ancestor of both `CONCEPT_UI` and `CONCEPT_JS` in the vocab's DFS interval.
const CONCEPT_PARENT: &str = "d6435de5-acdb-4ef3-8181-0bd035e5d5c6";

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()),
    );
    dir.join("graphs").is_dir().then_some(dir)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

/// A private directory per call. The tag alone is NOT unique — cargo runs these
/// tests in parallel threads of ONE process, and several of them build a corpus
/// tagged "base", so a pid-and-tag path would have them deleting and re-emitting
/// each other's fixtures mid-run.
fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-layers-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One tile of a `concept-list` node. `tileid` is caller-chosen precisely so the
/// overlay can mint a FRESH one — an on-device edit that recreates a tile does
/// not know the base's tileid, and composition must not depend on it.
fn topics_tile(resource: &str, tileid: &str, values: &[&str]) -> serde_json::Value {
    json!({
        "tileid": tileid,
        "nodegroup_id": TOPICS_NG,
        "parenttile_id": null,
        "resourceinstance_id": resource,
        "sortorder": 0,
        "provisionaledits": null,
        "data": { TOPICS_NG: values },
    })
}

fn title_tile(resource: &str, tileid: &str, title: &str) -> serde_json::Value {
    json!({
        "tileid": tileid,
        "nodegroup_id": TITLE_NG,
        "parenttile_id": null,
        "resourceinstance_id": resource,
        "sortorder": 0,
        "provisionaledits": null,
        "data": { TITLE_NG: { "en": { "value": title, "direction": "ltr" } } },
    })
}

fn resource(id: &str, name: &str, tiles: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "resourceinstance": {
            "resourceinstanceid": id,
            "graph_id": TALK_GRAPH,
            "name": name,
            "legacyid": null,
            "descriptors": { "en": { "name": name, "description": "", "map_popup": "" } },
        },
        "tiles": tiles,
    })
}

/// Write a corpus: the demo's graphs + vocabularies (so the Talk model and the
/// SKOS hierarchy are real), with a Talk business-data file of our own.
fn corpus(
    tag: &str,
    demo: &Path,
    talks: Vec<serde_json::Value>,
    keep_other_models: bool,
) -> PathBuf {
    let dir = scratch(tag);
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    if keep_other_models {
        // The base is a whole corpus; the overlay is deliberately Talk-only.
        copy_dir(&demo.join("resources"), &dir.join("resources"));
    }
    let talk_dir = dir.join("resources").join("talk");
    let _ = std::fs::remove_dir_all(&talk_dir);
    std::fs::create_dir_all(&talk_dir).unwrap();
    std::fs::write(
        talk_dir.join("talks.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": talks } })).unwrap(),
    )
    .unwrap();
    dir
}

fn emit(tag: &str, data: &Path) -> PathBuf {
    let out = scratch(&format!("{tag}-head"));
    ros_madair_emit::emit(
        data.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
    )
    .expect("emit");
    out
}

/// The base: Talk A (`UI` + `MISC`), Talk B (`JS`), Talk C (`UI`).
fn base_corpus(demo: &Path) -> PathBuf {
    corpus(
        "base",
        demo,
        vec![
            resource(
                TALK_A,
                "3 W's of UI",
                vec![
                    title_tile(
                        TALK_A,
                        "aaaa1111-0000-4000-8000-000000000001",
                        "3 W's of UI",
                    ),
                    topics_tile(
                        TALK_A,
                        "aaaa1111-0000-4000-8000-000000000002",
                        &[VALUE_UI, VALUE_MISC],
                    ),
                ],
            ),
            resource(
                TALK_B,
                "Javascript, Typescript and Manuscript",
                vec![
                    title_tile(
                        TALK_B,
                        "bbbb2222-0000-4000-8000-000000000001",
                        "Javascript, Typescript and Manuscript",
                    ),
                    topics_tile(TALK_B, "bbbb2222-0000-4000-8000-000000000002", &[VALUE_JS]),
                ],
            ),
            resource(
                TALK_C,
                "A talk the overlay never touches",
                vec![
                    title_tile(
                        TALK_C,
                        "cccc3333-0000-4000-8000-000000000001",
                        "A talk the overlay never touches",
                    ),
                    topics_tile(TALK_C, "cccc3333-0000-4000-8000-000000000002", &[VALUE_UI]),
                ],
            ),
        ],
        true,
    )
}

/// The overlay: Talk A and Talk B, `topics` ONLY, both retagged to `UI`, with
/// tileids that match nothing in the base.
fn overlay_corpus(demo: &Path) -> PathBuf {
    corpus(
        "overlay",
        demo,
        vec![
            resource(
                TALK_A,
                "3 W's of UI",
                vec![topics_tile(
                    TALK_A,
                    "9999ffff-0000-4000-8000-00000000000a",
                    &[VALUE_UI],
                )],
            ),
            resource(
                TALK_B,
                "Javascript, Typescript and Manuscript",
                vec![topics_tile(
                    TALK_B,
                    "9999ffff-0000-4000-8000-00000000000b",
                    &[VALUE_UI],
                )],
            ),
        ],
        false,
    )
}

fn talk_graph(demo: &Path) -> StaticGraph {
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(demo.join("graphs").join(format!("{TALK_GRAPH}.json"))).unwrap(),
    )
    .unwrap();
    let value = raw
        .get("graph")
        .and_then(|g| g.get(0))
        .cloned()
        .unwrap_or(raw);
    serde_json::from_value(value).unwrap()
}

/// base + overlay, emitted and opened as a stack. `None` when the demo fixture
/// is absent (the tests then skip, as `hydrate.rs` does).
fn stack() -> Option<(Layers, PathBuf, PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let base = emit("base", &base_corpus(&demo));
    let overlay = emit("overlay", &overlay_corpus(&demo));
    let layers = Layers::open(&[base.as_path(), overlay.as_path()]).expect("open layers");
    let graph = talk_graph(&demo);
    Some((layers, base, overlay, graph))
}

fn is_concept(concept: &str) -> Query {
    Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(Expr::Concept {
            path: "topics".to_string(),
            op: ConceptOp::Is,
            value: concept.to_string(),
        }),
        measures: vec![Measure::CountRecords],
        limit: None,
    }
}

/// Count via the fast path, and assert the two independent slow paths agree.
/// A composed count that only one implementation believes is not a fact.
fn agreed_count(layers: &Layers, query: &Query, graph: &StaticGraph) -> usize {
    let fast = layers.count(query, graph, None).expect("count");
    let union = layers.count_by_union(query, graph, None).expect("union");
    let sql = layers
        .count_by_attached_sql(query, graph, None)
        .expect("attached sql");
    assert_eq!(
        fast, union,
        "the fast count disagrees with the union ORACLE — the optimisation is wrong"
    );
    assert_eq!(
        sql, union,
        "the ATTACH-in-SQL count disagrees with the union oracle"
    );
    fast
}

macro_rules! fixture {
    ($binding:pat) => {
        let Some($binding) = stack() else {
            eprintln!("demo fixture absent — skipping");
            return;
        };
    };
}

// ---------------------------------------------------------------------------
// The premise: layers do not share a dictionary
// ---------------------------------------------------------------------------

/// **The load-bearing premise of the whole module, asserted rather than assumed.**
///
/// Each head interns its own terms sequentially, so the SAME uuid has DIFFERENT
/// `term_id`s in different layers. Nothing may be joined across layers but the
/// UUID *string*. If this test ever fails because the ids happen to coincide,
/// that is luck, not a guarantee — but it documents why no code here joins on a
/// `term_id`, an `rid`, a `chunk` or a `concept` id across a layer boundary.
#[test]
fn layers_do_not_share_a_dictionary() {
    fixture!((_layers, base, overlay, _graph));

    let term_id = |head: &Path, term: &str| -> Option<i64> {
        let conn = rusqlite::Connection::open(head.join("head.sqlite")).unwrap();
        conn.query_row("SELECT term_id FROM dict WHERE term = ?1", [term], |r| {
            r.get(0)
        })
        .ok()
    };

    // The same resource, and the same concept, exist in both layers...
    let base_talk = term_id(&base, TALK_B).expect("talk B is in the base dict");
    let overlay_talk = term_id(&overlay, TALK_B).expect("talk B is in the overlay dict");
    let base_concept = term_id(&base, CONCEPT_UI).expect("UI is in the base dict");
    let overlay_concept = term_id(&overlay, CONCEPT_UI).expect("UI is in the overlay dict");

    // ...and at least one of them is interned under a different id, because the
    // overlay's corpus is smaller and interns in its own order. The point is not
    // WHICH differs; it is that neither may be assumed to agree.
    assert!(
        base_talk != overlay_talk || base_concept != overlay_concept,
        "term ids coincided across layers ({base_talk}/{overlay_talk}, \
         {base_concept}/{overlay_concept}) — this is not a guarantee, and no \
         query may ever rely on it"
    );
}

// ---------------------------------------------------------------------------
// The three ways naive composition breaks
// ---------------------------------------------------------------------------

/// **Double counting**, and **the base's verdict surviving**, in one query.
///
/// `UI` matches Talk A in the base AND in the overlay (2 + 2 = 4 the naive way),
/// while Talk C matches only in the base and must not be lost. The truth is 3.
#[test]
fn a_resource_matching_in_two_layers_is_counted_once_and_base_only_matches_survive() {
    fixture!((layers, base, _overlay, graph));

    // What each layer says ALONE, so the naive answers are on the record.
    let base_only = Layers::open(&[base.as_path()]).unwrap();
    assert_eq!(
        base_only
            .count(&is_concept(CONCEPT_UI), &graph, None)
            .unwrap(),
        2,
        "base alone: Talk A and Talk C"
    );

    let composed = agreed_count(&layers, &is_concept(CONCEPT_UI), &graph);
    assert_eq!(
        composed, 3,
        "Talk A (both layers, once), Talk B (overlay), Talk C (base only) — \
         NOT 4, which is what summing the per-layer counts gives"
    );

    let mut ids = layers
        .resolve(&is_concept(CONCEPT_UI), &graph, None)
        .unwrap();
    ids.sort();
    let mut expected = vec![TALK_A.to_string(), TALK_B.to_string(), TALK_C.to_string()];
    expected.sort();
    assert_eq!(
        ids, expected,
        "and resolve() returns each resource exactly once"
    );
}

/// **Flipping OUT.** The base says Talk B is a `JS` talk. The overlay redefines
/// Talk B, and it is no longer one. A union of per-layer matches would keep
/// counting it forever; the composed answer is zero.
///
/// This is the failure the whole precedence rule exists to prevent, and it is
/// SILENT — a stale hit looks exactly like a real one.
#[test]
fn a_resource_the_overlay_flipped_out_of_the_filter_is_not_counted() {
    fixture!((layers, base, _overlay, graph));

    let base_only = Layers::open(&[base.as_path()]).unwrap();
    assert_eq!(
        base_only
            .count(&is_concept(CONCEPT_JS), &graph, None)
            .unwrap(),
        1,
        "the base, alone, really does still say Talk B is a JS talk"
    );

    assert_eq!(
        agreed_count(&layers, &is_concept(CONCEPT_JS), &graph),
        0,
        "but the overlay redefined Talk B without JS, so the composed view has none"
    );
    assert!(layers
        .resolve(&is_concept(CONCEPT_JS), &graph, None)
        .unwrap()
        .is_empty());
}

/// The same, for a concept the overlay merely DROPPED rather than replaced.
/// Talk A had `MISC` in the base; the overlay's `topics` restatement omits it.
/// Nothing announces the removal — the tile is simply different.
#[test]
fn a_concept_the_overlay_dropped_is_not_counted() {
    fixture!((layers, base, _overlay, graph));

    let base_only = Layers::open(&[base.as_path()]).unwrap();
    assert_eq!(
        base_only
            .count(&is_concept(CONCEPT_MISC), &graph, None)
            .unwrap(),
        1
    );
    assert_eq!(agreed_count(&layers, &is_concept(CONCEPT_MISC), &graph), 0);
}

/// A single layer must compose to exactly itself — no precedence machinery may
/// alter the answer when there is nothing to override. (This is also the path a
/// consumer takes before the user has any device layer at all, so a regression
/// here breaks the common case, not the exotic one.)
#[test]
fn one_layer_composes_to_itself() {
    fixture!((_layers, base, _overlay, graph));
    let solo = Layers::open(&[base.as_path()]).unwrap();

    for concept in [CONCEPT_UI, CONCEPT_JS, CONCEPT_MISC] {
        let q = is_concept(concept);
        let fast = solo.count(&q, &graph, None).unwrap();
        assert_eq!(fast, solo.count_by_union(&q, &graph, None).unwrap());
        assert_eq!(fast, solo.count_by_attached_sql(&q, &graph, None).unwrap());
    }
    assert_eq!(
        solo.count(&is_concept(CONCEPT_UI), &graph, None).unwrap(),
        2
    );
    assert_eq!(
        solo.count(&is_concept(CONCEPT_JS), &graph, None).unwrap(),
        1
    );
}

/// Hierarchy (DFS-interval `BETWEEN`) composes too — the fast path for a parent
/// concept is a rollup point-lookup in the base, and the corrections still have
/// to be applied on top of it.
#[test]
fn a_hierarchical_filter_composes() {
    fixture!((layers, _base, _overlay, graph));

    let q = Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(Expr::Concept {
            path: "topics".to_string(),
            op: ConceptOp::DescendantOrSelfOf,
            value: CONCEPT_PARENT.to_string(),
        }),
        measures: vec![Measure::CountRecords],
        limit: None,
    };

    // The parent spans both UI and JS, so every talk qualifies in the composed
    // view: A and B via the overlay's UI, C via the base's UI.
    assert_eq!(agreed_count(&layers, &q, &graph), 3);
}

// ---------------------------------------------------------------------------
// Tile composition (the hydration half), through the layered reader
// ---------------------------------------------------------------------------

/// The overlay's `topics` tile carries a tileid that appears nowhere in the base,
/// and it must STILL override — composition collapses cardinality-1 scopes on
/// the graph's CARDINALITY, not on tile identity. (Arches-minted ids never
/// collide across layers; if override needed matching ids, it would never fire.)
///
/// And because layers are PARTIAL, the base's `title` — which the overlay never
/// restates — has to survive.
#[test]
fn the_overlay_overrides_the_tile_it_restates_and_keeps_the_ones_it_does_not() {
    fixture!((layers, _base, _overlay, graph));

    let tree = layers.hydrate_resource(TALK_B, &graph).expect("hydrate");

    // Overridden: the overlay's topics won, though its tileid matched nothing.
    let topics = tree["topics"].as_array().expect("topics is a list");
    let ids: Vec<&str> = topics
        .iter()
        .map(|t| t["value_id"].as_str().or_else(|| t.as_str()).unwrap_or(""))
        .collect();
    assert!(
        !format!("{topics:?}").contains(VALUE_JS),
        "the base's JS topic must not survive the overlay's restatement: {topics:?}"
    );
    assert!(
        format!("{topics:?}").contains(VALUE_UI),
        "the overlay's UI topic must be present: {topics:?} (ids: {ids:?})"
    );

    // Preserved: the overlay never mentioned `title`, so the base's stands.
    assert_eq!(
        tree["title"]["en"]["value"], "Javascript, Typescript and Manuscript",
        "a partial overlay must not blank the tiles it did not restate"
    );

    // Exactly one topics tile: a cardinality-1 nodegroup that ends up holding two
    // is the silent-wrong-answer this composition exists to prevent (the hydrator
    // would pick between them arbitrarily).
    let tiles = layers.resource_tiles(TALK_B, &graph).unwrap();
    assert_eq!(
        tiles.iter().filter(|t| t.nodegroup_id == TOPICS_NG).count(),
        1,
        "cardinality-1 must hold exactly one tile after composition"
    );
}

/// A resource only the BASE defines hydrates untouched through the stack.
#[test]
fn a_resource_the_overlay_does_not_carry_hydrates_from_the_base() {
    fixture!((layers, _base, _overlay, graph));
    let tree = layers.hydrate_resource(TALK_C, &graph).expect("hydrate");
    assert_eq!(
        tree["title"]["en"]["value"],
        "A talk the overlay never touches"
    );
}

/// `defined_uuids` is the precedence primitive — "the topmost layer that DEFINES
/// a resource is authoritative for it" — so what each layer claims to define is
/// worth pinning directly, not just through its consequences.
#[test]
fn each_layer_defines_exactly_what_it_carries() {
    fixture!((layers, _base, _overlay, graph));

    let base_defines = layers.defined_uuids(0, &graph).unwrap();
    let overlay_defines = layers.defined_uuids(1, &graph).unwrap();

    assert_eq!(base_defines.len(), 3, "the base carries all three talks");
    assert!(base_defines.contains(TALK_C));

    assert_eq!(
        overlay_defines.len(),
        2,
        "the overlay carries only the two it edited — this is what bounds the \
         cost of the corrected count to O(overlay), not O(corpus)"
    );
    assert!(overlay_defines.contains(TALK_A) && overlay_defines.contains(TALK_B));
    assert!(
        !overlay_defines.contains(TALK_C),
        "and Talk C is NOT redefined, which is why the base stays authoritative for it"
    );
}

// ---------------------------------------------------------------------------
// Refusing to compose the incomposable
// ---------------------------------------------------------------------------

/// Layers minting URIs under different bases are not the same corpus. Composing
/// them would silently answer questions about a union of two unrelated worlds,
/// so `open` refuses.
#[test]
fn layers_from_different_base_uris_are_refused() {
    let Some(demo) = demo_data() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let base = emit("base-uri-a", &base_corpus(&demo));

    // Same data, different base_uri.
    let other_data = overlay_corpus(&demo);
    let other = scratch("base-uri-b-head");
    ros_madair_emit::emit(
        other_data.to_str().unwrap(),
        other.to_str().unwrap(),
        "https://elsewhere.invalid/",
    )
    .expect("emit");

    let err = Layers::open(&[base.as_path(), other.as_path()])
        .expect_err("differing base_uri must not compose");
    assert!(
        matches!(err, ReadError::Incompatible { ref what, .. } if what == "base_uri"),
        "{err}"
    );
    assert!(err.to_string().contains("not composable"));
}

/// A manifest is REQUIRED per layer: without one there is nothing to check
/// composability against, and silently composing two incompatible snapshots is
/// the failure this crate exists to prevent. (The single-snapshot read path
/// treats the manifest as an optional fast path — composition cannot.)
#[test]
fn a_layer_without_a_manifest_is_refused() {
    fixture!((_layers, base, _overlay, _graph));

    let crippled = scratch("no-manifest");
    copy_dir(&base, &crippled);
    std::fs::remove_file(crippled.join("manifest.json")).unwrap();

    let err = Layers::open(&[crippled.as_path()]).expect_err("no manifest must not compose");
    assert!(matches!(err, ReadError::MissingManifest(_)), "{err}");
    assert!(err.to_string().contains("composition requires one per"));
}

/// An empty stack is a caller bug, and a typed one.
#[test]
fn an_empty_stack_is_refused() {
    assert!(matches!(
        Layers::open(&[]).expect_err("empty"),
        ReadError::NoLayers
    ));
}

/// A model no layer carries cannot be queried — the compiled SQL would name a
/// spine table that does not exist. Better a typed error than a SQLite one.
#[test]
fn a_model_in_no_layer_is_a_typed_error() {
    fixture!((layers, _base, _overlay, _graph));

    // A syntactically valid graph the stack has never heard of.
    let stranger: StaticGraph = serde_json::from_value(json!({
        "graphid": "11111111-2222-3333-4444-555555555555",
        "name": {"en": "Stranger"},
        "root": {"nodeid": "r", "name": "R", "datatype": "semantic",
                 "graph_id": "11111111-2222-3333-4444-555555555555"},
        "nodes": [{"nodeid": "r", "name": "R", "datatype": "semantic",
                   "graph_id": "11111111-2222-3333-4444-555555555555"}],
        "nodegroups": [], "edges": []
    }))
    .unwrap();

    let q = Query {
        model: "11111111-2222-3333-4444-555555555555".to_string(),
        r#where: None,
        measures: vec![Measure::CountRecords],
        limit: None,
    };
    let err = layers
        .count(&q, &stranger, None)
        .expect_err("a model in no layer must not silently answer 0");
    assert!(matches!(err, ReadError::ModelInNoLayer(_)), "{err}");
}
