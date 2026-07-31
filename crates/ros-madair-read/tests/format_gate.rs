// SPDX-License-Identifier: AGPL-3.0-or-later
//! **P17: the reader refuses a format-skewed snapshot, coherently across all
//! three artifacts.** A snapshot emitted at the current `FORMAT_VERSION` reads
//! fine; tamper the head's `user_version`, the manifest's `format_version`, or a
//! chunk's framing header, and the matching read fails LOUDLY — never silently
//! misreads. This is the whole point of the version gate: chunk payloads are
//! msgpack named maps that decode cleanly under field drift.
//!
//! Demo fixture lives outside the repo; `ROS_MADAIR_DEMO_DATA` relocates it,
//! absent → skip.

use std::path::{Path, PathBuf};

use alizarin_core::graph::StaticGraph;
use ros_madair_read::{hydrate_resource, open_head, Layers, ReadError};
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TITLE_NG: &str = "7d8e443d-1ae0-11f0-8c5c-8fd6f4eb1a02";
const TALK: &str = "5a5a5a5a-0000-4000-8000-000000000001";

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
    let dir = std::env::temp_dir().join(format!("rm-p17-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Emit a one-resource snapshot (a Talk with a title) and return (head, graph).
fn emit_snapshot() -> Option<(PathBuf, StaticGraph)> {
    let demo = demo_data()?;
    let src = scratch("src");
    copy_dir(&demo.join("graphs"), &src.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &src.join("vocabularies"));
    let talk_dir = src.join("resources").join("talk");
    std::fs::create_dir_all(&talk_dir).unwrap();
    std::fs::write(
        talk_dir.join("talks.json"),
        serde_json::to_vec(&json!({ "business_data": { "resources": [{
            "resourceinstance": {
                "resourceinstanceid": TALK, "graph_id": TALK_GRAPH, "name": "T", "legacyid": null,
                "descriptors": { "en": { "name": "T", "description": "", "map_popup": "" } },
            },
            "tiles": [{
                "tileid": "7a7a0000-0000-4000-8000-000000000001",
                "nodegroup_id": TITLE_NG, "parenttile_id": null,
                "resourceinstance_id": TALK, "sortorder": 0, "provisionaledits": null,
                "data": { TITLE_NG: { "en": { "value": "Hello", "direction": "ltr" } } },
            }],
        }] } }))
        .unwrap(),
    )
    .unwrap();

    let out = scratch("head");
    ros_madair_emit::emit(src.to_str().unwrap(), out.to_str().unwrap(), "https://example.org/")
        .expect("emit");

    let gp = src.join("graphs").join(format!("{TALK_GRAPH}.json"));
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&gp).unwrap()).unwrap();
    let mut graph: StaticGraph = serde_json::from_value(raw["graph"][0].clone()).unwrap();
    graph.build_indices();
    Some((out, graph))
}

/// The happy path: a current-version snapshot reads with no gate error.
#[test]
fn a_current_version_snapshot_reads() {
    let Some((out, graph)) = emit_snapshot() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    assert!(open_head(&out).is_ok(), "head opens at the current version");
    assert!(Layers::open(&[out.as_path()]).is_ok(), "layer opens");
    let tree = hydrate_resource(&out, TALK, &graph, &["en"]).unwrap();
    assert!(tree.is_object(), "the resource hydrates");
}

/// Head arm: a wrong `PRAGMA user_version` is refused by `open_head` (and hence
/// every read that goes through it), not surfaced later as a missing table.
#[test]
fn a_skewed_head_user_version_is_refused() {
    let Some((out, graph)) = emit_snapshot() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    // Bump the head's user_version out from under the reader.
    let rw = rusqlite::Connection::open(out.join("head.sqlite")).unwrap();
    rw.pragma_update(None, "user_version", 999).unwrap();
    drop(rw);

    assert!(
        matches!(open_head(&out), Err(ReadError::FormatSkew { found: 999, .. })),
        "open_head rejects a skewed head"
    );
    // …and so the whole read fails, loudly.
    assert!(matches!(
        hydrate_resource(&out, TALK, &graph, &["en"]),
        Err(ReadError::FormatSkew { .. })
    ));
}

/// Manifest arm: a wrong `format_version` in manifest.json is refused by
/// `Layers::open` before composition begins.
#[test]
fn a_skewed_manifest_format_version_is_refused() {
    let Some((out, _graph)) = emit_snapshot() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let mpath = out.join("manifest.json");
    let mut m: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mpath).unwrap()).unwrap();
    m["format_version"] = json!(42);
    std::fs::write(&mpath, serde_json::to_vec(&m).unwrap()).unwrap();

    assert!(
        matches!(
            Layers::open(&[out.as_path()]),
            Err(ReadError::FormatSkew { found: 42, .. })
        ),
        "Layers::open rejects a skewed manifest"
    );
}

/// Chunk arm: a chunk whose framing header is clobbered is refused on read,
/// rather than the named-map body decoding cleanly to the wrong thing.
#[test]
fn a_skewed_chunk_header_is_refused() {
    let Some((out, graph)) = emit_snapshot() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    // Clobber the RMC magic on every chunk.
    for e in std::fs::read_dir(out.join("chunks")).unwrap() {
        let p = e.unwrap().path();
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[0] = b'Z'; // break the magic
        std::fs::write(&p, &bytes).unwrap();
    }
    // Head + manifest are untouched, so this must be a CHUNK error specifically.
    assert!(
        matches!(hydrate_resource(&out, TALK, &graph, &["en"]), Err(ReadError::Chunk { .. })),
        "a clobbered chunk header is refused on hydrate"
    );
}
