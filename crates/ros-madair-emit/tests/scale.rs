// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Scale gate: does ros-madair-emit handle the full Aonach Mór footprint set?**
//!
//! `cargo test -p ros-madair-emit --test scale -- --ignored --nocapture`
//!
//! The demo's big corpus currently uses a bespoke JS generator (genFull) that
//! hand-writes Parquet, bypassing emit. Before translating that logic onto the
//! real engine (so the demo exercises the actual melt substrate + layering), this
//! proves emit ingests ~33k resources — one per real OSM footprint — carrying
//! geometry (geo zone-map), a concept (melted concept store), and a number (melted
//! ordered store, via the handler added this session), and produces a multi-
//! row-group substrate. Reports wall-time and the artifact shape.

use std::collections::HashMap;
use std::path::PathBuf;

use duckdb::Connection;
use ros_madair_emit::ClusterConfig;
use ros_madair_handlers::default_registry;
use serde_json::json;

const GRAPH: &str = "aca11111-0000-4000-8000-000000000001";
const ROOT: &str = "aca11111-1ae0-11f0-b22a-8fd6f4eb1a02";
const NG: &str = "b0000000-0000-4000-8000-00000000000a";
const GEO: &str = "b0000000-0000-4000-8000-00000000000b";
const CON: &str = "b0000000-0000-4000-8000-00000000000c";
const NUM: &str = "b0000000-0000-4000-8000-00000000000d";

const N: usize = 33_000; // the real footprint count

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-scale-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node(id: &str, ng: &str, alias: &str, dt: &str, collector: bool) -> serde_json::Value {
    json!({"nodeid": id, "nodegroup_id": ng, "name": alias, "alias": alias, "datatype": dt,
           "graph_id": GRAPH, "istopnode": false, "is_collector": collector, "isrequired": false,
           "issearchable": true, "exportable": false, "sortorder": 0})
}
fn edge(id: &str, f: &str, t: &str) -> serde_json::Value {
    json!({"edgeid": id, "domainnode_id": f, "rangenode_id": t, "graph_id": GRAPH})
}

#[test]
#[ignore = "scale gate; run explicitly with --ignored"]
fn emit_scales_to_the_full_footprint_set() {
    let dir = scratch("in");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({"nodeid": ROOT, "name": "Placed", "alias": "placed", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    std::fs::write(&gp, serde_json::to_vec(&json!({"graph": [{
        "graphid": GRAPH, "name": "Placed", "root": root.clone(),
        "nodes": [root,
            node(NG, NG, "profile", "semantic", true),
            node(GEO, NG, "footprint", "geojson-feature-collection", false),
            node(CON, NG, "gem", "concept", false),
            node(NUM, NG, "height_m", "number", false)],
        "nodegroups": [{"nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [edge("e1", ROOT, NG), edge("e2", NG, GEO), edge("e3", NG, CON), edge("e4", NG, NUM)],
    }]})).unwrap()).unwrap();

    // ~33k resources across the Aonach bbox, each a footprint point + a GEM concept
    // + a height. Deterministic LCG (no RNG dep); coords spread to exercise the geo
    // zone-map, concepts to exercise the melted concept store.
    let (x0, y0, x1, y1) = (80.18_f64, 12.89_f64, 80.36_f64, 13.25_f64);
    const GEMS: [&str; 8] = ["CR/LFM", "CR/LFINF", "MUR+CLBRS", "S/LFM", "W/LWAL", "E/LWAL", "MUR+STDRE", "MIX"];
    let rdir = dir.join("resources").join("placed");
    std::fs::create_dir_all(&rdir).unwrap();
    let mut a: u64 = 0x9E3779B9;
    let mut rnd = || {
        a = a.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (a >> 33) as f64 / (1u64 << 31) as f64
    };
    let mut resources = Vec::with_capacity(N);
    for i in 0..N {
        let (lng, lat) = (x0 + rnd() * (x1 - x0), y0 + rnd() * (y1 - y0));
        let r = format!("r{i:07}-0000-4000-8000-000000000001");
        let gem = GEMS[i % GEMS.len()];
        let height = 4 + (i % 36);
        let fc = json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {},
             "geometry": {"type": "Point", "coordinates": [lng, lat]}}]});
        resources.push(json!({
            "resourceinstance": {"resourceinstanceid": r, "graph_id": GRAPH, "name": r, "legacyid": null,
                "descriptors": {"en": {"name": r, "description": "", "map_popup": ""}}},
            "tiles": [{"tileid": format!("t{i:07}-0000-4000-8000-000000000001"), "nodegroup_id": NG,
                "parenttile_id": null, "resourceinstance_id": r, "sortorder": 0, "provisionaledits": null,
                "data": {GEO: fc, CON: gem, NUM: height}}],
        }));
    }
    std::fs::write(rdir.join("placed.json"),
        serde_json::to_vec(&json!({"business_data": {"resources": resources}})).unwrap()).unwrap();

    let pq = scratch("out");
    let mut cfg = HashMap::new();
    cfg.insert(GRAPH.to_string(), ClusterConfig { row_group_size: 8192, ..Default::default() });
    let t = std::time::Instant::now();
    ros_madair_emit::emit_parquet(
        dir.to_str().unwrap(), pq.to_str().unwrap(), "https://example.org/",
        &default_registry(), &cfg,
    ).expect("emit at scale");
    let secs = t.elapsed().as_secs_f64();

    let conn = Connection::open_in_memory().unwrap();
    let count = |f: &str| -> i64 {
        let p = pq.join(format!("{f}_{GRAPH}.parquet"));
        if !p.exists() { return -1; }
        conn.query_row(&format!("SELECT count(*) FROM read_parquet('{}')", p.display()), [], |r| r.get(0)).unwrap()
    };
    let rgs = |f: &str| -> i64 {
        conn.query_row(&format!("SELECT sum(num_row_groups) FROM parquet_file_metadata('{}')",
            pq.join(format!("{f}_{GRAPH}.parquet")).display()), [], |r| r.get(0)).unwrap_or(-1)
    };
    let tiles = count("tiles");
    let concepts = count("concepts");
    let ordered = count("ordered");
    let geo_pop: i64 = conn.query_row(&format!(
        "SELECT count(*) FROM read_parquet('{}') WHERE geo_min_lng IS NOT NULL",
        pq.join(format!("tiles_{GRAPH}.parquet")).display()), [], |r| r.get(0)).unwrap();

    eprintln!("=== emit scale gate ===");
    eprintln!("resources in: {N}   emit wall-time: {secs:.1}s");
    eprintln!("tiles={tiles} (row groups {})   concepts={concepts}   ordered={ordered}   geo-populated tiles={geo_pop}",
        rgs("tiles"));
    eprintln!("concepts row groups={}   ordered row groups={}", rgs("concepts"), rgs("ordered"));

    assert_eq!(tiles, N as i64, "every resource emitted a tile");
    assert!(rgs("tiles") > 1, "multiple row groups (prunable)");
    assert_eq!(concepts, N as i64, "every GEM concept melted into the concept store");
    assert_eq!(ordered, N as i64, "every height melted into the ordered store");
    assert_eq!(geo_pop, N as i64, "every footprint promoted a geo bbox (the spatial zone-map)");
    eprintln!("OK: emit scales to the full footprint set, geo+concept+ordered all melted");
}
