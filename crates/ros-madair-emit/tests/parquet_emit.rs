// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Slice 1 verification: the additive tile-row Parquet writer produces a
//! well-formed file with the right promoted columns and real row-group chunking.**
//!
//! Builds the same synthetic Talk corpus as the read-side differential (geometry
//! + date on four resources), emits it through `emit_parquet`, and reads the
//! Parquet back to assert: one row per tile, `q_ordered` = the head's date
//! quantization, `geo_*` = the geometry bbox, and — with a small row-group size —
//! more than one row group (the "chunk"). Demo fixture outside the repo;
//! `ROS_MADAIR_DEMO_DATA` relocates it, absent → skip.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alizarin_core::quantize::quantize_date;
use arrow::array::{Array, Float64Array, Int64Array, StringArray};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::{FileReader, SerializedFileReader};
use ros_madair_emit::{default_registry, emit_parquet, ClusterConfig, ClusterDim};
use serde_json::json;

const DEMO_DATA: &str = "/home/philtweir/Cód/Oscailte/magic/Clódóir/data";
const TALK_GRAPH: &str = "a6c412db-72e0-4099-a690-ccc75ba841a9";
const TALK_ROOT: &str = "5a037559-1ae0-11f0-b22a-8fd6f4eb1a02";
const GEO_NG: &str = "5efd0000-0000-4000-8000-000000000002";
const FOUNDED_NG: &str = "5efd0000-0000-4000-8000-000000000001";

