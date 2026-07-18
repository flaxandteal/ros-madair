// SPDX-License-Identifier: AGPL-3.0-or-later
//! **A1(a): the spine `display_name` is the EVALUATED descriptor, not the raw
//! template.**
//!
//! `spine.display_name` is the resource→descriptor index consumers use to show a
//! name without re-hydrating the resource. It used to emit the resource's stored
//! `name`, which in an export whose descriptors were never computed is the
//! unrendered template (`'<Headword>'`) — structurally present, useless. Emit now
//! evaluates the descriptor template against the resource's own tiles.
//!
//! The demo Talk model's descriptor template is `<title>` but its node is *named*
//! `Title`; Arches matches placeholders on node name, case-sensitively, so the
//! demo template does not resolve as shipped. That is itself the two cases worth
//! pinning: a template that resolves (patched to `<Title>`) must WIN over the raw
//! name; one that does not (`<title>`) must FALL BACK to it, never emit a literal
//! `<title>`.
//!
//! Demo fixture lives outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it,
//! absent → skip.

use std::path::{Path, PathBuf};

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
/// Talk `3 W's of UI`: stored `name`, vs its `Title` tile value.
const TALK_A: &str = "179b8583-3140-437e-bd0b-34d5aa2f1550";
const TALK_A_RAW_NAME: &str = "3 W's of UI";
const TALK_A_TITLE: &str = "Wudgets, widgets and wodgets";

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

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-desc-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Copy the demo corpus; optionally rewrite the Talk name-descriptor template.
fn corpus(tag: &str, demo: &Path, name_template: Option<&str>) -> PathBuf {
    let dir = scratch(tag);
    copy_dir(demo, &dir);
    if let Some(t) = name_template {
        let gp = dir.join("graphs").join(format!("{TALK_GRAPH}.json"));
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
        let fxg = &mut doc["graph"][0]["functions_x_graphs"][0];
        fxg["config"]["descriptor_types"]["name"]["string_template"] = serde_json::json!(t);
        std::fs::write(&gp, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    }
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

fn talk_display_name(head: &Path, uuid: &str) -> String {
    let conn = rusqlite::Connection::open(head.join("head.sqlite")).unwrap();
    conn.query_row(
        "SELECT s.display_name FROM spine_talk s
           JOIN dict d ON d.term_id = s.term_id
          WHERE d.term = ?1",
        [uuid],
        |r| r.get(0),
    )
    .unwrap()
}

/// A template that RESOLVES: the evaluated title wins over the stored `name`.
#[test]
fn a_resolving_descriptor_is_evaluated_into_the_spine() {
    let Some(demo) = demo_data() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    // `<Title>` matches the node named `Title` (the shipped `<title>` does not).
    let head = emit("resolves", &corpus("resolves", &demo, Some("<Title>")));

    let name = talk_display_name(&head, TALK_A);
    assert_eq!(
        name, TALK_A_TITLE,
        "the spine must hold the EVALUATED title, not the stored name '{TALK_A_RAW_NAME}' \
         and never the literal template"
    );
    assert_ne!(
        name, TALK_A_RAW_NAME,
        "and it must genuinely differ from the raw name — otherwise the test proves nothing"
    );
}

/// A template that does NOT resolve (case mismatch) falls back to the stored
/// name — never emits a literal `<title>`. This is the no-regression guard: the
/// fix must not make a resource with a real stored name worse.
#[test]
fn an_unresolvable_descriptor_falls_back_to_the_stored_name() {
    let Some(demo) = demo_data() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    // Shipped `<title>` vs node `Title` — Arches name-match is case-sensitive.
    let head = emit("fallback", &corpus("fallback", &demo, None));

    let name = talk_display_name(&head, TALK_A);
    assert_eq!(
        name, TALK_A_RAW_NAME,
        "an unresolved template must fall back to the stored name"
    );
    assert!(
        !name.contains('<'),
        "and must NEVER emit a residual placeholder like '<title>': {name}"
    );
}
