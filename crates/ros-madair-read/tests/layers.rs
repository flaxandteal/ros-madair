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
//! The demo Talk model has one concept card (`topics`). The tests add more to
//! their own copy of the model: `region` (cardinality-1, its own card), `tag`
//! (cardinality-n), and `topic_kind` — a SECOND node inside the `topics` card, the
//! one shape where whole-nodegroup override differs from per-node.
//!
//! | resource | BASE | OVERLAY |
//! |---|---|---|
//! | Talk A | topics `UI`,`MISC` + topic_kind `JS` · region `MISC` · tag `MISC` | topics `UI` (card restated) · tag `JS` |
//! | Talk B | topics `JS` | topics `UI` — **flipped out of JS** |
//! | Talk C | topics `UI` | *(absent)* |
//! | Talk D | topics `UI` | topics `null` — **retracted** |
//!
//! This breaks every naive composition:
//!
//! 1. **Double counting** — Talk A matches `UI` in BOTH layers. `Σ per-layer
//!    count` says 4; the truth is 3.
//! 2. **Flipping OUT** — the base says Talk B is a `JS` talk and the overlay says
//!    it is not. A union of per-layer matches counts it forever.
//! 3. **The base's verdict SURVIVING** — Talk C is untouched (its `region` card,
//!    which the overlay never carries, stays the base's), so the base stays
//!    authoritative for it.
//! 4. **RESOURCE-level precedence was wrong** — the overlay carries Talk A but
//!    not its `region` card; under per-RESOURCE precedence the overlay owned every
//!    field of Talk A, so `region = MISC` answered "no" and an `all` across cards
//!    owned by different layers matched in neither. The nodegroup rule answers each
//!    card by its owner.
//! 5. **NODEGROUP is the unit, not the node** — the overlay restates the `topics`
//!    card with `topics` only; its sibling `topic_kind` is BLANKED (whole-card
//!    override), where per-node would have kept the base's. This is the one place
//!    the granularity choice is observable.
//! 6. **RETRACTION** — Talk D's overlay ships a `topics` tile with no value, which
//!    still produces a `fragment_dir` row, so the overlay OWNS the (now empty)
//!    card and the base's `topics` does not leak through. An empty/absent tile is
//!    "carried, empty", distinct from "never mentioned".

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
/// A second filterable node sharing the TOPICS card with `topics`. Its whole
/// point is to be a sibling the overlay does NOT restate, so a whole-nodegroup
/// override blanks it — the one behaviour where atomic differs from per-node.
const TOPIC_KIND: &str = "5eaa0000-0000-4000-8000-000000000001";
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

/// Add a SECOND filterable node into an existing nodegroup, so that nodegroup has
/// two filterable fields sharing one card. This is the only shape where atomic
/// (whole-nodegroup) override behaves differently from per-node: the card is the
/// unit, so an overlay tile that sets one of the two blanks the other.
fn add_sibling_node(graph_path: &Path, ng: &str, node_id: &str, alias: &str) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": node_id,
        "nodegroup_id": ng,          // SAME nodegroup — a sibling in the card
        "name": alias,
        "alias": alias,
        "datatype": "concept-list",
        "config": { "rdmCollection": RDM_COLLECTION },
        "graph_id": TALK_GRAPH,
        "istopnode": false,
        "is_collector": false,
        "isrequired": false,
        "issearchable": true,
        "exportable": false,
        "sortorder": 1,
    }));
    // The sibling hangs off the nodegroup's collector node.
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": format!("{node_id}-edge"),
        "domainnode_id": ng,
        "rangenode_id": node_id,
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
    // `topic_kind` shares the TOPICS card with `topics` — the one nodegroup the
    // overlay RESTATES, so a whole-card override there blanks this sibling.
    add_sibling_node(graph_path, TOPICS_NG, TOPIC_KIND, "topic_kind");
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

