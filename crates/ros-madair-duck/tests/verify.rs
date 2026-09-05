// SPDX-License-Identifier: AGPL-3.0-or-later
//! The sign→verify loop over a real snapshot directory.
//!
//! `ros-madair-emit`'s `seal_and_sign` builds and signs the manifest (hashing the
//! snapshot's files into the `snapshot_id`); `ros-madair-duck` (via
//! `ros-madair-format`'s `verify` module) recomputes that id from the files on
//! disk and checks the attestation. This proves the two agree — a signed id and a
//! recomputed id cannot drift — and that `open_layers` refuses a tampered layer.

use std::fs;
use std::path::{Path, PathBuf};

use ros_madair_duck::{verify_snapshot, DuckReader};
use ros_madair_format::attest::HeadTrust;

fn scratch(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rm-duck-verify-{tag}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A minimal snapshot: one tiny `tiles_test.parquet` — enough for `open_layers`
/// (it needs `resource_id`/`nodegroup_id`); `seal_and_sign` then writes the
/// manifest + attestations over it.
fn minimal_snapshot(tag: &str) -> PathBuf {
    let dir = scratch(tag);
    let con = duckdb::Connection::open_in_memory().unwrap();
    con.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('r1','ng1','t1',NULL,0,'{{}}','Alpha',NULL,NULL,NULL,NULL,NULL,NULL)\
         ) t(resource_id, nodegroup_id, tileid, parenttile_id, sortorder, data, \
            descriptor_name, concept_ids, q_ordered, geo_min_lng, geo_max_lng, \
            geo_min_lat, geo_max_lat)) TO '{}' (FORMAT PARQUET)",
        dir.join("tiles_test.parquet").display()
    ))
    .unwrap();
    dir
}

fn key_path(dir: &Path) -> PathBuf {
    dir.join("signing.key")
}

#[test]
fn seal_sign_then_verify_roundtrips_and_open_accepts() {
    let dir = minimal_snapshot("ok");
    let id = ros_madair_emit::seal_and_sign(&dir, &key_path(&dir), None).expect("sign");
    assert!(!id.is_empty(), "signing yields a snapshot_id");

    // Verify recomputes the SAME id from the files and confirms the signature —
    // proving no drift between emit's signing and format/duck's verify.
    match verify_snapshot(&dir).expect("verify reads the manifest") {
        HeadTrust::Verified { .. } => {}
        other => panic!("expected Verified, got {other:?}"),
    }
    // A verified layer opens.
    DuckReader::open_layers(&[dir.as_path()]).expect("open a verified layer");
}

#[test]
fn a_tampered_layer_is_failed_and_open_refuses_it() {
    let dir = minimal_snapshot("tamper");
    ros_madair_emit::seal_and_sign(&dir, &key_path(&dir), None).expect("sign");

    // Corrupt a signed artifact: append bytes to the tiles parquet. Its hash
    // moves, so the recomputed snapshot_id no longer matches the signed one.
    let tiles = dir.join("tiles_test.parquet");
    let mut bytes = fs::read(&tiles).unwrap();
    bytes.extend_from_slice(b"tampered");
    fs::write(&tiles, &bytes).unwrap();

    match verify_snapshot(&dir).expect("verify reads the manifest") {
        HeadTrust::Failed { .. } => {}
        other => panic!("expected Failed after tamper, got {other:?}"),
    }
    // open_layers runs the verify gate first, so it refuses the tampered layer
    // before ever reading the (now corrupt) parquet.
    assert!(
        DuckReader::open_layers(&[dir.as_path()]).is_err(),
        "open_layers must refuse a tampered layer"
    );
}

/// A snapshot with no manifest/attestations (a local or unsigned build) is not a
/// signed snapshot — `open_layers` must still open it (only a proven tamper is
/// fatal), matching the behaviour the other integration tests rely on.
#[test]
fn an_unsigned_local_snapshot_still_opens() {
    let dir = minimal_snapshot("unsigned");
    // No seal_and_sign: no manifest.json, no attestations.json.
    DuckReader::open_layers(&[dir.as_path()]).expect("an unsigned local layer opens");
}
