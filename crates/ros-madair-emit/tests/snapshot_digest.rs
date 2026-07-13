// SPDX-License-Identifier: AGPL-3.0-or-later
//! The snapshot id is the identity of a DEPLOYMENT, not just of its data.
//!
//! `hash_artifacts()` used to cover head.sqlite + closure.json + chunks/* and
//! nothing else — the manifest is written afterwards, so it was never hashed.
//! Two emits of the same data with different manifests (a different handler
//! registry, a different tier, different declared field classes) therefore
//! produced the SAME snapshot id while answering queries differently.
//!
//! The demo fixture lives outside the repo (Clódóir's demo data); point
//! `ROS_MADAIR_DEMO_DATA` elsewhere to move it. Absent → the tests skip.

use std::path::PathBuf;

use alizarin_core::ExtensionTypeRegistry;
use ros_madair_emit::{default_registry, EmitOptions};

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";

fn demo_data() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ROS_MADAIR_DEMO_DATA").unwrap_or_else(|_| DEMO_DATA.to_string()),
    );
    dir.join("graphs").is_dir().then_some(dir)
}

/// Emit the demo into a fresh temp dir with the given registry; return the
/// snapshot id and the emitted manifest.
fn emit(tag: &str, registry: &ExtensionTypeRegistry) -> (String, serde_json::Value) {
    let data = demo_data().expect("checked by caller");
    let out = std::env::temp_dir().join(format!("rm-digest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let summary = ros_madair_emit::emit_with_options(
        data.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
        &EmitOptions::default(),
        registry,
    )
    .expect("emit");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["snapshot_id"], summary.snapshot_id);
    (summary.snapshot_id, manifest)
}

/// Determinism is the whole premise: same data, same options → same id.
#[test]
fn identical_emits_produce_identical_snapshot_ids() {
    if demo_data().is_none() {
        eprintln!("demo fixture absent — skipping");
        return;
    }
    let (a, ma) = emit("det-a", &default_registry());
    let (b, mb) = emit("det-b", &default_registry());
    assert_eq!(a, b, "snapshot id must be run-stable");
    assert_eq!(ma, mb, "manifest must be byte-stable");
}

/// The bug: a manifest-only difference. The demo carries no `reference`
/// fields, so emitting with and without the CLM handler indexes byte-identical
/// artifacts — ONLY the manifest's `handlers` block differs. Before the fix
/// these two shared a snapshot id, i.e. a deployment whose query side rebuilds
/// its registry from the artifact could not tell them apart. They must differ.
#[test]
fn a_manifest_only_change_moves_the_snapshot_id() {
    if demo_data().is_none() {
        eprintln!("demo fixture absent — skipping");
        return;
    }
    let (with_clm, m_with) = emit("reg-with", &default_registry());
    let (without_clm, m_without) = emit("reg-without", &ExtensionTypeRegistry::new());

    // The data artifacts really are identical — this IS a manifest-only change.
    assert_ne!(m_with["handlers"], m_without["handlers"]);
    assert_eq!(
        m_with["artifacts"], m_without["artifacts"],
        "the fixture must have no reference fields for this test to mean anything"
    );

    // Data-identical, manifest-different: under a digest that hashes only the
    // data artifacts these two are indistinguishable. Under this one they are
    // not.
    assert_ne!(
        with_clm, without_clm,
        "a different declared handler set must be a different snapshot"
    );
}
