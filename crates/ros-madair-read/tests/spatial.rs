// SPDX-License-Identifier: AGPL-3.0-or-later
//! **A8.2: geometry is head-indexed as a bounding box; bbox-OVERLAP queries work
//! end to end — and return what a centroid index would wrongly drop.**
//!
//! The whole point of indexing a bbox rather than a centroid: a polygon that
//! genuinely intersects the query box but whose centroid sits OUTSIDE it must
//! still be returned. `polygon_whose_centroid_is_outside_is_still_returned` is
//! that proof — it is the test that would fail under the centroid design the
//! resilience-demo plan started from.
//!
//! bbox-overlap is a strict SUPERSET of true intersection, so the query is
//! `coarse`: it can admit a false positive (a geometry whose box overlaps the
//! query box but whose actual shape does not), which the client verifies away on
//! hydrated tiles. `bbox_overlap_is_a_coarse_superset` pins that behaviour.
//!
//! Same synthetic-corpus harness as `ordered.rs`: a Talk model with an added
//! geometry node, emitted through the real emitter and queried back. Demo fixture
//! lives outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it, absent → skip.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_query::{Expr, Measure, Query};
use ros_madair_read::Layers;
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const GEO_NG: &str = "5efd0000-0000-4000-8000-000000000002";

// A point at the origin — inside the query box.
const TALK_POINT: &str = "11110000-0000-4000-8000-000000000001";
// An L-shaped polygon touching the origin corner, whose CENTROID (~(4,4)) is
// well outside the query box — the anti-centroid case.
const TALK_LSHAPE: &str = "22220000-0000-4000-8000-000000000001";
// A point far away — a true negative.
const TALK_FAR: &str = "33330000-0000-4000-8000-000000000001";
// A diagonal line whose BBOX overlaps the query box but whose SHAPE does not —
// a bbox false positive (the coarse query returns it; the client drops it).
const TALK_DIAGONAL: &str = "44440000-0000-4000-8000-000000000001";

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
    let dir = std::env::temp_dir().join(format!("rm-geo-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `geojson-feature-collection` node in its own cardinality-1 card on Talk.
fn add_geo_node(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": GEO_NG,
        "nodegroup_id": GEO_NG,
        "name": "Location",
        "alias": "location",
        "datatype": "geojson-feature-collection",
        "graph_id": TALK_GRAPH,
        "istopnode": false,
        "is_collector": true,
        "isrequired": false,
        "issearchable": true,
        "exportable": false,
        "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": GEO_NG, "cardinality": "1", "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": "5efd0000-0000-4000-8000-0000000000ef",
        "domainnode_id": TALK_ROOT, "rangenode_id": GEO_NG, "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

/// A Talk carrying one geometry (a GeoJSON geometry object, wrapped in a
/// FeatureCollection — the shape `geojson-feature-collection` tile values take).
fn talk(id: &str, tileid: &str, geometry: serde_json::Value) -> serde_json::Value {
    let fc = json!({
        "type": "FeatureCollection",
        "features": [{ "type": "Feature", "properties": {}, "geometry": geometry }],
    });
    json!({
        "resourceinstance": {
            "resourceinstanceid": id, "graph_id": TALK_GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } },
        },
        "tiles": [{
            "tileid": tileid, "nodegroup_id": GEO_NG, "parenttile_id": null,
            "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
            "data": { GEO_NG: fc },
        }],
    })
}

fn corpus() -> Option<(PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let dir = scratch("corpus");
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    let gp = dir.join("graphs").join(format!("{TALK_GRAPH}.json"));
    add_geo_node(&gp);
    let talk_dir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&talk_dir).unwrap();
    std::fs::write(
        talk_dir.join("talks.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": [
            talk(
                TALK_POINT,
                "aaaa0000-0000-4000-8000-000000000002",
                json!({ "type": "Point", "coordinates": [0.0, 0.0] }),
            ),
            // Ring hugging the origin corner then extending far to the NE; its
            // centroid is ~(4,4) — OUTSIDE the [-1,1]² query box — but the corner
            // vertex (0.5, 0.5) is INSIDE it, so it genuinely intersects.
            talk(
                TALK_LSHAPE,
                "bbbb0000-0000-4000-8000-000000000002",
                json!({ "type": "Polygon", "coordinates": [[
                    [0.5, 0.5], [10.0, 0.5], [10.0, 2.0],
                    [2.0, 2.0], [2.0, 10.0], [0.5, 10.0], [0.5, 0.5]
                ]] }),
            ),
            talk(
                TALK_FAR,
                "cccc0000-0000-4000-8000-000000000002",
                json!({ "type": "Point", "coordinates": [100.0, 100.0] }),
            ),
            // Line from (0.5,5) to (5,0.5): bbox [0.5,0.5,5,5] overlaps the query
            // box, but the segment stays at y>=4.5 for x in [0.5,1], so it never
            // enters [-1,1]² — a bbox false positive.
            talk(
                TALK_DIAGONAL,
                "dddd0000-0000-4000-8000-000000000002",
                json!({ "type": "LineString", "coordinates": [[0.5, 5.0], [5.0, 0.5]] }),
            ),
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

/// The unit query box `[-1,-1] .. [1,1]` around the origin.
fn around_origin() -> Query {
    Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(Expr::Bbox {
            path: "location".to_string(),
            min_lng: -1.0,
            min_lat: -1.0,
            max_lng: 1.0,
            max_lat: 1.0,
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    }
}

/// Each geometry's bbox reached the head as a `geo_bbox` row — the L-shape's box
/// is its full extent `[0.5,0.5]..[10,10]`, NOT its centroid.
#[test]
fn geometries_are_indexed_as_their_bounding_box() {
    let Some((out, _graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let c = rusqlite::Connection::open(out.join("head.sqlite")).unwrap();

    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM geo_bbox", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4,
        "every talk indexed its geometry's bbox"
    );
    // The L-shape stored its full extent, not a reduced centroid.
    let (min_lng, min_lat, max_lng, max_lat): (f64, f64, f64, f64) = c
        .query_row(
            "SELECT g.min_lng, g.min_lat, g.max_lng, g.max_lat FROM geo_bbox g
               JOIN spine_talk s ON s.rid = g.rid
               JOIN dict d ON d.term_id = s.term_id
              WHERE d.term = ?1",
            [TALK_LSHAPE],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((min_lng, min_lat, max_lng, max_lat), (0.5, 0.5, 10.0, 10.0));
}

/// THE test: a polygon that intersects the query box but whose centroid lies
/// outside it is returned. A centroid index would drop it; bbox-overlap does not.
#[test]
fn polygon_whose_centroid_is_outside_is_still_returned() {
    let Some((out, graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let layers = Layers::open(&[out.as_path()]).unwrap();
    let mut ids = layers.resolve(&around_origin(), &graph, None).unwrap();
    ids.sort();

    assert!(
        ids.contains(&TALK_LSHAPE.to_string()),
        "the L-shape truly intersects the box; its centroid is outside, but a \
         bbox index must still return it — got {ids:?}"
    );
    assert!(
        ids.contains(&TALK_POINT.to_string()),
        "the origin point is inside the box"
    );
    assert!(
        !ids.contains(&TALK_FAR.to_string()),
        "the far point's bbox does not overlap the box"
    );
}

/// bbox-overlap is a coarse SUPERSET: the diagonal line's box overlaps the query
/// box though the line itself does not, so the coarse query returns it (the
/// client filters it exactly on hydration). The compiled statement says so.
#[test]
fn bbox_overlap_is_a_coarse_superset() {
    let Some((out, graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };

    // The compiler marks a bbox filter coarse.
    let stmt = &ros_madair_query::compile(&around_origin(), &graph).unwrap()[0];
    assert!(
        stmt.coarse,
        "bbox-overlap over-approximates intersection, so it must be coarse"
    );

    // And the over-approximation shows up in results: the diagonal is admitted.
    let layers = Layers::open(&[out.as_path()]).unwrap();
    let ids = layers.resolve(&around_origin(), &graph, None).unwrap();
    assert!(
        ids.contains(&TALK_DIAGONAL.to_string()),
        "the diagonal's bbox overlaps the box, so the coarse query admits it \
         (a false positive the client verifies away) — got {ids:?}"
    );
}

/// A bbox filter on a NON-spatial field is a typed error, not a silent empty
/// result — the same discipline concept/link/range mismatches get.
#[test]
fn a_bbox_on_a_non_spatial_field_is_a_typed_error() {
    let Some((_out, graph)) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let q = Query {
        model: TALK_GRAPH.to_string(),
        r#where: Some(Expr::Bbox {
            path: "title".to_string(), // a string node, not geometry
            min_lng: -1.0,
            min_lat: -1.0,
            max_lng: 1.0,
            max_lat: 1.0,
        }),
        measures: vec![Measure::SelectIds],
        limit: None,
    };
    assert!(
        ros_madair_query::compile(&q, &graph).is_err(),
        "a bbox on a non-spatial field must be rejected, not answer empty"
    );
}
