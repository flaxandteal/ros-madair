// SPDX-License-Identifier: AGPL-3.0-or-later
//! **A8.1: date fields are head-indexed as an ordered scalar; range queries
//! work end to end.**
//!
//! There is no real temporal corpus yet (only `RosMadair-ResilienceDemo-Plan.md`),
//! so this is the validation: a synthetic Talk model with a `founded` DATE node,
//! two talks with known dates, emitted through the real emitter and queried back.
//! It exercises the whole vertical — `IndexClass::Ordered`, the emitter's
//! days-from-civil quantization into `value_tags`, and the compiler's
//! `qvalue BETWEEN lo AND hi`.
//!
//! Demo fixture lives outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it,
//! absent → skip.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use alizarin_core::quantize::quantize_date;
use ros_madair_query::{Expr, Measure, Query};
use ros_madair_read::Layers;
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const FOUNDED_NG: &str = "5efd0000-0000-4000-8000-000000000001";

const TALK_A: &str = "179b8583-3140-437e-bd0b-34d5aa2f1550";
const TALK_B: &str = "97cc9a1b-ee42-412e-9fa5-203d98bff815";
const DATE_A: &str = "2005-06-01";
const DATE_B: &str = "2018-03-15";

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
    let dir = std::env::temp_dir().join(format!("rm-ord-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `date` node in its own cardinality-1 card on the Talk model.
fn add_date_node(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": FOUNDED_NG,
        "nodegroup_id": FOUNDED_NG,
        "name": "Founded",
        "alias": "founded",
        "datatype": "date",
        "graph_id": TALK_GRAPH,
        "istopnode": false,
        "is_collector": true,
        "isrequired": false,
        "issearchable": true,
        "exportable": false,
        "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": FOUNDED_NG, "cardinality": "1", "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": "5efd0000-0000-4000-8000-0000000000ee",
        "domainnode_id": TALK_ROOT, "rangenode_id": FOUNDED_NG, "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn talk(id: &str, tileid: &str, founded: &str) -> serde_json::Value {
    json!({
        "resourceinstance": {
            "resourceinstanceid": id, "graph_id": TALK_GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } },
        },
        "tiles": [{
            "tileid": tileid, "nodegroup_id": FOUNDED_NG, "parenttile_id": null,
            "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
            "data": { FOUNDED_NG: founded },
        }],
    })
}

fn corpus() -> Option<(PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let dir = scratch("corpus");
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    let gp = dir.join("graphs").join(format!("{TALK_GRAPH}.json"));
    add_date_node(&gp);
    let talk_dir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&talk_dir).unwrap();
    std::fs::write(
        talk_dir.join("talks.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": [
            talk(TALK_A, "aaaa0000-0000-4000-8000-000000000001", DATE_A),
            talk(TALK_B, "bbbb0000-0000-4000-8000-000000000001", DATE_B),
        ] } }))
        .unwrap(),
    )
    .unwrap();

    let out = scratch("head");
    ros_madair_emit::emit(
        dir.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
    )
    .expect("emit");

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    Some((out, graph))
}

fn range(lo: &str, hi: &str) -> Query {
    Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(Expr::Range {
            path: "founded".to_string(),
            lo: quantize_date(lo).unwrap(),
            hi: quantize_date(hi).unwrap(),
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

/// The date reached the head as a quantized `value_tags` row — the exact
/// days-from-civil key, not a placeholder or a dropped detail-only value.
#[test]
fn dates_are_quantized_into_value_tags() {
    let Some((out, _graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let c = rusqlite::Connection::open(out.join("head.sqlite")).unwrap();

    // Every founded date is present as its days-from-civil key, keyed to the
    // right resource via the spine.
    let got: i64 = c
        .query_row(
            "SELECT vt.qvalue FROM value_tags vt
               JOIN spine_talk s ON s.rid = vt.rid
               JOIN dict d ON d.term_id = s.term_id
              WHERE d.term = ?1",
            [TALK_A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(got, quantize_date(DATE_A).unwrap(), "Talk A's founded date");
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM value_tags", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2,
        "both talks indexed their date"
    );
}

/// The range query returns exactly the resources whose date falls in [lo, hi].
#[test]
fn a_date_range_query_selects_the_right_resources() {
    let Some((out, graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[out.as_path()]).unwrap();
    let ids = |q: &Query| {
        let mut v = layers.resolve(q, &graph, None).unwrap();
        v.sort();
        v
    };

    // A window around 2005 catches Talk A only.
    assert_eq!(
        ids(&range("2000-01-01", "2010-12-31")),
        vec![TALK_A.to_string()],
        "only the 2005 talk is in the 2000s window"
    );
    // A window around 2018 catches Talk B only.
    assert_eq!(
        ids(&range("2015-01-01", "2020-01-01")),
        vec![TALK_B.to_string()],
    );
    // A window spanning both catches both.
    let mut both = vec![TALK_A.to_string(), TALK_B.to_string()];
    both.sort();
    assert_eq!(ids(&range("1990-01-01", "2030-01-01")), both);
    // A window before either catches none.
    assert!(ids(&range("1800-01-01", "1900-01-01")).is_empty());
    // Inclusive boundary: [DATE_A, DATE_A] catches Talk A (BETWEEN is inclusive).
    assert_eq!(ids(&range(DATE_A, DATE_A)), vec![TALK_A.to_string()]);
}

/// A range on a NON-ordered field is a typed error, not a silent empty result —
/// the same discipline concept/link mismatches already get.
#[test]
fn a_range_on_a_non_ordered_field_is_a_typed_error() {
    let Some((out, graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[out.as_path()]).unwrap();
    // `title` is a string (detail-only), not ordered.
    let q = Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(Expr::Range {
            path: "title".to_string(),
            lo: 0,
            hi: 100,
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    };
    assert!(
        layers.resolve(&q, &graph, None).is_err(),
        "a range on a non-ordered field must be rejected, not answer empty"
    );
}
