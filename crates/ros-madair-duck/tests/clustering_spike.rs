// SPDX-License-Identifier: AGPL-3.0-or-later
//! **SPIKE / benchmark (run on demand): graph-aware clustering vs value clustering.**
//!
//! `cargo test -p ros-madair-duck --test clustering_spike -- --ignored --nocapture`
//!
//! The multi-hop thesis: a traversal's cost is dominated by how many row groups
//! the frontier touches, and that is a function of TILE ORDERING. Today RM
//! clusters tiles by geo ⊕ descriptor — a VALUE order that is uncorrelated with
//! the edge graph, so a connected frontier scatters across the file. A
//! CONNECTIVITY order (co-locate linked resources) keeps the frontier local.
//!
//! This harness builds a synthetic community-structured resource graph (the shape
//! heritage data actually has — a site's features/events/actors link within the
//! site), assigns each resource a row-group index under two orderings, and
//! measures, over many 2-hop seeds, how many row groups the frontier spans:
//!
//!   - value order  ≈ `ORDER BY hash(rid)`         (proxy for geo/descriptor)
//!   - connectivity ≈ `ORDER BY community, rid`     (a cheap connectivity cluster)
//!
//! It asserts connectivity clustering wins by a wide margin. It is NOT a unit test
//! of RM code — it is the executable evidence behind the clustering design, so it
//! is `#[ignore]`d out of the normal suite.

use duckdb::Connection;

const N: i64 = 100_000; // resources
const S: i64 = 100; // community size  → C = N/S = 1000 communities
const D: i64 = 8; // intra-community out-degree
const RGS: i64 = 2048; // row-group size (DuckDB's vector-size floor)

#[test]
#[ignore = "spike/benchmark; run explicitly with --ignored"]
fn connectivity_clustering_localises_multi_hop_frontiers() {
    let conn = Connection::open_in_memory().unwrap();

    // Resources tagged with their community (block structure).
    conn.execute_batch(&format!(
        "CREATE TABLE res AS SELECT i AS rid, (i // {S}) AS community FROM range({N}) t(i);"
    ))
    .unwrap();

    // Edges: D intra-community links (a ring within the community) + 1 cross-community
    // link to the same local offset in the next community. Deterministic, no RNG.
    conn.execute_batch(&format!(
        "CREATE TABLE edge AS \
         SELECT i AS src, (i // {S})*{S} + ((i % {S}) + k) % {S} AS dst \
           FROM range({N}) t(i), range(1, {D}+1) kk(k) \
         UNION ALL \
         SELECT i AS src, ((((i // {S}) + 1) % ({N} // {S}))*{S} + (i % {S})) AS dst \
           FROM range({N}) t(i);"
    ))
    .unwrap();

    // Row-group index under each ordering. hash(rid) is a fair proxy for a
    // value/descriptor order: deterministic, uncorrelated with community.
    conn.execute_batch(&format!(
        "CREATE TABLE pos AS SELECT rid, community, \
           (row_number() OVER (ORDER BY hash(rid)) - 1) // {RGS} AS rg_value, \
           (row_number() OVER (ORDER BY community, rid) - 1) // {RGS} AS rg_conn \
         FROM res;"
    ))
    .unwrap();

    // Average, over many seeds spread across the graph, the number of row groups a
    // 2-hop frontier (seed ∪ neighbours ∪ neighbours-of-neighbours) spans.
    let mut tot_frontier = 0i64;
    let mut tot_value = 0i64;
    let mut tot_conn = 0i64;
    let seeds: Vec<i64> = (0..64).map(|k| k * (N / 64) + S / 2).collect();
    for &seed in &seeds {
        let (fsize, gv, gc): (i64, i64, i64) = conn
            .query_row(
                &format!(
                    "WITH n1 AS (SELECT dst AS rid FROM edge WHERE src = {seed}), \
                          n2 AS (SELECT e.dst AS rid FROM edge e JOIN n1 ON e.src = n1.rid), \
                          frontier AS (SELECT DISTINCT rid FROM ( \
                              SELECT {seed} AS rid UNION ALL SELECT rid FROM n1 \
                              UNION ALL SELECT rid FROM n2)) \
                     SELECT count(*), count(DISTINCT rg_value), count(DISTINCT rg_conn) \
                       FROM frontier JOIN pos USING (rid);"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        tot_frontier += fsize;
        tot_value += gv;
        tot_conn += gc;
    }

    let n = seeds.len() as i64;
    let total_groups: i64 = conn
        .query_row("SELECT max(rg_value) + 1 FROM pos", [], |r| r.get(0))
        .unwrap();
    let avg_f = tot_frontier as f64 / n as f64;
    let avg_v = tot_value as f64 / n as f64;
    let avg_c = tot_conn as f64 / n as f64;
    eprintln!("--- graph-clustering spike ---");
    eprintln!("corpus: {N} resources, {} communities, {total_groups} row groups", N / S);
    eprintln!("avg 2-hop frontier size: {avg_f:.0} resources");
    eprintln!("avg row groups touched — value order:        {avg_v:.1}");
    eprintln!("avg row groups touched — connectivity order: {avg_c:.1}");
    eprintln!("locality win: {:.1}x fewer row groups", avg_v / avg_c);

    // The claim: connectivity clustering touches dramatically fewer row groups for
    // a connected frontier. (Value order scatters it across most of the file.)
    assert!(
        avg_c * 5.0 < avg_v,
        "connectivity clustering should touch >5x fewer row groups (value {avg_v:.1}, conn {avg_c:.1})"
    );
}
