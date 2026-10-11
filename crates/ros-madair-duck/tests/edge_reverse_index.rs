// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Reverse edge index (`redges_<graph>.parquet`): target-anchored pruning.**
//!
//! The forward edge table is sorted `(src_node, target_resource)`, so a probe
//! that fixes `src_node` (a typed hop) prunes on target WITHIN that node's band.
//! But an UNTYPED reverse probe — "which resources link to T through *any*
//! predicate?" — or a shared-neighbour self-join ("resources that share a related
//! resource", the v1 "people in the same city" pattern) does NOT fix `src_node`,
//! and then the forward file's per-row-group target ranges span the whole domain
//! across every node band: T appears once per band, so you scan one row group per
//! predicate. The reverse index sorts the SAME rows `(target_resource, src_node)`,
//! so every row group covers a disjoint target slice and T lives in ONE.
//!
//! This test builds a corpus with TWO link predicates (L1, L2) over a shared
//! target domain, emits with a tiny row-group size to force multiple groups, and
//! asserts the structural win directly from `parquet_metadata`: a mid-domain
//! target is contained by 2 forward row groups (one per band) but 1 reverse one —
//! and the two files agree on the untyped-reverse result set (pruning changes
//! cost, never answers).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use duckdb::Connection;
use ros_madair_duck::DuckReader;
use ros_madair_emit::ClusterConfig;
use ros_madair_handlers::default_registry;
use serde_json::json;

const GRAPH: &str = "e0e0e0e0-0000-4000-8000-000000000001";
const ROOT: &str = "e0e0e0e0-1ae0-11f0-b22a-8fd6f4eb1a02";
// Two link nodegroups (collectors NG1/NG2), each with a resource-instance-list
// link node (L1/L2). L1's id sorts before L2's, so L1 is the first band in the
// forward (src_node-first) file.
const NG1: &str = "11110000-0000-4000-8000-00000000000a";
const L1: &str = "11110000-0000-4000-8000-00000000000b";
const NG2: &str = "22220000-0000-4000-8000-00000000000a";
const L2: &str = "22220000-0000-4000-8000-00000000000b";

// DuckDB flushes row groups on a 2048-row vector-size floor, so the corpus must
// clear it to produce multiple groups. 4096 resources × 2 predicates = 8192 edge
// rows; with row_group_size 2048 that is 4 groups, and the counts divide cleanly
// so a mid-domain target lands in exactly 1 reverse group vs 2 forward bands.
const N_RESOURCES: usize = 4096;
const M_TARGETS: usize = 16; // shared target domain, zero-padded so lexical == numeric
const ROW_GROUP_SIZE: usize = 2048;

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-redges-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn edge(id: &str, from: &str, to: &str) -> serde_json::Value {
    json!({ "edgeid": id, "domainnode_id": from, "rangenode_id": to, "graph_id": GRAPH })
}

fn node(nodeid: &str, ng: &str, alias: &str, datatype: &str, collector: bool) -> serde_json::Value {
    json!({
        "nodeid": nodeid, "nodegroup_id": ng, "name": alias, "alias": alias,
        "datatype": datatype, "graph_id": GRAPH, "istopnode": false, "is_collector": collector,
        "isrequired": false, "issearchable": true, "exportable": false, "sortorder": 0,
    })
}