const R_POINT: &str = "11110000-0000-4000-8000-000000000001";
const R_LSHAPE: &str = "22220000-0000-4000-8000-000000000001";
const R_FAR: &str = "33330000-0000-4000-8000-000000000001";
const R_DIAG: &str = "44440000-0000-4000-8000-000000000001";

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
    let dir = std::env::temp_dir().join(format!("rm-pq-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn add_geo_node(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": GEO_NG, "nodegroup_id": GEO_NG, "name": "Location", "alias": "location",
        "datatype": "geojson-feature-collection", "graph_id": TALK_GRAPH,
        "istopnode": false, "is_collector": true, "isrequired": false,
        "issearchable": true, "exportable": false, "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": GEO_NG, "cardinality": "1", "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": "5efd0000-0000-4000-8000-0000000000ef",
        "domainnode_id": TALK_ROOT, "rangenode_id": GEO_NG, "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn add_date_node(graph_path: &Path) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(graph_path).unwrap()).unwrap();
    let g = &mut doc["graph"][0];
    g["nodes"].as_array_mut().unwrap().push(json!({
        "nodeid": FOUNDED_NG, "nodegroup_id": FOUNDED_NG, "name": "Founded", "alias": "founded",
        "datatype": "date", "graph_id": TALK_GRAPH,
        "istopnode": false, "is_collector": true, "isrequired": false,
        "issearchable": true, "exportable": false, "sortorder": 0,
    }));
    g["nodegroups"].as_array_mut().unwrap().push(json!({
        "nodegroupid": FOUNDED_NG, "cardinality": "1", "parentnodegroup_id": null,
    }));
    g["edges"].as_array_mut().unwrap().push(json!({
        "edgeid": "5efd0000-0000-4000-8000-0000000000ee",
        "domainnode_id": TALK_ROOT, "rangenode_id": FOUNDED_NG, "graph_id": TALK_GRAPH,
    }));
    std::fs::write(graph_path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

fn talk(id: &str, geo_tile: &str, date_tile: &str, geometry: serde_json::Value, founded: &str) -> serde_json::Value {
    let fc = json!({
        "type": "FeatureCollection",
        "features": [{ "type": "Feature", "properties": {}, "geometry": geometry }],
    });
    json!({
        "resourceinstance": {
            "resourceinstanceid": id, "graph_id": TALK_GRAPH, "name": id, "legacyid": null,
            "descriptors": { "en": { "name": id, "description": "", "map_popup": "" } },
        },
        "tiles": [
            { "tileid": geo_tile, "nodegroup_id": GEO_NG, "parenttile_id": null,
              "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
              "data": { GEO_NG: fc } },
            { "tileid": date_tile, "nodegroup_id": FOUNDED_NG, "parenttile_id": null,
              "resourceinstance_id": id, "sortorder": 0, "provisionaledits": null,
              "data": { FOUNDED_NG: founded } },
        ],
    })
}

fn corpus() -> Option<PathBuf> {
    let demo = demo_data()?;
    let dir = scratch("corpus");
    copy_dir(&demo.join("graphs"), &dir.join("graphs"));
    copy_dir(&demo.join("vocabularies"), &dir.join("vocabularies"));
    let gp = dir.join("graphs").join(format!("{TALK_GRAPH}.json"));
    add_geo_node(&gp);
    add_date_node(&gp);
    let talk_dir = dir.join("resources").join("talk");
    std::fs::create_dir_all(&talk_dir).unwrap();

    let resources = vec![
        talk(R_POINT, "a0aa0000-0000-4000-8000-000000000010", "b0bb0000-0000-4000-8000-000000000010",
             json!({ "type": "Point", "coordinates": [0.0, 0.0] }), "2005-06-01"),
        talk(R_LSHAPE, "a1aa0000-0000-4000-8000-000000000010", "b1bb0000-0000-4000-8000-000000000010",
             json!({ "type": "Polygon", "coordinates": [[[0.5,0.5],[10.0,0.5],[10.0,2.0],[2.0,2.0],[2.0,10.0],[0.5,10.0],[0.5,0.5]]] }), "2018-03-15"),
        talk(R_FAR, "a2aa0000-0000-4000-8000-000000000010", "b2bb0000-0000-4000-8000-000000000010",
             json!({ "type": "Point", "coordinates": [100.0, 100.0] }), "1850-01-01"),
        talk(R_DIAG, "a3aa0000-0000-4000-8000-000000000010", "b3bb0000-0000-4000-8000-000000000010",
             json!({ "type": "LineString", "coordinates": [[0.5,5.0],[5.0,0.5]] }), "2021-07-01"),
    ];
    std::fs::write(
        talk_dir.join("talks.json"),
        serde_json::to_vec_pretty(&json!({ "business_data": { "resources": resources } })).unwrap(),
    )
    .unwrap();
    Some(dir)
}

/// Read every batch of a Parquet file and return (column name -> column) for the
/// first batch, plus the total row count.
fn read_all(path: &Path) -> (Vec<arrow::record_batch::RecordBatch>, usize) {
    let file = std::fs::File::open(path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let reader = builder.build().unwrap();
    let batches: Vec<_> = reader.map(|b| b.unwrap()).collect();
    let rows = batches.iter().map(|b| b.num_rows()).sum();
    (batches, rows)
}

fn col<'a>(batch: &'a arrow::record_batch::RecordBatch, name: &str) -> &'a dyn Array {
    let idx = batch.schema().index_of(name).unwrap();
    batch.column(idx).as_ref()
}

#[test]
fn tile_row_parquet_carries_promoted_columns_and_chunks() {
    let Some(dir) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let out = scratch("out");
    // A tiny row-group size so 8 tiles span multiple row groups (the "chunks").
    let mut cfg_by_graph = HashMap::new();
    cfg_by_graph.insert(
        TALK_GRAPH.to_string(),
        ClusterConfig { dimensions: vec![ClusterDim::Geo, ClusterDim::Descriptor], row_group_size: 2, partition_by_nodegroup: false, nodegroup_hierarchical_order: false },
    );

    let registry = default_registry();
    let summaries = emit_parquet(dir.to_str().unwrap(), out.to_str().unwrap(), "https://example.org/", &registry, &cfg_by_graph)
        .expect("emit_parquet");

    let talk = summaries
        .iter()
        .find(|s| s.graph_id == TALK_GRAPH)
        .expect("Talk model emitted");
    assert_eq!(talk.resources, 4, "four talks");
    assert_eq!(talk.tiles, 8, "two tiles each -> eight rows");
    // DuckDB flushes row groups in vector (2048-row) multiples, so 8 tiles land
    // in a single row group regardless of ROW_GROUP_SIZE — the "chunk" split only
    // manifests at scale. Assert at least one, and verify the zone-map stats below.
    assert!(talk.row_groups >= 1, "at least one row group, got {}", talk.row_groups);

    let path = PathBuf::from(&talk.path);
    assert!(path.exists(), "tiles parquet exists at {}", talk.path);

    let (batches, rows) = read_all(&path);
    assert_eq!(rows, 8, "row per tile");

    // Gather promoted values across all rows, keyed by (resource, nodegroup).
    let mut q_by_res: HashMap<String, i64> = HashMap::new();
    let mut geo_present: HashMap<String, bool> = HashMap::new();
    for b in &batches {
        let rid = col(b, "resource_id").as_any().downcast_ref::<StringArray>().unwrap();
        let ng = col(b, "nodegroup_id").as_any().downcast_ref::<StringArray>().unwrap();
        let q = col(b, "q_ordered").as_any().downcast_ref::<Int64Array>().unwrap();
        let gmnx = col(b, "geo_min_lng").as_any().downcast_ref::<Float64Array>().unwrap();
        for i in 0..b.num_rows() {
            let res = rid.value(i).to_string();
            if ng.value(i) == FOUNDED_NG && q.is_valid(i) {
                q_by_res.insert(res.clone(), q.value(i));
            }
            if ng.value(i) == GEO_NG {
                geo_present.insert(res.clone(), gmnx.is_valid(i));
            }
        }
    }

    // The date tile's q_ordered is exactly the head's day-quantization.
    assert_eq!(q_by_res.get(R_POINT).copied(), quantize_date("2005-06-01"));
    assert_eq!(q_by_res.get(R_LSHAPE).copied(), quantize_date("2018-03-15"));
    assert_eq!(q_by_res.get(R_FAR).copied(), quantize_date("1850-01-01"));
    assert_eq!(q_by_res.get(R_DIAG).copied(), quantize_date("2021-07-01"));

    // Every geometry tile promoted a bbox.
    for r in [R_POINT, R_LSHAPE, R_FAR, R_DIAG] {
        assert_eq!(geo_present.get(r).copied(), Some(true), "geo bbox promoted for {r}");
    }

    // The footer really has row-group stats on the promoted columns (the zone-map).
    let file = std::fs::File::open(&path).unwrap();
    let meta = SerializedFileReader::new(file).unwrap().metadata().clone();
    assert!(meta.num_row_groups() >= 1);
    // q_ordered column has min/max statistics in at least one row group.
    let has_qstats = (0..meta.num_row_groups()).any(|g| {
        let rg = meta.row_group(g);
        (0..rg.num_columns()).any(|c| {
            let cc = rg.column(c);
            cc.column_path().string() == "q_ordered" && cc.statistics().is_some()
        })
    });
    assert!(has_qstats, "the promoted q_ordered column must carry row-group zone-map stats");

    // Optional: drop the emitted file where a DuckDB smoke-check can read it.
    if let Ok(dst) = std::env::var("RM_PARQUET_OUT") {
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::copy(&path, Path::new(&dst).join("tiles_talk.parquet")).unwrap();
        eprintln!("copied tiles_talk.parquet to {dst}");
    }

    eprintln!(
        "OK: {} tiles, {} row groups, promoted columns + footer stats verified",
        talk.tiles, talk.row_groups
    );
}

/// Partition-by-nodegroup: a "give me nodegroups X,Y,Z" query can read ONLY those
/// partition files. Here the Talk corpus has two indexable nodegroups (geometry,
/// date); each becomes its own Parquet under a Hive-partitioned directory, and a
/// consumer wanting just the date nodegroup never opens the geometry file.
#[test]
fn nodegroup_partitioning_isolates_each_nodegroup_into_its_own_file() {
    let Some(dir) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let out = scratch("out-part");
    let mut cfg_by_graph = HashMap::new();
    cfg_by_graph.insert(
        TALK_GRAPH.to_string(),
        ClusterConfig {
            dimensions: vec![ClusterDim::Geo, ClusterDim::Descriptor],
            row_group_size: 8192,
            partition_by_nodegroup: true,
            nodegroup_hierarchical_order: false,
        },
    );

    let registry = default_registry();
    let summaries = emit_parquet(dir.to_str().unwrap(), out.to_str().unwrap(), "https://example.org/", &registry, &cfg_by_graph)
        .expect("emit_parquet");
    let talk = summaries.iter().find(|s| s.graph_id == TALK_GRAPH).unwrap();

    assert_eq!(talk.tiles, 8, "still 8 tiles total");
    assert_eq!(talk.partitions, 2, "two indexable nodegroups -> two partitions");

    // DuckDB's PARTITION_BY writes `nodegroup_id=<v>/data_0.parquet` (name is the
    // engine's, not ours) — resolve the single parquet inside each partition dir.
    let part_file = |ng: &str| -> PathBuf {
        let dir = PathBuf::from(&talk.path).join(format!("nodegroup_id={ng}"));
        std::fs::read_dir(&dir)
            .unwrap_or_else(|_| panic!("partition dir {} exists", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().map(|x| x == "parquet").unwrap_or(false))
            .unwrap_or_else(|| panic!("a parquet in {}", dir.display()))
    };
    let geo_part = part_file(GEO_NG);
    let date_part = part_file(FOUNDED_NG);
    assert!(geo_part.exists(), "geometry partition file exists");
    assert!(date_part.exists(), "date partition file exists");

    // Each partition holds exactly its nodegroup's 4 tiles (one per resource) — a
    // consumer reads one file and touches none of the other nodegroup's bytes.
    let (gb, grows) = read_all(&geo_part);
    let (db, drows) = read_all(&date_part);
    assert_eq!(grows, 4, "geometry partition = one tile per resource");
    assert_eq!(drows, 4, "date partition = one tile per resource");
    for b in &gb {
        let ng = col(b, "nodegroup_id").as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..b.num_rows() {
            assert_eq!(ng.value(i), GEO_NG, "geometry partition holds only geo tiles");
        }
    }
    for b in &db {
        let ng = col(b, "nodegroup_id").as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..b.num_rows() {
            assert_eq!(ng.value(i), FOUNDED_NG, "date partition holds only date tiles");
        }
    }

    if let Ok(dst) = std::env::var("RM_PARQUET_PART_OUT") {
        let _ = std::fs::remove_dir_all(&dst);
        copy_dir(&PathBuf::from(&talk.path), Path::new(&dst));
        eprintln!("copied partition tree to {dst}");
    }
    eprintln!("OK: {} partitions, each isolated to its nodegroup", talk.partitions);
}

/// Slice 2: `emit_parquet` writes a real, self-describing manifest (a non-empty
/// `snapshot_id` derived over the content-file hashes, the models, and the
/// format version), and `sign_head` signs THAT id into an `attestations.json`
/// that verifies — while a different snapshot (what a reader computes after a
/// tampered chunk) does not. This is the on-device signing path end to end,
/// minus the read-side recompute (slice 3).
#[test]
fn emit_writes_a_signable_manifest_that_verifies() {
    let Some(dir) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let out = scratch("signout");
    let registry = default_registry();
    let cfg_by_graph = HashMap::new();
    emit_parquet(
        dir.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
        &registry,
        &cfg_by_graph,
    )
    .expect("emit_parquet");

    // 1. A real self-describing manifest landed (not the stubbed JS one).
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).expect("manifest.json"))
            .unwrap();
    let snapshot_id = manifest["snapshot_id"].as_str().expect("snapshot_id present");
    assert!(!snapshot_id.is_empty(), "snapshot_id is non-empty");
    assert_eq!(manifest["format_version"].as_u64(), Some(1), "format_version stamped");
    assert_eq!(manifest["base_uri"].as_str(), Some("https://example.org/"));
    assert!(
        manifest["artifacts"].as_array().is_some_and(|a| !a.is_empty()),
        "artifacts are hashed (they feed the snapshot_id)"
    );
    assert!(
        manifest["models"].as_array().is_some_and(|a| !a.is_empty()),
        "models are populated"
    );

    // 2. sign_head signs the manifest's OWN id, and the bundle verifies.
    let key = out.join("signing_ed25519.key");
    let signed = ros_madair_emit::sign_head(&out, &key).expect("sign_head");
    assert_eq!(signed, snapshot_id, "sign_head signs the manifest's snapshot_id");
    let bundle: ros_madair_emit::AttestationBundle = serde_json::from_slice(
        &std::fs::read(out.join("attestations.json")).expect("attestations.json"),
    )
    .unwrap();
    assert!(
        ros_madair_emit::verify_bundle(&bundle, snapshot_id).is_trusted(),
        "the signed snapshot verifies"
    );

    // 3. Tamper-evidence: a different snapshot_id (a swapped chunk moves it) fails.
    assert!(
        !ros_madair_emit::verify_bundle(&bundle, "0000000000000000").is_trusted(),
        "a different snapshot is untrusted"
    );
    eprintln!("OK: manifest signed and verified for snapshot {snapshot_id}");
}

/// Slice 3b: the read-side `verify_head` gate. Unsigned → untrusted; signed +
/// unmodified → trusted; a byte appended to a LISTED artifact → the recomputed
/// snapshot_id moves off the signed subject → untrusted. This is the "tamper a
/// chunk turns the badge red" mechanic, proven without a reader.
#[test]
fn verify_head_trusts_signed_and_flags_tamper() {
    let Some(dir) = corpus() else {
        eprintln!("demo fixture absent — skipping");
        return;
    };
    let out = scratch("verifyout");
    let registry = default_registry();
    let cfg_by_graph = HashMap::new();
    emit_parquet(
        dir.to_str().unwrap(),
        out.to_str().unwrap(),
        "https://example.org/",
        &registry,
        &cfg_by_graph,
    )
    .expect("emit_parquet");

    // Unsigned: manifest is self-consistent but nothing vouches for it.
    assert!(
        !ros_madair_emit::verify_head(&out).expect("verify_head").is_trusted(),
        "an unsigned head is untrusted"
    );

    // Sign, then it verifies.
    let key = out.join("signing_ed25519.key");
    ros_madair_emit::sign_head(&out, &key).expect("sign_head");
    assert!(
        ros_madair_emit::verify_head(&out).expect("verify_head").is_trusted(),
        "a signed, unmodified head verifies"
    );

    // Tamper a listed parquet artifact (robust to partitioning: take the path
    // from the manifest, not a dir glob).
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    let victim_rel = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|a| a["path"].as_str().filter(|p| p.ends_with(".parquet")))
        .expect("a parquet artifact is listed");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(out.join(victim_rel))
            .unwrap();
        f.write_all(b"\x00").unwrap();
    }

    match ros_madair_emit::verify_head(&out).expect("verify_head") {
        ros_madair_emit::Verdict::Untrusted { reason } => {
            assert!(reason.contains("altered"), "tamper reason names alteration: {reason}");
            eprintln!("OK: tamper detected -> {reason}");
        }
        v => panic!("expected Untrusted after tamper, got {v:?}"),
    }
}