/// A tile for a nodegroup that carries TWO nodes — for exercising a shared card.
fn two_node_tile(
    resource: &str,
    tileid: &str,
    ng: &str,
    node_a: &str,
    values_a: &[&str],
    node_b: &str,
    values_b: &[&str],
) -> serde_json::Value {
    json!({
        "tileid": tileid,
        "nodegroup_id": ng,
        "parenttile_id": null,
        "resourceinstance_id": resource,
        "sortorder": 0,
        "provisionaledits": null,
        "data": { node_a: values_a, node_b: values_b },
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
                    // The topics CARD carries two nodes: topics AND topic_kind.
                    // The overlay restates this card with topics only, so under
                    // whole-nodegroup override topic_kind is blanked.
                    two_node_tile(
                        TALK_A,
                        "aaaa1111-0000-4000-8000-000000000002",
                        TOPICS_NG,
                        TOPICS_NG,
                        &[VALUE_UI, VALUE_MISC],
                        TOPIC_KIND,
                        &[VALUE_JS],
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
    // Every filterable field in this fixture is the sole collector of its own
    // nodegroup, EXCEPT topic_kind (a sibling in TOPICS_NG), so the general form
    // takes both; the common case passes the node as its own nodegroup.
    let nodegroup = if node == TOPIC_KIND { TOPICS_NG } else { node };
    composed_concepts_in(f, uuid, nodegroup, node)
}

fn composed_concepts_in(f: &Fixture, uuid: &str, nodegroup: &str, node: &str) -> BTreeSet<String> {
    let tiles = f
        .layers
        .resource_tiles(uuid, &f.graph)
        .expect("compose tiles");
    tiles
        .iter()
        .filter(|t| t.nodegroup_id == nodegroup)
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

/// **The atomic-nodegroup rule, at the one shape where it DIFFERS from per-node.**
///
/// `topics` and `topic_kind` share ONE card (nodegroup). Base Talk A carries both
/// (`topics = [UI, MISC]`, `topic_kind = [JS]`); the overlay RESTATES that card
/// with `topics = [UI]` and no `topic_kind`. Under whole-nodegroup override the
/// overlay owns the whole card, so `topic_kind` is **blanked** — the sibling the
/// overlay omitted is gone, exactly as it would be for a user editing the card in
/// Arches and saving it.
///
/// Per-node precedence (the rejected alternative) would keep the base's
/// `topic_kind = JS`, and query and hydration would then disagree with the card
/// the user sees. This is the whole reason the choice mattered — every other test
/// puts each field in its own nodegroup, where the two rules coincide.
#[test]
fn a_shared_card_partially_restated_blanks_the_omitted_sibling() {
    fixture!(f);

    // Hydration oracle: the composed topics card has no topic_kind left.
    assert!(
        composed_concepts(&f, TALK_A, TOPIC_KIND).is_empty(),
        "the overlay restated the topics card, so its sibling topic_kind is gone"
    );

    // The base ALONE still finds it — so the overlay is what removes it, not a
    // fixture accident.
    let base_only = Layers::open(&[f.base_head.as_path()]).unwrap();
    assert_eq!(
        base_only
            .count(&query(is_concept("topic_kind", CONCEPT_JS)), &f.graph, None)
            .unwrap(),
        1,
        "the base alone has Talk A's topic_kind = JS"
    );

    // Composed query agrees with the composed card: nobody matches.
    let q = query(is_concept("topic_kind", CONCEPT_JS));
    let ids = f.layers.resolve(&q, &f.graph, None).unwrap();
    assert!(
        !ids.contains(&TALK_A.to_string()),
        "a filter on the blanked sibling must not match Talk A: {ids:?}"
    );
    assert_eq!(
        f.layers.count(&q, &f.graph, None).unwrap(),
        0,
        "whole-nodegroup override dropped topic_kind, so the composed count is 0"
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

/// A tile with no value is a RETRACTION, not silence. Talk D's overlay ships a
/// `topics` tile whose value is null; whole-nodegroup override lets that tile win,
/// so the composed resource has no topics and must not match one.
///
/// The tile still produces a `fragment_dir` row, which is what keeps "the overlay
/// retracted it" distinct from "the overlay never mentioned it": without the row
/// the query would fall back to the base and keep matching a topic no longer on
/// screen. (The row exists for an *empty* tile too, `{}` — null-valued and empty
/// retract identically.)
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

/// `fragment_dir` doubles as the nodegroup-presence index that drives per-
/// nodegroup precedence, so pin what it records — including the retraction tile,
/// which is the row that distinguishes "carried, empty" from "never mentioned".
///
/// (Each of these node ids is also its nodegroup's id — every filterable field in
/// this fixture is the sole collector of its own nodegroup — so the nodegroup
/// terms and the node terms coincide.)
#[test]
fn the_head_records_which_nodegroups_each_layer_carries() {
    fixture!(f);

    let ngs_for = |head: &Path, uuid: &str| -> BTreeSet<String> {
        let conn = rusqlite::Connection::open(head.join("head.sqlite")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT dn.term FROM fragment_dir fd
                   JOIN spine_talk s ON s.rid = fd.rid
                   JOIN dict dr ON dr.term_id = s.term_id
                   JOIN dict dn ON dn.term_id = fd.nodegroup
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

    // The base's fragment_dir carries every nodegroup, filterable or not; restrict
    // to the three we filter on so the assertion is about presence, not layout.
    let filterable = BTreeSet::from([
        TOPICS_NG.to_string(),
        REGION_NG.to_string(),
        TAG_NG.to_string(),
    ]);
    let base = &ngs_for(&f.base_head, TALK_A) & &filterable;
    assert_eq!(
        base, filterable,
        "the base carries all three filterable cards"
    );

    let overlay = &ngs_for(&f.overlay_head, TALK_A) & &filterable;
    assert_eq!(
        overlay,
        BTreeSet::from([TOPICS_NG.to_string(), TAG_NG.to_string()]),
        "the overlay carries topics and tag — but says NOTHING about region, and \
         that silence is what the base's verdict survives on"
    );

    // The retraction tile (Talk D's null topics) still produces a fragment_dir
    // row: an empty/retracting tile is CARRIED, not silence.
    assert!(
        ngs_for(&f.overlay_head, TALK_D).contains(TOPICS_NG),
        "a retraction is a carried nodegroup — without the row it would read as \
         'never mentioned' and the base would leak through"
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

/// **A5.** A handler-VERSION delta must NOT block composition; a
/// handler-PROVIDER delta must.
///
/// Composition identity is `(datatype, provider)` — WHAT indexes a field, not
/// the exact build that did it. `HandlerDecl.version` is provenance only. This
/// is live for the on-device model: a device overlay built against a newer app
/// (bumped clm-core) has to compose with its OWN shipped base, which it cannot
/// re-emit. Exact-version matching made that a hard `Incompatible`.
#[test]
fn a_handler_version_delta_composes_but_a_provider_delta_does_not() {
    fixture!(f);

    // Two real heads of the same corpus; patch the overlay's handler `version`.
    let overlay = scratch("hv-overlay");
    copy_dir(&f.overlay_head, &overlay);

    let patch_handlers = |dir: &Path, edit: &dyn Fn(&mut serde_json::Value)| {
        let p = dir.join("manifest.json");
        let mut m: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        for h in m["handlers"].as_array_mut().expect("handlers array") {
            edit(h);
        }
        std::fs::write(&p, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    };

    // A different VERSION — the clm-core patch-bump case — must still compose.
    patch_handlers(&overlay, &|h| {
        h["version"] = json!("99.99.99-does-not-matter")
    });
    Layers::open(&[f.base_head.as_path(), overlay.as_path()])
        .expect("a handler version delta must NOT block composition");

    // A different PROVIDER — a genuinely different thing indexing the field —
    // must still refuse: that IS a real incompatibility.
    let bad_provider = scratch("hv-bad-provider");
    copy_dir(&f.overlay_head, &bad_provider);
    patch_handlers(&bad_provider, &|h| {
        h["provider"] = json!("some-other-crate")
    });
    let err = Layers::open(&[f.base_head.as_path(), bad_provider.as_path()])
        .expect_err("a provider delta is a real incompatibility");
    assert!(
        matches!(err, ReadError::Incompatible { ref what, .. } if what == "handlers"),
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