fn write_graph(gp: &Path) {
    std::fs::create_dir_all(gp.parent().unwrap()).unwrap();
    let root = json!({
        "nodeid": ROOT, "name": "Person", "alias": "person", "datatype": "semantic",
        "graph_id": GRAPH, "istopnode": true,
    });
    let doc = json!({ "graph": [{
        "graphid": GRAPH, "name": "Person", "root": root.clone(),
        "nodes": [
            root,
            node(NG1, NG1, "lives_in", "semantic", true),
            node(L1, NG1, "lives_in_place", "resource-instance-list", false),
            node(NG2, NG2, "works_in", "semantic", true),
            node(L2, NG2, "works_in_place", "resource-instance-list", false),
        ],
        "nodegroups": [
            { "nodegroupid": NG1, "cardinality": "n", "parentnodegroup_id": null },
            { "nodegroupid": NG2, "cardinality": "n", "parentnodegroup_id": null },
        ],
        "edges": [
            edge("e0000000-0000-4000-8000-000000000001", ROOT, NG1),
            edge("e0000000-0000-4000-8000-000000000002", NG1, L1),
            edge("e0000000-0000-4000-8000-000000000003", ROOT, NG2),
            edge("e0000000-0000-4000-8000-000000000004", NG2, L2),
        ],
    }]});
    std::fs::write(gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

/// Zero-padded target id so lexical order matches numeric order.
fn target(k: usize) -> String {
    format!("t{k:04}")
}

fn link_tile(tile: &str, r: &str, ng: &str, node_id: &str, tgt: &str) -> serde_json::Value {
    json!({
        "tileid": tile, "nodegroup_id": ng, "parenttile_id": null,
        "resourceinstance_id": r, "sortorder": 0, "provisionaledits": null,
        "data": { node_id: [{ "resourceId": tgt }] },
    })
}

fn corpus() -> (PathBuf, PathBuf, PathBuf) {
    let dir = scratch("corpus");
    write_graph(&dir.join("graphs").join(format!("{GRAPH}.json")));
    let rdir = dir.join("resources").join("person");
    std::fs::create_dir_all(&rdir).unwrap();

    let mut resources = Vec::with_capacity(N_RESOURCES);
    for i in 0..N_RESOURCES {
        let r = format!("r{i:06}-0000-4000-8000-000000000001");
        // Both predicates point at the SAME target for this resource, so every
        // target is reached through BOTH L1 and L2 — the case that scatters a
        // target across two node bands in the forward file.
        let tgt = target(i % M_TARGETS);
        let t1 = format!("a{i:06}-0000-4000-8000-00000000000a");
        let t2 = format!("b{i:06}-0000-4000-8000-00000000000b");
        resources.push(json!({
            "resourceinstance": { "resourceinstanceid": r, "graph_id": GRAPH, "name": r,
                "legacyid": null,
                "descriptors": { "en": { "name": r, "description": "", "map_popup": "" } } },
            "tiles": [
                link_tile(&t1, &r, NG1, L1, &tgt),
                link_tile(&t2, &r, NG2, L2, &tgt),
            ],
        }));
    }
    std::fs::write(
        rdir.join("people.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();

    let mut cfg = HashMap::new();
    cfg.insert(
        GRAPH.to_string(),
        ClusterConfig {
            row_group_size: ROW_GROUP_SIZE,
            ..Default::default()
        },
    );
    let pq = scratch("pq");
    ros_madair_emit::emit_parquet(
        dir.to_str().unwrap(),
        pq.to_str().unwrap(),
        "https://example.org/",
        &default_registry(),
        &cfg,
    )
    .expect("emit parquet");

    (
        pq.join(format!("tiles_{GRAPH}.parquet")),
        pq.join(format!("edges_{GRAPH}.parquet")),
        pq.join(format!("redges_{GRAPH}.parquet")),
    )
}

/// How many row groups of `file` have a `target_resource` [min,max] band that
/// contains `tgt` — i.e. how many groups a target-anchored scan cannot prune.
fn row_groups_containing(conn: &Connection, file: &Path, tgt: &str) -> i64 {
    conn.query_row(
        &format!(
            "SELECT count(*) FROM parquet_metadata('{}') \
             WHERE path_in_schema = 'target_resource' \
               AND stats_min <= '{tgt}' AND stats_max >= '{tgt}'",
            file.display()
        ),
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn reverse_index_prunes_a_target_the_forward_index_cannot() {
    let (tiles, edges, redges) = corpus();
    assert!(redges.exists(), "emit wrote the reverse edge index redges_*.parquet");

    let conn = Connection::open_in_memory().unwrap();

    // Both files hold the SAME rows (reverse is a re-sort, not a subset).
    let count = |f: &Path| -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM read_parquet('{}')", f.display()),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(&edges), count(&redges), "reverse index is a re-sort of the same edges");
    assert_eq!(count(&edges), (N_RESOURCES * 2) as i64);

    // Both files have >1 row group (otherwise there is nothing to prune).
    let groups = |f: &Path| -> i64 {
        conn.query_row(
            &format!(
                "SELECT count(DISTINCT row_group_id) FROM parquet_metadata('{}')",
                f.display()
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    eprintln!(
        "rows: edges={} redges={}  groups: edges={} redges={}",
        count(&edges),
        count(&redges),
        groups(&edges),
        groups(&redges)
    );
    assert!(groups(&edges) > 1 && groups(&redges) > 1, "need multiple row groups to prune");

    // The structural win: a mid-domain target is scattered across both predicate
    // bands in the forward file (≥2 unprunable row groups) but contiguous in the
    // reverse file (exactly 1).
    let tgt = target(M_TARGETS / 2 - 1); // a target squarely inside the domain
    let fwd = row_groups_containing(&conn, &edges, &tgt);
    let rev = row_groups_containing(&conn, &redges, &tgt);
    eprintln!("target {tgt}: forward touches {fwd} row groups, reverse touches {rev}");
    assert_eq!(rev, 1, "reverse index localises the target to one row group");
    assert!(fwd >= 2, "forward index scatters the target across ≥2 bands (got {fwd})");

    // Correctness: an UNTYPED reverse lookup (who links to T through any predicate)
    // is identical whichever file answers it — pruning changes cost, not results.
    let who_links_to = |f: &Path| -> Vec<String> {
        let mut st = conn
            .prepare(&format!(
                "SELECT DISTINCT src_resource FROM read_parquet('{}') \
                 WHERE target_resource = '{tgt}' ORDER BY src_resource",
                f.display()
            ))
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    };
    assert_eq!(who_links_to(&edges), who_links_to(&redges), "same answer, cheaper scan");
    assert!(!who_links_to(&redges).is_empty());

    // Regression: the reader still opens with the new redges_ sibling present.
    DuckReader::open(tiles.to_str().unwrap()).expect("reader opens with reverse index present");

    eprintln!("OK: reverse edge index prunes target-anchored access the forward index cannot");
}
