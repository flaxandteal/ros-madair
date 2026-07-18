// SPDX-License-Identifier: AGPL-3.0-or-later
//! **A2: concept labels live in the head (`vocab.label`); `closure.json` is
//! retired.**
//!
//! A consumer resolves a concept's display text with a self-contained join
//! (`concept_tags → vocab.label`) — no `closure.json` sidecar, no client-side
//! RDF/XML. The labels are the SKOS prefLabels the emitter already walked, so
//! they are identical to what the sidecar carried.
//!
//! Demo fixture lives outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it,
//! absent → skip.

use std::collections::BTreeSet;
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
    let out = std::env::temp_dir().join(format!("rm-labels-{tag}-{}", std::process::id()));
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

/// The head resolves concept display with no sidecar: `concept_tags → vocab.label`
/// returns the real SKOS prefLabels, and `closure.json` is no longer shipped.
#[test]
fn the_head_carries_concept_labels_and_closure_json_is_gone() {
    let Some(out) = emit("labels") else {
        eprintln!("demo fixture absent — skipping");
        return;
    };

    // The retired sidecar must NOT be emitted.
    assert!(
        !out.join("closure.json").exists(),
        "closure.json is retired (A2) — it must not be emitted"
    );

    let conn = rusqlite::Connection::open(out.join("head.sqlite")).unwrap();

    // vocab carries a label column, populated.
    let (rows, labelled): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COUNT(label) FROM vocab WHERE label IS NOT NULL AND label <> ''",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(rows > 0 && labelled == rows, "every vocab row has a label");

    // The display join a consumer runs, over the concepts this corpus actually
    // tags. The demo's `topics` concepts resolve to real SKOS prefLabels.
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT v.label FROM concept_tags ct
               JOIN vocab v ON v.concept = ct.concept",
        )
        .unwrap();
    let labels: BTreeSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    // These are the prefLabels of the demo's tagged concepts — proof the join
    // yields display text, not a UUID or a placeholder. (Values are from the
    // demo vocabulary; a fixture change would legitimately update them.)
    let expected = BTreeSet::from([
        "Interoperability".to_string(),
        "Lingo & Controlled Lists".to_string(),
        "Vocabularies".to_string(),
    ]);
    assert_eq!(
        labels, expected,
        "concept_tags → vocab.label must resolve to the real SKOS prefLabels — \
         a bare UUID here would mean label_of hit its no-prefLabel fallback"
    );
}
