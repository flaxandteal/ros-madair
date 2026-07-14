// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Multi-layer composition (R1), end to end.** Two REAL snapshots, emitted by
//! the real emitter from two real corpora, read back as one composed view.
//!
//! # The oracle
//!
//! Every count is cross-checked against what the user would actually SEE: the
//! filter, evaluated against the **composed hydrated tiles**. That oracle shares
//! no code with the head, the index, or the query compiler — it merges tiles and
//! reads values. **Query and hydration agreeing is the whole invariant**, and it
//! is precisely what per-resource precedence broke.
//!
//! # The corpus, and why it is shaped like this
//!
//! The demo Talk model has one concept node (`topics`). The tests add a second,
//! `region`, to their own copy of the model — because the decisive case needs a
//! field that **only the base carries**, on a resource the overlay **does**
//! carry, and one filterable field cannot express it.
//!
//! | resource | BASE | OVERLAY |
//! |---|---|---|
//! | Talk A | topics `UI`,`MISC` · region `MISC` · title | topics `UI` — *no region, no title* |
//! | Talk B | topics `JS` · title | topics `UI` — **flipped out of JS** |
//! | Talk C | topics `UI` · title | *(absent)* |
//! | Talk D | topics `UI` · title | topics `null` — **retracted** |
//!
//! This breaks every naive composition:
//!
//! 1. **Double counting** — Talk A matches `UI` in BOTH layers. `Σ per-layer
//!    count` says 4; the truth is 3.
//! 2. **Flipping OUT** — the base says Talk B is a `JS` talk and the overlay says
//!    it is not. A union of per-layer matches counts it forever.
//! 3. **The base's verdict SURVIVING** — Talk C is untouched, so the base stays
//!    authoritative for it.
//! 4. **PARTIAL layers (the bug this fixes)** — the overlay carries Talk A but
//!    says NOTHING about its `region`. Under per-RESOURCE precedence the overlay
//!    became authoritative for every field of Talk A, so `region = MISC` answered
//!    "no" and the base's correct verdict was discarded. Worse, an `all` across
//!    `topics` (overlay) and `region` (base) matched in NEITHER layer, though the
//!    composed resource on screen satisfies both.
//! 5. **RETRACTION** — Talk D's overlay tile sets `topics` to null. A null is not
//!    silence: the merge sees the key and lets it win. If `node_presence` did not
//!    record nulls, the composed query would fall through to the base and keep
//!    matching a topic the user can no longer see.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_query::{ConceptOp, Expr, Measure, Query};
use ros_madair_read::{Layers, ReadError};
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";

const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const TOPICS_NG: &str = "3784d67d-1ae9-11f0-86d0-a32be8fb5c91";
const TITLE_NG: &str = "7d8e443d-1ae0-11f0-8c5c-8fd6f4eb1a02";
/// A SECOND concept node, added by these tests to their copy of the Talk model.
/// The decisive partial-layer case needs a field only the base carries.
const REGION_NG: &str = "5e910000-0000-4000-8000-000000000001";
/// A cardinality-N concept node. Layers ACCUMULATE here rather than overriding:
/// the merge keeps every layer's tiles, so a value the base wrote must survive an
/// overlay writing to the same node. The override rule would silently drop it.
const TAG_NG: &str = "5e920000-0000-4000-8000-000000000001";
/// The collection `topics` draws on; `region` reuses it, so the same vocabulary
/// (and the same DFS intervals) serve both.
const RDM_COLLECTION: &str = "10f1b99b-80c8-49a6-b825-440d8d2ced37";

const TALK_A: &str = "179b8583-3140-437e-bd0b-34d5aa2f1550";
const TALK_B: &str = "97cc9a1b-ee42-412e-9fa5-203d98bff815";
const TALK_C: &str = "0d1e2f3a-4b5c-6d7e-8f90-a1b2c3d4e5f6";
const TALK_D: &str = "0d1e2f3a-4b5c-6d7e-8f90-a1b2c3d4e5f7";

