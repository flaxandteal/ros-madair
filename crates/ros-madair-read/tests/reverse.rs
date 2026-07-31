// SPDX-License-Identifier: AGPL-3.0-or-later
//! **A7 / RM principle P12: cross-layer reverse traversal (`Layers::cited_by`).**
//!
//! The regression this exists for: opening resource B and asking "who cites me?"
//! must find a citer A **even when A and B live in different heads** (A an
//! on-device overlay entry, B the shipped base). v1 did this reverse-cognate
//! lookup and bridged the dialect continuum; v2 dropped the traversal (the data —
//! `chunk_link_summary` — was still there, symmetric; only the operation was
//! missing). This is that operation, tested both ways it composes:
//!
//! - **cardinality-n (additive)** — the cognate case: the citer set is the UNION
//!   across layers, and an inbound link cannot be retracted. `cited_by` must
//!   return citers from BOTH the base and the overlay, and exclude a coarse
//!   false positive (a resource whose link points elsewhere but lands in the
//!   same summary range).
//! - **cardinality-1 (override)** — an overlay that RESTATES a citer's link card
//!   retargets the citation: the citer no longer cites its old target and now
//!   cites the new one. This mirrors the forward "flip out" case, in reverse.
//!
//! Same synthetic-corpus harness as `spatial.rs`/`layers.rs`. Demo fixture lives
//! outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it, absent → skip.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_read::{Layers, ReadError};
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";

// cardinality-n link node (the cognate continuum: a resource cites several).
const COGNATE_NG: &str = "5efd0000-0000-4000-8000-00000000000a";
// cardinality-1 link node (a single citation an overlay can retarget).
const COGNATE1_NG: &str = "5efd0000-0000-4000-8000-00000000000b";

// Targets — real resources in the BASE.
const B: &str = "b0000000-0000-4000-8000-000000000001"; // the cited entry
const D: &str = "d0000000-0000-4000-8000-000000000001"; // a second target
// Citers.
const C: &str = "c0000000-0000-4000-8000-000000000001"; // base, cognate -> B
const E: &str = "e0000000-0000-4000-8000-000000000001"; // base, cognate -> D (distractor)
const A: &str = "a0000000-0000-4000-8000-000000000001"; // override subject (both layers)
const F: &str = "f0000000-0000-4000-8000-000000000001"; // OVERLAY, cognate -> B (cross-layer)

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()),
    );
    dir.join("graphs").is_dir().then_some(dir)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-rev-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Add a `resource-instance` link node in its own card on the Talk model.
fn add_link_node(g: &mut serde_json::Value, ng: &str, alias: &str, datatype: &str, card: &str) {
    let g = &mut g["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": ng, "nodegroup_id": ng, "name": alias, "alias": alias,
        "datatype": datatype, "graph_id": TALK_GRAPH, "istopnode": false,
        "is_collector": true, "isrequired": false, "issearchable": true,
        "exportable": false, "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": ng, "cardinality": card, "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": format!("{ng}-edge"),
        "domainnode_id": TALK_ROOT, "rangenode_id": ng, "graph_id": TALK_GRAPH,
    }));
}

fn extend(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    add_link_node(&mut doc, COGNATE_NG, "cognate", "resource-instance-list", "n");
    add_link_node(&mut doc, COGNATE1_NG, "cognate1", "resource-instance", "1");
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

/// A `resource-instance` tile linking `resource` to `target` via node `ng`.
fn link_tile(resource: &str, tileid: &str, ng: &str, target: &str) -> serde_json::Value {
    json!({
        "tileid": tileid, "nodegroup_id": ng, "parenttile_id": null,
        "resourceinstance_id": resource, "sortorder": 0, "provisionaledits": null,
        "data": { ng: [{ "resourceId": target, "ontologyProperty": "", "inverseOntologyProperty": "" }] },
    })
}

fn resource(id: &str, tiles: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "resourceinstance": {
            "resourceinstanceid": id, "graph_id": TALK_GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } },
        },
        "tiles": tiles,
    })
}

fn corpus(tag: &str, demo: &Path, talks: Vec<serde_json::Value>) -> PathBuf {
    let dir = scratch(tag);
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    extend(&dir.join("graphs").join(format!("{TALK_GRAPH}.json")));
    let talk_dir = dir.join("resources").join("talk");
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
    ros_madair_emit::emit(data.to_str().unwrap(), out.to_str().unwrap(), "https://example.org/")
        .expect("emit");
    out
}

