// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Bandwidth-constrained performance: a Range reader fetches KB, not the file.**
//!
//! This is the functional backing for the demo deck's cost/axis-1 claim — *"a static
//! fileset representing a whole city answers graph/spatial/hierarchy queries while pulling
//! kilobytes–megabytes over the wire, not gigabytes."* The mechanism is the only prune the
//! substrate has: every store is **sorted** so its predicate column's row-group min/max
//! (the Parquet zone-map) lets a reader skip row groups, and Parquet is **columnar** so it
//! skips unread columns' chunks too. Over HTTP that is exactly what an httpfs / duckdb-wasm
//! Range reader fetches: the footer, plus the compressed column-chunks of the row groups the
//! zone-map does **not** prune, for the **columns the query projects**.
//!
//! Each case below pairs:
//!   1. a **real `DuckReader` query** (or the compiler's lowered predicate) — proving the
//!      answer is correct, so the small read isn't small because it's wrong; and
//!   2. a **byte accounting from `parquet_metadata`** — the sum of compressed sizes of the
//!      (surviving row-group × projected-column) chunks a Range reader would pull, asserted
//!      to be a small, bounded fraction of the whole fileset, with the figure printed.
//!
//! NB this measures *bytes a Range reader fetches*, computed from the files' own zone-map
//! stats — the standard rule every Parquet Range reader (DuckDB httpfs, duckdb-wasm)
//! applies. It does not open a socket: native `duck` reads local files and `httpfs` is not
//! bundled offline. The end-to-end socket measurement (the 472 MB→7.6 MB browser figure)
//! lives in the duckdb-wasm demo; this is its deterministic, offline, CI-able analogue.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use duckdb::Connection;
use ros_madair_duck::DuckReader;
use ros_madair_handlers::{default_registry, number_scale, quantize_number};
use ros_madair_query::{ConceptOp, Expr, Measure, Query};
use serde_json::json;

const GRAPH: &str = "ba11d111-0000-4000-8000-000000000001";
const ROOT: &str = "ba11d111-0000-4000-8000-000000000002";
const NG: &str = "c0000000-0000-4000-8000-0000000000b1";
const GEO: &str = "c0000000-0000-4000-8000-0000000000b2";
const ROLE: &str = "c0000000-0000-4000-8000-0000000000b3";
const NUM: &str = "c0000000-0000-4000-8000-0000000000b4";

// DuckDB's Parquet writer floors ROW_GROUP_SIZE at its 2048-row vector size, so a smaller
// value is silently clamped up. To make the zone-map prune *observable* the corpus must span
// many 2048-row groups — hence N at several ×2048 (≈8 row groups per store).
const N: usize = 16384; // = 8 × 2048 → ~8 row groups per store
const RGS: usize = 2048; // the effective floor; smaller is clamped to this
                         // Roles assigned round-robin; "Shelter" is 1/8 of the corpus.
const ROLES: [&str; 8] = [
    "Hospital", "Shelter", "School", "Clinic", "Depot", "Office", "Market", "Temple",
];

