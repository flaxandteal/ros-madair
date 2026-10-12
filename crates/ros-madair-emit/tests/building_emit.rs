// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Path-A proof: the translated genFull building generator emits correctly.**
//!
//! `cargo test -p ros-madair-emit --test building_emit -- --ignored --nocapture`
//!
//! Runs example/aonach_mor/build/emit_buildings.py (the genFull→emit translation,
//! real footprints + skeleton zone-sampling) to produce a resource-JSON data_dir,
//! then runs the REAL ros-madair-emit over it and asserts the melt substrate is
//! populated — crucially that concepts melt NON-ZERO (the concepts:0 trap: a
//! concept value that does not resolve melts to nothing).

use std::path::PathBuf;
use std::process::Command;

use duckdb::Connection;
use ros_madair_emit::ClusterConfig;
use ros_madair_handlers::default_registry;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rm-bldemit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
#[ignore = "invokes python + emit; run explicitly with --ignored"]
fn translated_building_generator_emits_the_melt_substrate() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf();
    let gen = repo.join("example/aonach_mor/build/emit_buildings.py");
    let dd = scratch("dd");

    let out = Command::new("python3")
        .arg(&gen)
        .args(["--count", "3000", "--out"])
        .arg(&dd)
        .output()
        .expect("run emit_buildings.py");
    assert!(out.status.success(), "generator failed: {}", String::from_utf8_lossy(&out.stderr));
    eprintln!("{}", String::from_utf8_lossy(&out.stdout));

    let pq = scratch("pq");
    let cfg: std::collections::HashMap<String, ClusterConfig> = std::collections::HashMap::new();
    let t = std::time::Instant::now();
    ros_madair_emit::emit_parquet(
        dd.to_str().unwrap(), pq.to_str().unwrap(), "https://example.org/",
        &default_registry(), &cfg,
    ).expect("emit buildings");
    let secs = t.elapsed().as_secs_f64();

    // locate the single tiles_*.parquet and its siblings
    let tiles = std::fs::read_dir(&pq).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
        .find(|p| p.file_name().unwrap().to_str().unwrap().starts_with("tiles_")).expect("tiles parquet");
    let stem = tiles.file_name().unwrap().to_str().unwrap().strip_prefix("tiles_").unwrap().to_string();
    let sib = |k: &str| pq.join(format!("{k}_{stem}"));

    let conn = Connection::open_in_memory().unwrap();
    let cnt = |p: &PathBuf| -> i64 {
        if !p.exists() { return -1; }
        conn.query_row(&format!("SELECT count(*) FROM read_parquet('{}')", p.display()), [], |r| r.get(0)).unwrap()
    };
    let rgs = |p: &PathBuf| -> i64 {
        conn.query_row(&format!("SELECT sum(num_row_groups) FROM parquet_file_metadata('{}')", p.display()), [], |r| r.get(0)).unwrap_or(-1)
    };
    let geo_pop: i64 = conn.query_row(&format!(
        "SELECT count(*) FROM read_parquet('{}') WHERE geo_min_lng IS NOT NULL", tiles.display()), [], |r| r.get(0)).unwrap();
    let concepts = cnt(&sib("concepts"));
    let ordered = cnt(&sib("ordered"));
    // how many distinct concept values melted (gem/heritage/cluster/service_roles)
    let distinct_concepts: i64 = conn.query_row(&format!(
        "SELECT count(DISTINCT concept_id) FROM read_parquet('{}')", sib("concepts").display()), [], |r| r.get(0)).unwrap_or(-1);
    let hospitals: i64 = conn.query_row(&format!(
        "SELECT count(*) FROM read_parquet('{}') WHERE concept_id = 'Hospital'", sib("concepts").display()), [], |r| r.get(0)).unwrap_or(-1);

    eprintln!("=== translated building emit ({secs:.1}s) ===");
    eprintln!("tiles={} (rg {})  geo-populated={geo_pop}", cnt(&tiles), rgs(&tiles));
    eprintln!("concepts={concepts} (rg {}, {distinct_concepts} distinct values; Hospital rows={hospitals})", rgs(&sib("concepts")));
    eprintln!("ordered(heights)={ordered} (rg {})", rgs(&sib("ordered")));

    assert!(cnt(&tiles) > 0, "tiles emitted");
    assert!(geo_pop > 0, "footprints promoted geo bboxes");
    assert!(concepts > 0, "CONCEPTS MELTED NON-ZERO (the concepts:0 trap)");
    assert!(hospitals > 0, "the Hospital service-role melted (the demo's target concept)");
    assert!(ordered > 0, "heights melted into the ordered store");
    eprintln!("OK: genFull→emit translation produces the full melt substrate");
}