/// (base head, overlay head, graph). Base holds B, D, and citers C (cognate→B),
/// E (cognate→D), A (cognate1→B). Overlay holds F (cognate→B — cross-layer) and
/// restates A's cognate1 card to point at D instead.
fn setup() -> Option<(PathBuf, PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let base = corpus(
        "base",
        &demo,
        vec![
            resource(B, vec![]),
            resource(D, vec![]),
            resource(C, vec![link_tile(C, "c0000000-0000-4000-8000-0000000000c1", COGNATE_NG, B)]),
            resource(E, vec![link_tile(E, "e0000000-0000-4000-8000-0000000000e1", COGNATE_NG, D)]),
            resource(A, vec![link_tile(A, "a0000000-0000-4000-8000-0000000000a1", COGNATE1_NG, B)]),
        ],
    );
    let overlay = corpus(
        "overlay",
        &demo,
        vec![
            resource(F, vec![link_tile(F, "f0000000-0000-4000-8000-0000000000f1", COGNATE_NG, B)]),
            // Overlay RESTATES A's cognate1 card, retargeting it from B to D.
            resource(A, vec![link_tile(A, "a0000000-0000-4000-8000-0000000000a2", COGNATE1_NG, D)]),
        ],
    );

    let base_head = emit("base", &base);
    let overlay_head = emit("overlay", &overlay);

    let gp = base.join("graphs").join(format!("{TALK_GRAPH}.json"));
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let mut graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    graph.build_indices();
    Some((base_head, overlay_head, graph))
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// THE regression: `cited_by(B, cognate)` finds the OVERLAY citer F as well as
/// the base citer C — a cross-layer reverse link — and excludes the distractor E
/// (which cites D, a coarse false positive the exact verify drops).
#[test]
fn cited_by_unions_citers_across_layers() {
    let Some((base, overlay, graph)) = setup() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[base.as_path(), overlay.as_path()]).unwrap();

    assert_eq!(
        sorted(layers.cited_by(B, "cognate", &graph, None).unwrap()),
        vec![C.to_string(), F.to_string()],
        "B is cited by C (base) AND F (overlay) — the cross-layer traversal; E cites D, not B"
    );
    // The other target's citer is E only.
    assert_eq!(
        layers.cited_by(D, "cognate", &graph, None).unwrap(),
        vec![E.to_string()],
    );
    // Nobody cites an unlinked UUID.
    assert!(layers
        .cited_by(TALK_ROOT, "cognate", &graph, None)
        .unwrap()
        .is_empty());
}

/// A single-layer reverse lookup (base only) still works — F is gone, so B is
/// cited by C alone. Guards against the traversal only functioning with an overlay.
#[test]
fn cited_by_works_on_a_single_layer() {
    let Some((base, _overlay, graph)) = setup() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[base.as_path()]).unwrap();
    assert_eq!(
        layers.cited_by(B, "cognate", &graph, None).unwrap(),
        vec![C.to_string()],
    );
}

/// cardinality-1 override, in reverse: the overlay restated A's `cognate1` card
/// to point at D, so A no longer cites B (the base's old link is shadowed) and
/// now cites D. Query-and-hydration agreement, run backwards.
#[test]
fn cited_by_honours_cardinality_one_override() {
    let Some((base, overlay, graph)) = setup() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[base.as_path(), overlay.as_path()]).unwrap();

    // A's cognate1 was retargeted B -> D by the overlay, which OWNS the card.
    assert!(
        layers.cited_by(B, "cognate1", &graph, None).unwrap().is_empty(),
        "the overlay owns A's cognate1 card and points it at D, so A no longer cites B"
    );
    assert_eq!(
        layers.cited_by(D, "cognate1", &graph, None).unwrap(),
        vec![A.to_string()],
        "A now cites D via the overlay's restated card"
    );
}

/// P15: the chunk cache serves repeat reads without re-fetching. Hydrating the
/// same resource twice fetches its chunks once; the second hydrate is all hits.
#[test]
fn chunk_cache_serves_repeat_reads() {
    let Some((base, overlay, graph)) = setup() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[base.as_path(), overlay.as_path()]).unwrap();
    assert!(layers.chunk_cache().is_empty(), "cache starts cold");

    layers.hydrate_resource(C, &graph, &["en"]).unwrap();
    let cold_misses = layers.chunk_cache().misses();
    let cold_hits = layers.chunk_cache().hits();
    assert!(cold_misses > 0, "the first hydrate fetched chunks");

    layers.hydrate_resource(C, &graph, &["en"]).unwrap();
    assert_eq!(
        layers.chunk_cache().misses(),
        cold_misses,
        "a repeat hydrate must not re-fetch any chunk"
    );
    assert!(
        layers.chunk_cache().hits() > cold_hits,
        "the repeat hydrate was served from cache"
    );
}

/// P12: `cited_by` reads NO chunks — it is an indexed `reverse_links` lookup, so
/// the reverse traversal costs zero chunk fetches (the coarse scan it replaced
/// read 71–100% of a model's link chunks). The chunk cache is untouched by it;
/// only the subsequent hydrate reads chunks.
#[test]
fn cited_by_reads_no_chunks() {
    let Some((base, overlay, graph)) = setup() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[base.as_path(), overlay.as_path()]).unwrap();

    let citers = layers.cited_by(B, "cognate", &graph, None).unwrap();
    assert!(citers.contains(&C.to_string()));
    assert_eq!(
        layers.chunk_cache().misses(),
        0,
        "cited_by is an index lookup — it fetches no chunks"
    );
    assert!(layers.chunk_cache().is_empty(), "no chunk entered the cache");

    // Only the hydrate touches chunks.
    layers.hydrate_resource(C, &graph, &["en"]).unwrap();
    assert!(
        layers.chunk_cache().misses() > 0,
        "the hydrate (not the scan) is what reads chunks"
    );
}

/// `cited_by` on a NON-link node is a typed error (the same discipline a forward
/// `HasLink` on a concept node gets), not a silent empty answer.
#[test]
fn cited_by_on_a_non_link_node_is_a_typed_error() {
    let Some((base, overlay, graph)) = setup() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[base.as_path(), overlay.as_path()]).unwrap();
    // `topics` is a concept node on the demo Talk model, not a link.
    assert!(matches!(
        layers.cited_by(B, "topics", &graph, None),
        Err(ReadError::Query(_))
    ));
}