// Chennai-ish bbox the coords spread across (same space the demo uses).
const X0: f64 = 80.18;
const Y0: f64 = 12.89;
const X1: f64 = 80.36;
const Y1: f64 = 13.25;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rm-bw-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node(id: &str, alias: &str, dt: &str, collector: bool) -> serde_json::Value {
    json!({"nodeid": id, "nodegroup_id": NG, "name": alias, "alias": alias, "datatype": dt,
           "graph_id": GRAPH, "istopnode": false, "is_collector": collector, "isrequired": false,
           "issearchable": true, "exportable": false, "sortorder": 0})
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({"nodeid": ROOT, "name": "Place", "alias": "place", "datatype": "semantic",
                      "graph_id": GRAPH, "istopnode": true});
    let doc = json!({"graph": [{
        "graphid": GRAPH, "name": "Place", "root": root.clone(),
        "nodes": [root,
            node(GEO, "footprint", "geojson-feature-collection", true),
            node(ROLE, "service_role", "concept", false),
            node(NUM, "height_m", "number", false)],
        "nodegroups": [{"nodegroupid": NG, "cardinality": "1", "parentnodegroup_id": null}],
        "edges": [
            {"edgeid": "e1", "domainnode_id": ROOT, "rangenode_id": NG, "graph_id": GRAPH},
            {"edgeid": "e2", "domainnode_id": NG, "rangenode_id": GEO, "graph_id": GRAPH},
            {"edgeid": "e3", "domainnode_id": NG, "rangenode_id": ROLE, "graph_id": GRAPH},
            {"edgeid": "e4", "domainnode_id": NG, "rangenode_id": NUM, "graph_id": GRAPH}],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

/// Build the fileset ONCE. Returns (tiles path, dir, graph) and the per-resource facts
/// a brute-force correctness check needs (coords, role, height).
struct Fact {
    id: String,
    lng: f64,
    lat: f64,
    role: &'static str,
    height: i64,
}

fn corpus() -> (PathBuf, Vec<Fact>, StaticGraph) {
    let dir = scratch("in");
    let gp = dir.join("graphs").join(format!("{GRAPH}.json"));
    write_graph(&gp);
    let rdir = dir.join("resources").join("place");
    std::fs::create_dir_all(&rdir).unwrap();

    // Deterministic LCG — no RNG dep, reproducible row groups.
    let mut a: u64 = 0x9E3779B9;
    let mut rnd = || {
        a = a
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (a >> 33) as f64 / (1u64 << 31) as f64
    };

    let mut facts = Vec::with_capacity(N);
    let mut resources = Vec::with_capacity(N);
    for i in 0..N {
        let (lng, lat) = (X0 + rnd() * (X1 - X0), Y0 + rnd() * (Y1 - Y0));
        let id = format!("r{i:07}-0000-4000-8000-000000000001");
        let role = ROLES[i % ROLES.len()];
        let height = 4 + (i % 36) as i64;
        let fc = json!({"type": "FeatureCollection", "features": [
            {"type": "Feature", "properties": {}, "geometry": {"type": "Point", "coordinates": [lng, lat]}}]});
        resources.push(json!({
            "resourceinstance": {"resourceinstanceid": id, "graph_id": GRAPH, "name": id, "legacyid": null,
                "descriptors": {"en": {"name": id, "description": "", "map_popup": ""}}},
            "tiles": [{"tileid": format!("t{i:07}-0000-4000-8000-000000000001"), "nodegroup_id": NG,
                "parenttile_id": null, "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
                "data": {GEO: fc, ROLE: role, NUM: height}}],
        }));
        facts.push(Fact {
            id,
            lng,
            lat,
            role,
            height,
        });
    }
    std::fs::write(
        rdir.join("place.json"),
        serde_json::to_vec(&json!({"business_data": {"resources": resources}})).unwrap(),
    )
    .unwrap();

    let pq = scratch("out");
    let mut cfg = HashMap::new();
    cfg.insert(
        GRAPH.to_string(),
        ros_madair_emit::ClusterConfig {
            row_group_size: RGS,
            ..Default::default()
        },
    );
    ros_madair_emit::emit_parquet(
        dir.to_str().unwrap(),
        pq.to_str().unwrap(),
        "https://example.org/",
        &default_registry(),
        &cfg,
    )
    .expect("emit");

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    (pq.join(format!("tiles_{GRAPH}.parquet")), facts, graph)
}

// ---- metadata helpers (what a Range reader fetches, from the files' own zone-map) ----

fn i64_row(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Σ compressed bytes across every chunk of the file — the whole-download baseline.
fn total_compressed(conn: &Connection, path: &Path) -> i64 {
    i64_row(
        conn,
        &format!(
            "SELECT coalesce(sum(total_compressed_size),0) FROM parquet_metadata('{}')",
            path.display()
        ),
    )
}

fn total_row_groups(conn: &Connection, path: &Path) -> i64 {
    i64_row(
        conn,
        &format!(
            "SELECT coalesce(sum(num_row_groups),0) FROM parquet_file_metadata('{}')",
            path.display()
        ),
    )
}

/// Σ compressed bytes a reader fetches for `cols` across the row groups in `hit_sql`
/// (a subquery yielding `row_group_id`).
fn fetched_bytes(conn: &Connection, path: &Path, cols: &[&str], hit_sql: &str) -> i64 {
    let list = cols
        .iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(",");
    i64_row(
        conn,
        &format!(
            "SELECT coalesce(sum(total_compressed_size),0) FROM parquet_metadata('{}') \
             WHERE row_group_id IN ({hit_sql}) AND path_in_schema IN ({list})",
            path.display()
        ),
    )
}

fn count_groups(conn: &Connection, hit_sql: &str) -> i64 {
    i64_row(conn, &format!("SELECT count(*) FROM ({hit_sql})"))
}

fn pct(part: i64, whole: i64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

#[test]
#[ignore = "bandwidth/scale gate (emits a multi-row-group corpus; ~minutes). Run with: \
            cargo test -p ros-madair-duck --test bandwidth -- --ignored --nocapture"]
fn range_reader_fetches_kilobytes_not_the_fileset() {
    let (tiles, facts, graph) = corpus();
    let dir = tiles.parent().unwrap();
    let p = |name: &str| dir.join(format!("{name}_{GRAPH}.parquet"));
    let reg = default_registry();
    let meta = Connection::open_in_memory().expect("duckdb");
    let duck = DuckReader::open(tiles.to_str().unwrap()).expect("open");

    let resolve = |expr: Expr| -> Vec<String> {
        let q = Query {
            model: GRAPH.into(),
            r#where: Some(expr),
            measures: vec![Measure::SelectIds],
            limit: None,
        };
        duck.resolve_ids(&q, &graph, &reg).expect("resolve_ids")
    };

    let total_fileset: i64 = ["tiles", "concepts", "ordered", "grid", "resources", "edges"]
        .iter()
        .filter(|n| p(n).exists())
        .map(|n| total_compressed(&meta, &p(n)))
        .sum();

    eprintln!("=== bandwidth: {N} resources, row_group_size={RGS} ===");
    eprintln!("whole fileset (compressed) = {} KB\n", total_fileset / 1024);

    // Assert a case: real answer is right AND the fetched read is a small slice.
    // `whole` is the store's own compressed size; `fileset_pct` is vs the full fileset.
    let report = |label: &str, fetched: i64, groups: i64, total_groups: i64, store: &Path| {
        let whole = total_compressed(&meta, store);
        eprintln!(
            "{label:<34} fetch {:>7} KB  ({:>5.1}% of this store, {:>4.1}% of fileset)  rowgroups {groups}/{total_groups}",
            fetched / 1024,
            pct(fetched, whole),
            pct(fetched, total_fileset),
        );
        (whole, pct(fetched, whole))
    };

    // ---------------------------------------------------------------------------
    // CASE 1 — spatial viewport over the cell-sorted grid edge (the responder's map read).
    // Real reader: GridBox around a patch of the city → a strict, non-empty subset.
    // Fetched: the grid row groups whose `cell` band overlaps the near set's cells, all
    // columns (a viewport wants the render fields). The rest of the city's cells are skipped.
    // ---------------------------------------------------------------------------
    // A ~12%-per-side box near the bbox centre.
    let (cx, cy) = ((X0 + X1) / 2.0, (Y0 + Y1) / 2.0);
    let (hw, hh) = (0.06 * (X1 - X0), 0.06 * (Y1 - Y0));
    let (bxlo, bxhi, bylo, byhi) = (cx - hw, cx + hw, cy - hh, cy + hh);

    let near = resolve(Expr::GridBox {
        path: "footprint".into(),
        min_lng: bxlo,
        min_lat: bylo,
        max_lng: bxhi,
        max_lat: byhi,
    });
    let brute_in_box = facts
        .iter()
        .filter(|f| f.lng >= bxlo && f.lng <= bxhi && f.lat >= bylo && f.lat <= byhi)
        .count();
    assert!(
        !near.is_empty() && near.len() < N,
        "GridBox must return a non-empty strict subset (got {} of {N})",
        near.len()
    );
    assert!(
        near.len() >= brute_in_box,
        "GridBox is a superset of exact containment: {} >= {brute_in_box}",
        near.len()
    );
    // Cell band of the near set, read off the grid edge.
    let in_list = near
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let (clo, chi): (i64, i64) = meta
        .query_row(
            &format!(
                "SELECT min(cell), max(cell) FROM read_parquet('{}') WHERE resource_id IN ({in_list})",
                p("grid").display()
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("cell band");
    let grid_hit = format!(
        "SELECT row_group_id FROM (SELECT row_group_id, \
           max(CASE WHEN path_in_schema='cell' THEN CAST(stats_min_value AS BIGINT) END) AS clo, \
           max(CASE WHEN path_in_schema='cell' THEN CAST(stats_max_value AS BIGINT) END) AS chi \
         FROM parquet_metadata('{}') GROUP BY row_group_id) WHERE clo <= {chi} AND chi >= {clo}",
        p("grid").display()
    );
    let grid_cols = [
        "cell",
        "resource_id",
        "descriptor",
        "geo_descriptor",
        "render_summary",
        "render_fields",
    ];
    let fetched = fetched_bytes(&meta, &p("grid"), &grid_cols, &grid_hit);
    let groups = count_groups(&meta, &grid_hit);
    let tg = total_row_groups(&meta, &p("grid"));
    let (_, frac) = report("1 spatial viewport (grid)", fetched, groups, tg, &p("grid"));
    assert!(
        groups < tg,
        "viewport must prune grid row groups ({groups} < {tg})"
    );
    assert!(
        frac < 50.0,
        "viewport read must be a slice of the grid edge, was {frac:.1}%"
    );

    // ---------------------------------------------------------------------------
    // CASE 2 — the tiles geo zone-map + the columnar win (render fields avoid hydration).
    // The zone-map (geo_* row-group min/max) prunes to the box's row groups; crucially the
    // fat `data` hydration column is NOT projected, so a filter/id-resolve skips it entirely.
    // Correctness: the coarse zone-map predicate (what Bbox compiles to; needs no spatial
    // ext) counts the same candidate set a reader would scan.
    // ---------------------------------------------------------------------------
    let geo_hit = format!(
        "SELECT row_group_id FROM (SELECT row_group_id, \
           max(CASE WHEN path_in_schema='geo_min_lng' THEN CAST(stats_min_value AS DOUBLE) END) AS xlo, \
           max(CASE WHEN path_in_schema='geo_max_lng' THEN CAST(stats_max_value AS DOUBLE) END) AS xhi, \
           max(CASE WHEN path_in_schema='geo_min_lat' THEN CAST(stats_min_value AS DOUBLE) END) AS ylo, \
           max(CASE WHEN path_in_schema='geo_max_lat' THEN CAST(stats_max_value AS DOUBLE) END) AS yhi \
         FROM parquet_metadata('{}') GROUP BY row_group_id) \
         WHERE xlo <= {bxhi} AND xhi >= {bxlo} AND ylo <= {byhi} AND yhi >= {bylo}",
        p("tiles").display()
    );
    let candidates: i64 = meta
        .query_row(
            &format!(
                "SELECT count(*) FROM read_parquet('{}') \
                 WHERE geo_min_lng <= {bxhi} AND geo_max_lng >= {bxlo} \
                   AND geo_min_lat <= {byhi} AND geo_max_lat >= {bylo}",
                p("tiles").display()
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        candidates as usize >= brute_in_box && (candidates as usize) < N,
        "geo zone-map candidate set is a strict superset of containment: {brute_in_box} <= {candidates} < {N}"
    );
    let geo_cols = [
        "geo_min_lng",
        "geo_min_lat",
        "geo_max_lng",
        "geo_max_lat",
        "resource_id",
    ];
    let fetched = fetched_bytes(&meta, &p("tiles"), &geo_cols, &geo_hit);
    let groups = count_groups(&meta, &geo_hit);
    let tg = total_row_groups(&meta, &p("tiles"));
    report(
        "2 tiles geo zone-map (ids)",
        fetched,
        groups,
        tg,
        &p("tiles"),
    );
    assert!(
        groups < tg,
        "a small box must prune tile row groups ({groups} < {tg})"
    );

    // The columnar win, stated as its own number: the `data` column is the bulk of the
    // tiles file, and the spatial id-resolve above never fetches it.
    let data_bytes = i64_row(
        &meta,
        &format!(
            "SELECT coalesce(sum(total_compressed_size),0) FROM parquet_metadata('{}') WHERE path_in_schema='data'",
            p("tiles").display()
        ),
    );
    let tiles_whole = total_compressed(&meta, &p("tiles"));
    let data_share = pct(data_bytes, tiles_whole);
    eprintln!(
        "  └ `data` hydration column = {:.1}% of the tiles file — skipped entirely by a filter/id read",
        data_share
    );
    assert!(
        data_share > 40.0,
        "the fat `data` column should dominate the tiles file (was {data_share:.1}%); the columnar skip is the point"
    );
    // The whole spatial id-resolve (zone-map prune × projection) vs a naive whole-file pull.
    assert!(
        pct(fetched, tiles_whole) < 40.0,
        "pruned+projected spatial read must be a fraction of the tiles file, was {:.1}%",
        pct(fetched, tiles_whole)
    );
    // Monotonicity: a world-covering box can only fetch MORE than the small box.
    let world_hit = format!(
        "SELECT row_group_id FROM (SELECT row_group_id, \
           max(CASE WHEN path_in_schema='geo_min_lng' THEN CAST(stats_min_value AS DOUBLE) END) AS xlo, \
           max(CASE WHEN path_in_schema='geo_max_lng' THEN CAST(stats_max_value AS DOUBLE) END) AS xhi, \
           max(CASE WHEN path_in_schema='geo_min_lat' THEN CAST(stats_min_value AS DOUBLE) END) AS ylo, \
           max(CASE WHEN path_in_schema='geo_max_lat' THEN CAST(stats_max_value AS DOUBLE) END) AS yhi \
         FROM parquet_metadata('{}') GROUP BY row_group_id) \
         WHERE xlo <= 200 AND xhi >= -200 AND ylo <= 200 AND yhi >= -200",
        p("tiles").display()
    );
    let fetched_world = fetched_bytes(&meta, &p("tiles"), &geo_cols, &world_hit);
    assert!(
        fetched <= fetched_world,
        "zone-map model broken: small box ({fetched}) fetched more than world box ({fetched_world})"
    );

    // ---------------------------------------------------------------------------
    // CASE 3 — concept membership (service_role = Shelter) over the (node_id, concept_id)
    // -sorted melted concept store. Real reader resolves the ids; the concept_id band for
    // Shelter is contiguous, so a reader fetches only the row groups holding it.
    // ---------------------------------------------------------------------------
    let shelters = resolve(Expr::Concept {
        path: "service_role".into(),
        op: ConceptOp::Is,
        value: "Shelter".into(),
    });
    let brute_shelters = facts.iter().filter(|f| f.role == "Shelter").count();
    assert_eq!(
        shelters.len(),
        brute_shelters,
        "Concept(Shelter) must resolve exactly the Shelter resources"
    );
    // The stored concept_id for those resources (self-consistent — no hardcoded interning).
    let sh_list = shelters
        .iter()
        .take(64)
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let target: String = meta
        .query_row(
            &format!(
                "SELECT concept_id FROM read_parquet('{}') WHERE resource_id IN ({sh_list}) LIMIT 1",
                p("concepts").display()
            ),
            [],
            |r| r.get(0),
        )
        .expect("shelter concept_id");
    let con_hit = format!(
        "SELECT row_group_id FROM (SELECT row_group_id, \
           max(CASE WHEN path_in_schema='concept_id' THEN stats_min_value END) AS lo, \
           max(CASE WHEN path_in_schema='concept_id' THEN stats_max_value END) AS hi \
         FROM parquet_metadata('{}') GROUP BY row_group_id) WHERE lo <= '{target}' AND hi >= '{target}'",
        p("concepts").display()
    );
    let con_cols = [
        "resource_id",
        "tile_id",
        "nodegroup_id",
        "node_id",
        "concept_id",
    ];
    let fetched = fetched_bytes(&meta, &p("concepts"), &con_cols, &con_hit);
    let groups = count_groups(&meta, &con_hit);
    let tg = total_row_groups(&meta, &p("concepts"));
    let (_, frac) = report("3 concept = Shelter", fetched, groups, tg, &p("concepts"));
    assert!(
        groups < tg,
        "a single concept must prune the concept store ({groups} < {tg})"
    );
    assert!(
        frac < 60.0,
        "concept read must be a slice of the store, was {frac:.1}%"
    );

    // ---------------------------------------------------------------------------
    // CASE 4 — numeric range (height_m in [4,8]) over the (node_id, q_ordered)-sorted
    // ordered store. Real reader resolves via the typed Range IR, bounds quantized at the
    // shared scale; the q_ordered band is contiguous → few row groups.
    // ---------------------------------------------------------------------------
    let scale = number_scale(None);
    let (qlo, qhi) = (
        quantize_number(4.0, scale).unwrap(),
        quantize_number(8.0, scale).unwrap(),
    );
    let low = resolve(Expr::Range {
        path: "height_m".into(),
        lo: qlo,
        hi: qhi,
    });
    let brute_low = facts
        .iter()
        .filter(|f| f.height >= 4 && f.height <= 8)
        .count();
    assert_eq!(
        low.len(),
        brute_low,
        "Range(height 4..8) must resolve exactly the matching resources"
    );
    let ord_hit = format!(
        "SELECT row_group_id FROM (SELECT row_group_id, \
           max(CASE WHEN path_in_schema='q_ordered' THEN CAST(stats_min_value AS BIGINT) END) AS lo, \
           max(CASE WHEN path_in_schema='q_ordered' THEN CAST(stats_max_value AS BIGINT) END) AS hi \
         FROM parquet_metadata('{}') GROUP BY row_group_id) WHERE lo <= {qhi} AND hi >= {qlo}",
        p("ordered").display()
    );
    let ord_cols = [
        "resource_id",
        "tile_id",
        "nodegroup_id",
        "node_id",
        "q_ordered",
    ];
    let fetched = fetched_bytes(&meta, &p("ordered"), &ord_cols, &ord_hit);
    let groups = count_groups(&meta, &ord_hit);
    let tg = total_row_groups(&meta, &p("ordered"));
    let (_, frac) = report(
        "4 numeric range height 4..8",
        fetched,
        groups,
        tg,
        &p("ordered"),
    );
    assert!(
        groups < tg,
        "a narrow range must prune the ordered store ({groups} < {tg})"
    );
    assert!(
        frac < 60.0,
        "range read must be a slice of the store, was {frac:.1}%"
    );

    // ---------------------------------------------------------------------------
    // CASE 5 — id-probe hydrate (open one card) over the resource_id-sorted entry table.
    // Real reader: `resource_entries([id])` returns that resource's spine. A reader fetches
    // ~one row group — the entry table is why hydrating a single card is a point read, not a
    // scan.
    // ---------------------------------------------------------------------------
    let one = facts[N / 2].id.clone();
    let entries = duck
        .resource_entries(&[one.clone()])
        .expect("resource_entries");
    assert!(
        entries.contains_key(&one),
        "entry table must resolve the probed id"
    );
    let id_hit = format!(
        "SELECT row_group_id FROM (SELECT row_group_id, \
           max(CASE WHEN path_in_schema='resource_id' THEN stats_min_value END) AS lo, \
           max(CASE WHEN path_in_schema='resource_id' THEN stats_max_value END) AS hi \
         FROM parquet_metadata('{}') GROUP BY row_group_id) WHERE lo <= '{one}' AND hi >= '{one}'",
        p("resources").display()
    );
    let res_cols = [
        "resource_id",
        "graph_id",
        "legacyid",
        "descriptor",
        "cluster_key",
        "geo_descriptor",
        "render_summary",
        "render_fields",
        "descriptor_description",
    ];
    let fetched = fetched_bytes(&meta, &p("resources"), &res_cols, &id_hit);
    let groups = count_groups(&meta, &id_hit);
    let tg = total_row_groups(&meta, &p("resources"));
    let (_, frac) = report(
        "5 id-probe hydrate (1 card)",
        fetched,
        groups,
        tg,
        &p("resources"),
    );
    assert!(
        groups <= 2,
        "an id probe must hit ~1 entry-table row group (got {groups})"
    );
    assert!(
        frac < 40.0,
        "card hydrate read must be a fraction of the entry table, was {frac:.1}%"
    );

    // Overall: every real query answered correctly, and each read a slice of the fileset.
    eprintln!(
        "\nOK: 5 crossing query families — each returns the correct answer while a Range reader \
         fetches a bounded slice of the {} KB fileset.",
        total_fileset / 1024
    );
}