// A `concept-list` tile stores concept *value* ids; the emitter resolves them
// through SKOS to *concept* ids, and it is the CONCEPT id a filter names. Two id
// spaces, and mixing them yields a silent zero.
const VALUE_UI: &str = "f71cff66-5421-4420-b06e-397376eb2f56";
const VALUE_MISC: &str = "152fe3a0-f1de-43e4-9897-026587cee523";
const VALUE_JS: &str = "08d28d3b-9c92-4004-b7ee-ed131a348e92";

const CONCEPT_UI: &str = "7c3226cf-4d13-41b4-8a9d-7c26a058fd44";
const CONCEPT_MISC: &str = "de87aac2-8560-4609-9f54-5e27c990f9ce";
const CONCEPT_JS: &str = "3cce47ca-eaec-4718-aca4-ba55902466c6";
/// Ancestor of `CONCEPT_UI` and `CONCEPT_JS` in the vocab's DFS interval — but
/// NOT of `CONCEPT_MISC`, which hangs off a different parent.
const CONCEPT_PARENT: &str = "d6435de5-acdb-4ef3-8181-0bd035e5d5c6";

fn concept_of(value: &str) -> &'static str {
    match value {
        VALUE_UI => CONCEPT_UI,
        VALUE_MISC => CONCEPT_MISC,
        VALUE_JS => CONCEPT_JS,
        other => panic!("unmapped concept value {other}"),
    }
}

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

/// Add a concept node to the corpus's copy of the Talk model: a clone of `topics`
/// (same datatype, same collection) in its own nodegroup, hung off the same root.
///
/// The tests add two, and the CARDINALITY is the point of the second:
///
/// - `region`, cardinality **1** — an OVERRIDE field, and the one the overlay
///   never mentions (which is how the partial-layer bug is caught);
/// - `tag`, cardinality **n** — an ADDITIVE field, where the merge keeps EVERY
///   layer's tiles, so the composed values are the union and a lower layer's
///   value must survive a higher layer writing to the same node.
fn add_concept_node(graph_path: &Path, ng: &str, alias: &str, cardinality: &str, edge: &str) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": ng,
        "nodegroup_id": ng,
        "name": alias,
        "alias": alias,
        "datatype": "concept-list",
        "config": { "rdmCollection": RDM_COLLECTION },
        "graph_id": TALK_GRAPH,
        "istopnode": false,
        "is_collector": true,
        "isrequired": false,
        "issearchable": true,
        "exportable": false,
        "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": ng,
        "cardinality": cardinality,
        "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": edge,
        "domainnode_id": TALK_ROOT,
        "rangenode_id": ng,
        "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn extend_talk_model(graph_path: &Path) {
    add_concept_node(
        graph_path,
        REGION_NG,
        "region",
        "1",
        "5e910000-0000-4000-8000-0000000000ee",
    );
    add_concept_node(
        graph_path,
        TAG_NG,
        "tag",
        "n",
        "5e920000-0000-4000-8000-0000000000ee",
    );
}

