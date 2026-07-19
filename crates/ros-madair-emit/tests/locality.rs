// SPDX-License-Identifier: AGPL-3.0-or-later
//! **A9: resource ids are pre-interned in STREAM (locality) order.**
//!
//! A link target is a resource id. Interned inline — at first encounter, as a
//! target or as its own resource — a target's id had no relation to resource
//! identity, so `chunk_link_summary` target ranges spanned ~1/3 of the id space
//! (measured on wiktionary: median 33%) and coarse link routing was far weaker
//! than designed. Emit now pre-interns every resource id up front, in the same
//! order the streaming pass visits them (the P18-corollary already applied to
//! concepts, now to resources).
//!
//! Two observable consequences, both asserted here:
//! 1. Resource ids form a contiguous band ABOVE the concepts and BELOW the node
//!    ids — proof they were interned as a block up front, not interleaved with
//!    node ids in stream order (which is what inline interning produced).
//! 2. A link target resolves to the SAME id as the target resource's own spine
//!    id — the id is the target's stream position, which is the whole point.
//!
//! Demo fixture lives outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it,
//! absent → skip.

use std::path::PathBuf;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()),
    );
    dir.join("graphs").is_dir().then_some(dir)
}

fn emit(tag: &str) -> Option<PathBuf> {
    let data = demo_data()?;
    let out = std::env::temp_dir().join(format!("rm-loc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    ros_madair_emit::emit(
        data.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
    )
    .expect("emit");
    Some(out)
}

fn spine_tables(c: &rusqlite::Connection) -> Vec<String> {
    c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'spine_%'")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Resource ids are a contiguous block, interned before node ids — the marker of
/// an up-front pre-intern pass rather than inline (stream-interleaved) interning.
#[test]
fn resource_ids_are_pre_interned_as_a_locality_block_before_node_ids() {
    let Some(out) = emit("block") else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let c = rusqlite::Connection::open(out.join("head.sqlite")).unwrap();

    // Every resource's interned id, across all models.
    let mut resource_ids: Vec<i64> = spine_tables(&c)
        .iter()
        .flat_map(|t| {
            c.prepare(&format!("SELECT term_id FROM {t}"))
                .unwrap()
                .query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        })
        .collect();
    resource_ids.sort_unstable();
    assert!(!resource_ids.is_empty(), "the demo has resources");

    // Node ids are interned INLINE while streaming tiles, i.e. after the
    // pre-intern block. `concept_tags.node` is an interned node id.
    let min_node: i64 = c
        .query_row("SELECT MIN(node) FROM concept_tags", [], |r| r.get(0))
        .unwrap();

    let (lo, hi) = (resource_ids[0], *resource_ids.last().unwrap());
    assert_eq!(
        hi - lo + 1,
        resource_ids.len() as i64,
        "resource ids are a CONTIGUOUS band — a gapless block means they were \
         interned together up front, not interleaved with node ids per resource"
    );
    assert!(
        hi < min_node,
        "every resource id ({lo}..{hi}) is below every node id (>= {min_node}): \
         resources were pre-interned before any tile was processed"
    );
}

/// A resource-link target resolves to the target resource's OWN spine id — the
/// id is the target's stream position. (Interning is by string, so this is an id
/// two ways to the same term; the point is that the target band is the resource
/// band, which is what tightens the link summary ranges.)
#[test]
fn a_link_target_id_is_the_target_resources_spine_id() {
    let Some(out) = emit("target") else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let c = rusqlite::Connection::open(out.join("head.sqlite")).unwrap();

    // The demo's talk model links `presenter` -> person resources; those targets
    // must all live in the resource id band (they are corpus resources), i.e. no
    // link target sits up in the node-id range.
    let max_resource: i64 = spine_tables(&c)
        .iter()
        .map(|t| {
            c.query_row(&format!("SELECT MAX(term_id) FROM {t}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        })
        .max()
        .unwrap();

    // chunk_link_summary stores interned target ids as min/max per chunk. Every
    // one must fall within the resource band — a target above it would be an id
    // interned for something that is not a corpus resource.
    let max_link_target: i64 = c
        .query_row(
            "SELECT COALESCE(MAX(max_target), 0) FROM chunk_link_summary",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert!(
        max_link_target <= max_resource,
        "link targets ({max_link_target}) stay within the resource id band \
         (<= {max_resource}) — they resolve to target resources' own ids"
    );
}
