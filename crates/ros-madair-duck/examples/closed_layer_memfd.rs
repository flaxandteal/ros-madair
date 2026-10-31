// SPDX-License-Identifier: AGPL-3.0-or-later
//! **Split-layers spike 2: read a decrypted closed-layer parquet from RAM, never disk.**
//!
//! The split-layers handoff (§8.4) forbids writing decrypted closed data to disk:
//! "no caches, no SQLite or plaintext temp tables, no search index". But the whole
//! RM read path goes through `read_parquet('<path>')`, which wants a filesystem
//! path. So the binary question for the `on_phone` distribution is:
//!
//!   can DuckDB read a parquet that exists ONLY as bytes in memory,
//!   without those bytes ever touching the disk?
//!
//! This spike answers it with a Linux/Android `memfd_create`: an anonymous,
//! RAM-backed, process-private file descriptor with NO name in any directory. We
//! write the "decrypted" parquet bytes into it and hand DuckDB the magic
//! `/proc/self/fd/N` path. If `read_parquet` can scan it, the `on_phone` path is
//! feasible with zero disk writes and zero new DuckDB features.
//!
//! It then proves the thing that actually matters for the product: a normal
//! on-disk **open** layer and the in-RAM **closed** layer JOIN in one query — the
//! "assistant planned over open, closed clause executed on device" shape.
//!
//! Run:  cargo run -p ros-madair-duck --example closed_layer_memfd

use std::ffi::CString;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::FromRawFd;

use duckdb::Connection;

/// Build a tiny parquet with duckdb (write to a temp path, slurp the bytes, then
/// delete the file) so we have a realistic parquet `Vec<u8>` standing in for what
/// a decrypt step would hand us. The temp file is the *fixture*, not the thing
/// under test — the test is reading those bytes back without a disk file.
fn make_parquet_bytes(conn: &Connection, copy_sql: &str) -> Vec<u8> {
    let tmp = std::env::temp_dir().join(format!("rm-spike-fixture-{}.parquet", std::process::id()));
    conn.execute_batch(&format!("COPY ({copy_sql}) TO '{}' (FORMAT PARQUET);", tmp.display()))
        .expect("write fixture parquet");
    let bytes = std::fs::read(&tmp).expect("read fixture bytes");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Put `bytes` into an anonymous RAM-backed memfd and return (held File, the
/// `/proc/self/fd/N` path DuckDB can open). The File must stay alive for the path
/// to resolve. Nothing is ever written to a named location on any filesystem.
fn memfd_with(bytes: &[u8], name: &str) -> (File, String) {
    // MFD_CLOEXEC = 1. memfd_create is Linux ≥ 3.17 and Android (bionic) ≥ API 30.
    let cname = CString::new(name).unwrap();
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), 1) };
    assert!(fd >= 0, "memfd_create failed: {}", std::io::Error::last_os_error());
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.write_all(bytes).expect("write bytes into memfd");
    f.flush().unwrap();
    // Opening /proc/self/fd/N gives a fresh file description at offset 0, so the
    // writer's offset is irrelevant — but rewind anyway to be explicit.
    f.seek(SeekFrom::Start(0)).unwrap();
    let path = format!("/proc/self/fd/{fd}");
    (f, path)
}

fn main() {
    let conn = Connection::open_in_memory().expect("duckdb");

    // --- 1. The "decrypted closed layer": keyholder names + numbers for 2 buildings.
    let closed_bytes = make_parquet_bytes(
        &conn,
        "SELECT * FROM (VALUES \
            ('bldg-1','Páidí Ó Dónaill','+353 86 1112222','keyholder'), \
            ('bldg-1','Síle Nic Gearailt','+353 87 3334444','deputy'), \
            ('bldg-2','Arun Kumar','+353 85 5556666','keyholder')) \
         t(building_id, name, phone, role)",
    );
    println!("closed layer: {} bytes of parquet, held only in RAM next", closed_bytes.len());

    // --- 2. Read it back from an anonymous RAM fd — the decisive test.
    let (_held, closed_path) = memfd_with(&closed_bytes, "closed-keyholders");
    // Prove the fd has no on-disk name: its /proc link target is "/memfd:...(deleted)".
    let link = std::fs::read_link(&closed_path).map(|p| p.display().to_string()).unwrap_or_default();
    println!("memfd path: {closed_path}  ->  {link}");
    assert!(link.contains("memfd:"), "must be an anonymous memfd, not a real file: {link}");

    conn.execute_batch(&format!(
        "CREATE VIEW closed AS SELECT * FROM read_parquet('{closed_path}');"
    ))
    .expect("DuckDB must read a parquet from the RAM fd");

    let (n, first): (i64, String) = conn
        .query_row("SELECT count(*), min(name) FROM closed", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("scan the in-RAM closed layer");
    println!("read from RAM: {n} closed rows, first name = {first:?}");
    assert_eq!(n, 3, "all 3 closed rows read from memory");

    // --- 3. The open layer, on disk as normal (schema + declassified count).
    //        "2 keyholders recorded" is published BY THE OWNER into the open layer
    //        (§8.1: the count must NOT be derived from the closed layer). Here the
    //        open layer carries building_id, name, the published count, and status.
    let open_dir = std::env::temp_dir().join(format!("rm-spike-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&open_dir);
    std::fs::create_dir_all(&open_dir).unwrap();
    let open_path = open_dir.join("open.parquet");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
            ('bldg-1','Scoil Ros Bán', 2, 'usable'), \
            ('bldg-2','Halla an Phobail', 1, 'usable')) \
          t(building_id, descriptor, keyholders_recorded, status)) \
         TO '{}' (FORMAT PARQUET); \
         CREATE VIEW open AS SELECT * FROM read_parquet('{}');",
        open_path.display(), open_path.display()
    ))
    .expect("open layer on disk");

    // --- 4. The product shape: open (disk) JOIN closed (RAM) in ONE query.
    //        This is exactly "the open part ran; the closed clause executed on the
    //        device after opening" — composed, not copied.
    let mut stmt = conn
        .prepare(
            "SELECT o.descriptor, o.keyholders_recorded, c.name, c.phone, c.role \
             FROM open o JOIN closed c ON c.building_id = o.building_id \
             ORDER BY o.building_id, c.role",
        )
        .unwrap();
    let rows: Vec<(String, i64, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    println!("\nopen (disk) ⋈ closed (RAM):");
    for (desc, cnt, name, phone, role) in &rows {
        println!("  {desc}  (owner says {cnt} recorded)  ->  {name} · {role} · {phone}");
    }
    assert_eq!(rows.len(), 3, "join across disk+RAM layers returns all keyholders");
    // The owner-published count (open) matches the real closed cardinality here —
    // but it came from the OPEN layer, never computed from `closed`.
    assert!(rows.iter().any(|r| r.0 == "Scoil Ros Bán" && r.1 == 2));

    // --- 5. Drop it. On "Close now"/expiry the File is dropped, the fd closes, the
    //        RAM is reclaimed by the kernel. No disk artifact ever existed to wipe.
    drop(_held);
    let _ = std::fs::remove_dir_all(&open_dir);

    println!(
        "\nPASS: DuckDB read a parquet from an anonymous RAM fd (zero disk writes) and \n\
         joined it against an on-disk open layer. The on_phone closed-layer path is feasible \n\
         with no new DuckDB feature — just memfd + the existing read_parquet view model."
    );
}