fn concept_tile(
    resource: &str,
    tileid: &str,
    ng: &str,
    values: Option<&[&str]>,
) -> serde_json::Value {
    json!({
        "tileid": tileid,
        "nodegroup_id": ng,
        "parenttile_id": null,
        "resourceinstance_id": resource,
        "sortorder": 0,
        "provisionaledits": null,
        // `None` writes an explicit NULL: the key is present, the value is not.
        // That is a RETRACTION, and it must not read as silence.
        "data": { ng: values },
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

fn corpus(
    tag: &str,
    demo: &Path,
    talks: Vec<serde_json::Value>,
    keep_other_models: bool,
) -> PathBuf {
    let dir = scratch(tag);
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    extend_talk_model(&dir.join("graphs").join(format!("{TALK_GRAPH}.json")));
    if keep_other_models {
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
                    concept_tile(
                        TALK_A,
                        "aaaa1111-0000-4000-8000-000000000002",
                        TOPICS_NG,
                        Some(&[VALUE_UI, VALUE_MISC]),
                    ),
                    // The field the overlay will say NOTHING about.
                    concept_tile(
                        TALK_A,
                        "aaaa1111-0000-4000-8000-000000000003",
                        REGION_NG,
                        Some(&[VALUE_MISC]),
                    ),
                    // A cardinality-N field. The overlay will ALSO write a tag
                    // tile for Talk A — a SEPARATE tile, which the merge keeps
                    // alongside this one. Both values must remain findable.
                    concept_tile(
                        TALK_A,
                        "aaaa1111-0000-4000-8000-000000000004",
                        TAG_NG,
                        Some(&[VALUE_MISC]),
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
                    concept_tile(
                        TALK_B,
                        "bbbb2222-0000-4000-8000-000000000002",
                        TOPICS_NG,
                        Some(&[VALUE_JS]),
                    ),
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
                    concept_tile(
                        TALK_C,
                        "cccc3333-0000-4000-8000-000000000002",
                        TOPICS_NG,
                        Some(&[VALUE_UI]),
                    ),
                ],
            ),
            resource(
                TALK_D,
                "A talk whose topics get retracted",
                vec![
                    title_tile(
                        TALK_D,
                        "dddd4444-0000-4000-8000-000000000001",
                        "A talk whose topics get retracted",
                    ),
                    concept_tile(
                        TALK_D,
                        "dddd4444-0000-4000-8000-000000000002",
                        TOPICS_NG,
                        Some(&[VALUE_UI]),
                    ),
                ],
            ),
        ],
        true,
    )
}

/// The overlay: `topics` ONLY, with tileids matching nothing in the base. It says
/// nothing whatever about `region`, and nothing at all about Talk C.
fn overlay_corpus(demo: &Path) -> PathBuf {
    corpus(
        "overlay",
        demo,
        vec![
            resource(
                TALK_A,
                "3 W's of UI",
                vec![
                    concept_tile(
                        TALK_A,
                        "9999ffff-0000-4000-8000-00000000000a",
                        TOPICS_NG,
                        Some(&[VALUE_UI]),
                    ),
                    // Cardinality-N: this ADDS a tag; it does not replace the
                    // base's. Both tiles survive the merge, so both concepts are
                    // in the composed view — and a query must find EITHER.
                    concept_tile(
                        TALK_A,
                        "9999ffff-0000-4000-8000-00000000000c",
                        TAG_NG,
                        Some(&[VALUE_JS]),
                    ),
                ],
            ),
            resource(
                TALK_B,
                "Javascript, Typescript and Manuscript",
                vec![concept_tile(
                    TALK_B,
                    "9999ffff-0000-4000-8000-00000000000b",
                    TOPICS_NG,
                    Some(&[VALUE_UI]),
                )],
            ),
            resource(
                TALK_D,
                "A talk whose topics get retracted",
                // NULL: the key is present, the value is gone. A retraction.
                vec![concept_tile(
                    TALK_D,
                    "9999ffff-0000-4000-8000-00000000000d",
                    TOPICS_NG,
                    None,
                )],
            ),
        ],
        false,
    )
}

fn talk_graph(corpus_dir: &Path) -> StaticGraph {
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(corpus_dir.join("graphs").join(format!("{TALK_GRAPH}.json"))).unwrap(),
    )
    .unwrap();
    let value = raw
        .get("graph")
        .and_then(|g| g.get(0))
        .cloned()
        .unwrap_or(raw);
    serde_json::from_value(value).unwrap()
}

struct Fixture {
    layers: Layers,
    base_head: PathBuf,
    overlay_head: PathBuf,
    graph: StaticGraph,
}

fn stack() -> Option<Fixture> {
    let demo = demo_data()?;
    let base_data = base_corpus(&demo);
    let overlay_data = overlay_corpus(&demo);
    let base_head = emit("base", &base_data);
    let overlay_head = emit("overlay", &overlay_data);
    let layers = Layers::open(&[base_head.as_path(), overlay_head.as_path()]).expect("open layers");
    Some(Fixture {
        layers,
        base_head,
        overlay_head,
        graph: talk_graph(&base_data),
    })
}

macro_rules! fixture {
    ($binding:ident) => {
        let Some($binding) = stack() else {
            eprintln!("demo fixture absent — skipping");
            return;
        };
    };
}

// ---------------------------------------------------------------------------
// The ORACLE: what the user actually sees
// ---------------------------------------------------------------------------

/// The concepts a resource carries on `node`, **in the composed view** — read out
/// of the merged tiles, not out of any index.
///
/// An absent tile and a null value both yield the empty set, which is right: a
/// resource with no value for a node cannot match a predicate on it.
fn composed_concepts(f: &Fixture, uuid: &str, node: &str) -> BTreeSet<String> {
    let tiles = f
        .layers
        .resource_tiles(uuid, &f.graph)
        .expect("compose tiles");
    tiles
        .iter()
        .filter(|t| t.nodegroup_id == node)
        .filter_map(|t| t.data.get(node))
        .filter_map(|v| v.as_array())
        .flatten()
        .map(|v| concept_of(v.as_str().expect("concept value")).to_string())
        .collect()
}

fn every_resource(f: &Fixture) -> BTreeSet<String> {
    let mut all = BTreeSet::new();
    for i in 0..f.layers.len() {
        all.extend(f.layers.defined_uuids(i, &f.graph).expect("defined"));
    }
    all
}

/// **The independent oracle.** Evaluate the predicate against the composed
/// HYDRATED tiles of every resource in the stack — the values on screen.
///
/// This shares no code with the head, the index or the query compiler. If the
/// composed query and this disagree, then what the user filters by and what the
/// user is looking at are two different things, which is the entire bug class.
fn by_hydration(
    f: &Fixture,
    pred: impl Fn(&BTreeSet<String>, &BTreeSet<String>, &BTreeSet<String>) -> bool,
) -> BTreeSet<String> {
    every_resource(f)
        .into_iter()
        .filter(|u| {
            pred(
                &composed_concepts(f, u, TOPICS_NG),
                &composed_concepts(f, u, REGION_NG),
                &composed_concepts(f, u, TAG_NG),
            )
        })
        .collect()
}

/// Run a query three ways — the fast corrected count, the resolve-and-count
/// reference, and the hydration oracle — and demand they all agree.
fn agreed(
    f: &Fixture,
    query: &Query,
    pred: impl Fn(&BTreeSet<String>, &BTreeSet<String>, &BTreeSet<String>) -> bool,
) -> usize {
    let fast = f.layers.count(query, &f.graph, None).expect("count");
    let union = f
        .layers
        .count_by_union(query, &f.graph, None)
        .expect("count_by_union");
    let mut ids = f.layers.resolve(query, &f.graph, None).expect("resolve");
    ids.sort();

    let oracle = by_hydration(f, pred);
    let expected: Vec<String> = oracle.iter().cloned().collect();

    assert_eq!(
        ids, expected,
        "the composed QUERY and the composed HYDRATION disagree about WHICH \
         resources match — the user would filter by one thing and look at another"
    );
    assert_eq!(
        fast,
        oracle.len(),
        "the fast corrected count disagrees with the hydration oracle"
    );
    assert_eq!(
        union,
        oracle.len(),
        "the resolve-and-count reference disagrees with the hydration oracle"
    );
    fast
}

fn is_concept(path: &str, concept: &str) -> Expr {
    Expr::Concept {
        path: path.to_string(),
        op: ConceptOp::Is,
        value: concept.to_string(),
    }
}

fn query(expr: Expr) -> Query {
    Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(expr),
        measures: vec![Measure::CountRecords],
        limit: None,
    }
}

// ---------------------------------------------------------------------------
// THE PARTIAL-LAYER BUG: precedence is per-NODE, not per-resource
// ---------------------------------------------------------------------------

/// **The bug, in one test.** The overlay carries Talk A — but says NOTHING about
/// its `region`. The base's `region = MISC` must therefore stand.
///
/// Under per-RESOURCE precedence, the overlay "defined" Talk A and so became
/// authoritative for every field of it. Its head never indexed `region` (it never
/// saw a region tile), so the filter answered "no" there, and the base's correct
/// verdict was DISCARDED. The composed count was 0 while the composed resource on
/// screen plainly had `region = MISC`.
#[test]
fn a_filter_on_a_field_the_overlay_does_not_carry_reads_from_the_base() {
    fixture!(f);

    assert_eq!(
        composed_concepts(&f, TALK_A, REGION_NG),
        BTreeSet::from([CONCEPT_MISC.to_string()]),
        "precondition: the composed Talk A really does still have region = MISC"
    );

    let n = agreed(
        &f,
        &query(is_concept("region", CONCEPT_MISC)),
        |_t, r, _g| r.contains(CONCEPT_MISC),
    );
    assert_eq!(
        n, 1,
        "Talk A matches on a field only the BASE carries, though an overlay \
         carries the resource"
    );
}

/// **The other half of the bug.** An `all` across two fields owned by DIFFERENT
/// layers: `topics` (the overlay's) and `region` (the base's).
///
/// Under per-resource precedence this matched in NEITHER layer — the overlay
/// lacked `region`, the base's `topics` was stale — and so composed to zero,
/// while the resource the user is looking at satisfies both conjuncts.
#[test]
fn an_and_across_fields_owned_by_different_layers_matches() {
    fixture!(f);

    let q = query(Expr::All(vec![
        is_concept("topics", CONCEPT_UI), // the OVERLAY owns topics for Talk A
        is_concept("region", CONCEPT_MISC), // the BASE owns region for Talk A
    ]));

    let n = agreed(&f, &q, |t, r, _g| {
        t.contains(CONCEPT_UI) && r.contains(CONCEPT_MISC)
    });
    assert_eq!(
        n, 1,
        "Talk A satisfies both conjuncts in the composed view — one from each layer"
    );
}

/// **Cardinality-N is ADDITIVE, and "topmost defining layer wins" would be a
/// second silent wrong answer.**
///
/// `tag` is a cardinality-n nodegroup. The base gives Talk A a tag tile carrying
/// `MISC`; the overlay gives it a SEPARATE tag tile carrying `JS`. The merge never
/// collapses cardinality-n tiles, so BOTH survive and the composed Talk A carries
/// both tags — which is what the user sees.
///
/// A composed query must therefore find Talk A under EITHER. If it applied the
/// override rule here — "the overlay defines `tag`, so ask only the overlay" — it
/// would answer `tag = MISC` with a flat no, while the hydrated resource plainly
/// shows MISC. Same class of bug as per-resource precedence, one level down: the
/// composition rule has to be chosen by CARDINALITY, from the graph, which is the
/// same source alizarin's merge reads it from.
#[test]
fn a_cardinality_n_field_accumulates_across_layers_rather_than_overriding() {
    fixture!(f);

    // What the user sees: both tags, one from each layer.
    assert_eq!(
        composed_concepts(&f, TALK_A, TAG_NG),
        BTreeSet::from([CONCEPT_MISC.to_string(), CONCEPT_JS.to_string()]),
        "precondition: cardinality-n keeps BOTH layers' tiles"
    );

    // The BASE's tag must still be findable, though the overlay also wrote a tag.
    let n = agreed(&f, &query(is_concept("tag", CONCEPT_MISC)), |_t, _r, g| {
        g.contains(CONCEPT_MISC)
    });
    assert_eq!(
        n, 1,
        "the base's tag survives the overlay writing to the same node — \
         additive, not override"
    );

    // And the overlay's, obviously.
    let n = agreed(&f, &query(is_concept("tag", CONCEPT_JS)), |_t, _r, g| {
        g.contains(CONCEPT_JS)
    });
    assert_eq!(n, 1, "and so does the overlay's");
}

/// A null value is a RETRACTION, not silence. Talk D's overlay tile sets `topics`
/// to null; the merge lets the null win, so the composed resource has no topics
/// and must not match one.
///
/// This is what forces `node_presence` to record NULL-valued nodes: without those
/// rows, "the overlay retracted it" is indistinguishable from "the overlay never
/// mentioned it", and the query would fall back to the base and keep matching a
/// topic that is no longer on screen.
#[test]
fn a_null_in_an_overlay_retracts_rather_than_abstains() {
    fixture!(f);

    assert!(
        composed_concepts(&f, TALK_D, TOPICS_NG).is_empty(),
        "precondition: the composed Talk D has no topics left"
    );

    // Talk D had UI in the base. It must not be counted.
    let ids = f
        .layers
        .resolve(&query(is_concept("topics", CONCEPT_UI)), &f.graph, None)
        .unwrap();
    assert!(
        !ids.contains(&TALK_D.to_string()),
        "a retracted topic must not still match: {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// The three ways naive composition breaks
// ---------------------------------------------------------------------------

/// **Double counting** and **the base's verdict surviving**, in one query.
///
/// `UI` matches Talk A in BOTH layers (naively 2 + 3 = 5), Talk C only in the
/// base (and must not be lost), and Talk D not at all (retracted).
#[test]
fn a_resource_matching_in_two_layers_is_counted_once_and_base_only_matches_survive() {
    fixture!(f);

    let base_only = Layers::open(&[f.base_head.as_path()]).unwrap();
    assert_eq!(
        base_only
            .count(&query(is_concept("topics", CONCEPT_UI)), &f.graph, None)
            .unwrap(),
        3,
        "base alone: Talk A, Talk C and Talk D"
    );

    let n = agreed(&f, &query(is_concept("topics", CONCEPT_UI)), |t, _r, _g| {
        t.contains(CONCEPT_UI)
    });
    assert_eq!(
        n, 3,
        "Talk A (both layers, ONCE), Talk B (overlay), Talk C (base only); \
         Talk D retracted"
    );
}

/// **Flipping OUT.** The base says Talk B is a `JS` talk. The overlay redefines
/// its topics, and it is no longer one. A union of per-layer matches would keep
/// counting it forever — and the stale hit looks exactly like a real one.
#[test]
fn a_resource_the_overlay_flipped_out_of_the_filter_is_not_counted() {
    fixture!(f);

    let base_only = Layers::open(&[f.base_head.as_path()]).unwrap();
    assert_eq!(
        base_only
            .count(&query(is_concept("topics", CONCEPT_JS)), &f.graph, None)
            .unwrap(),
        1,
        "the base, alone, really does still say Talk B is a JS talk"
    );

    let n = agreed(&f, &query(is_concept("topics", CONCEPT_JS)), |t, _r, _g| {
        t.contains(CONCEPT_JS)
    });
    assert_eq!(n, 0, "but the composed view has no JS talks");
}

/// A concept the overlay merely DROPPED, rather than replaced. Talk A had `MISC`
/// in the base; the overlay's `topics` restatement omits it. Nothing announces
/// the removal — the tile is simply different.
///
/// Note this is `topics`, where the overlay DOES speak. Contrast
/// `a_filter_on_a_field_the_overlay_does_not_carry_reads_from_the_base`, where it
/// does not, and the base survives. Same resource, same concept, opposite answers
/// — and the ONLY thing that distinguishes them is per-node presence.
#[test]
fn a_concept_the_overlay_dropped_from_a_field_it_owns_is_not_counted() {
    fixture!(f);

    let n = agreed(
        &f,
        &query(is_concept("topics", CONCEPT_MISC)),
        |t, _r, _g| t.contains(CONCEPT_MISC),
    );
    assert_eq!(n, 0, "the overlay owns topics, and its topics have no MISC");

    // …while MISC on `region`, which the overlay does NOT own, still matches.
    let n = agreed(
        &f,
        &query(is_concept("region", CONCEPT_MISC)),
        |_t, r, _g| r.contains(CONCEPT_MISC),
    );
    assert_eq!(n, 1);
}

/// A single layer must compose to exactly itself — no precedence machinery may
/// alter the answer when there is nothing to override. This is also the path a
/// consumer takes before the user has any device layer at all, so a regression
/// here breaks the common case, not the exotic one.
#[test]
fn one_layer_composes_to_itself() {
    fixture!(f);
    let solo = Layers::open(&[f.base_head.as_path()]).unwrap();

    for (path, concept, expect) in [
        ("topics", CONCEPT_UI, 3),
        ("topics", CONCEPT_JS, 1),
        ("topics", CONCEPT_MISC, 1),
        ("region", CONCEPT_MISC, 1),
    ] {
        let q = query(is_concept(path, concept));
        let fast = solo.count(&q, &f.graph, None).unwrap();
        assert_eq!(fast, expect, "{path} is {concept}");
        assert_eq!(fast, solo.count_by_union(&q, &f.graph, None).unwrap());
    }
}

/// Hierarchy (DFS-interval `BETWEEN`) composes too — the base's fast path for a
/// parent concept is a rollup point-lookup, and the corrections still apply on
/// top of it.
#[test]
fn a_hierarchical_filter_composes() {
    fixture!(f);

    let q = query(Expr::Concept {
        path: "topics".to_string(),
        op: ConceptOp::DescendantOrSelfOf,
        value: CONCEPT_PARENT.to_string(),
    });

    // The parent spans UI and JS, but NOT misc.
    let n = agreed(&f, &q, |t, _r, _g| {
        t.contains(CONCEPT_UI) || t.contains(CONCEPT_JS)
    });
    assert_eq!(n, 3, "Talks A, B and C — D's topics were retracted");
}

// ---------------------------------------------------------------------------
// Premises and structure
// ---------------------------------------------------------------------------

/// **The load-bearing premise, asserted rather than assumed.** Each head interns
/// its own terms sequentially, so the same uuid has different `term_id`s in
/// different layers. Nothing may be joined across layers but the UUID *string*.
#[test]
fn layers_do_not_share_a_dictionary() {
    fixture!(f);

    let term_id = |head: &Path, term: &str| -> Option<i64> {
        let conn = rusqlite::Connection::open(head.join("head.sqlite")).unwrap();
        conn.query_row("SELECT term_id FROM dict WHERE term = ?1", [term], |r| {
            r.get(0)
        })
        .ok()
    };

    let base_talk = term_id(&f.base_head, TALK_B).expect("talk B in base dict");
    let overlay_talk = term_id(&f.overlay_head, TALK_B).expect("talk B in overlay dict");
    let base_concept = term_id(&f.base_head, CONCEPT_UI).expect("UI in base dict");
    let overlay_concept = term_id(&f.overlay_head, CONCEPT_UI).expect("UI in overlay dict");

    assert!(
        base_talk != overlay_talk || base_concept != overlay_concept,
        "term ids coincided across layers ({base_talk}/{overlay_talk}, \
         {base_concept}/{overlay_concept}) — this is not a guarantee, and no \
         query may ever rely on it"
    );
}

/// `node_presence` is what makes per-node precedence possible, so pin what it
/// actually records — including the NULL, which is the row that distinguishes a
/// retraction from silence.
#[test]
fn the_head_records_which_nodes_each_layer_carries() {
    fixture!(f);

    let nodes_for = |head: &Path, uuid: &str| -> BTreeSet<String> {
        let conn = rusqlite::Connection::open(head.join("head.sqlite")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT dn.term FROM node_presence np
                   JOIN spine_talk s ON s.rid = np.rid
                   JOIN dict dr ON dr.term_id = s.term_id
                   JOIN dict dn ON dn.term_id = np.node
                  WHERE dr.term = ?1",
            )
            .unwrap();
        let rows = stmt
            .query_map([uuid], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows
    };

    assert_eq!(
        nodes_for(&f.base_head, TALK_A),
        BTreeSet::from([
            TOPICS_NG.to_string(),
            REGION_NG.to_string(),
            TAG_NG.to_string()
        ]),
        "the base carries all three filterable fields of Talk A"
    );
    assert_eq!(
        nodes_for(&f.overlay_head, TALK_A),
        BTreeSet::from([TOPICS_NG.to_string(), TAG_NG.to_string()]),
        "the overlay carries topics and tag — but says NOTHING about region, and \
         that silence is what the base's verdict survives on"
    );
    assert_eq!(
        nodes_for(&f.overlay_head, TALK_D),
        BTreeSet::from([TOPICS_NG.to_string()]),
        "a NULL topics still counts as CARRIED: it retracts, and a retraction \
         that left no row would be indistinguishable from silence"
    );
}

/// Tile composition: the overlay's `topics` tile carries a tileid that appears
/// nowhere in the base and must STILL override — composition collapses
/// cardinality-1 scopes on the graph's CARDINALITY, not on tile identity. And
/// because layers are partial, the base's `title` and `region` survive.
#[test]
fn the_overlay_overrides_the_tile_it_restates_and_keeps_the_ones_it_does_not() {
    fixture!(f);

    let tree = f
        .layers
        .hydrate_resource(TALK_A, &f.graph)
        .expect("hydrate");

    let topics = format!("{:?}", tree["topics"]);
    assert!(
        !topics.contains(VALUE_MISC),
        "the base's MISC topic must not survive the overlay's restatement: {topics}"
    );
    assert!(
        topics.contains(VALUE_UI),
        "the overlay's UI topic must be there"
    );

    assert_eq!(
        tree["title"]["en"]["value"], "3 W's of UI",
        "a partial overlay must not blank a tile it did not restate"
    );
    assert!(
        format!("{:?}", tree["region"]).contains(VALUE_MISC),
        "nor a field it never mentioned"
    );

    let tiles = f.layers.resource_tiles(TALK_A, &f.graph).unwrap();
    assert_eq!(
        tiles.iter().filter(|t| t.nodegroup_id == TOPICS_NG).count(),
        1,
        "cardinality-1 must hold exactly one tile after composition"
    );
}

/// `defined_uuids` bounds the cost of composition: everything expensive ranges
/// over what the OVERLAYS touch, not over the corpus.
#[test]
fn each_layer_defines_exactly_what_it_carries() {
    fixture!(f);

    let base = f.layers.defined_uuids(0, &f.graph).unwrap();
    let overlay = f.layers.defined_uuids(1, &f.graph).unwrap();

    assert_eq!(base.len(), 4);
    assert_eq!(
        overlay.len(),
        3,
        "the overlay carries only what it edited — this is what bounds the \
         corrected count to O(overlay), not O(corpus)"
    );
    assert!(!overlay.contains(TALK_C), "and Talk C is untouched");
}

/// Layers minting URIs under different bases are not the same corpus.
#[test]
fn layers_from_different_base_uris_are_refused() {
    let Some(demo) = demo_data() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let base = emit("base-uri-a", &base_corpus(&demo));

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
}

/// A manifest is REQUIRED per layer: without one there is nothing to check
/// composability against, and silently composing two incompatible snapshots is
/// the failure this crate exists to prevent.
#[test]
fn a_layer_without_a_manifest_is_refused() {
    fixture!(f);

    let crippled = scratch("no-manifest");
    copy_dir(&f.base_head, &crippled);
    std::fs::remove_file(crippled.join("manifest.json")).unwrap();

    let err = Layers::open(&[crippled.as_path()]).expect_err("no manifest must not compose");
    assert!(matches!(err, ReadError::MissingManifest(_)), "{err}");
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
    fixture!(f);

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
    let err = f
        .layers
        .count(&q, &stranger, None)
        .expect_err("a model in no layer must not silently answer 0");
    assert!(matches!(err, ReadError::ModelInNoLayer(_)), "{err}");
}
