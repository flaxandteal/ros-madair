// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unit-1 keystone for the offline-slice USP: serve the WHOLE corpus from R2 and
//! excise a bbox-seeded + 1-hop slice that re-queries IDENTICALLY offline.
//!
//! `#[ignore]` — it range-reads the live 763k corpus on R2 over the network, so it
//! is not part of the default `cargo test`. Run it explicitly:
//!
//! ```bash
//! cargo test -p ros-madair-duck --test offline_slice -- --ignored --nocapture
//! ```
//!
//! What it proves (the three risks unit 1 had to retire):
//!   1. `open_http` loads httpfs and RANGE-READS the R2 snapshot natively (manifest
//!      → explicit partition URLs → views), incl. a CROSS-MODEL edge hop.
//!   2. `excise` COPYs the seed+hop closure to a local slice.
//!   3. Re-opening the slice OFFLINE answers the same hop IDENTICALLY, and the slice
//!      is a tiny fraction of the full corpus.

use ros_madair_duck::DuckReader;

const R2_BASE: &str = "https://r2.f-a-t.link/aonach763k";

#[test]
#[ignore = "network: range-reads the live 763k corpus on R2"]
fn excise_slice_requeries_identically() {
    // --- 1. open the whole corpus from R2 (httpfs range reads) ---
    let remote = DuckReader::open_http(R2_BASE).expect("open_http R2");
    let full_tiles = remote.count_tiles().expect("count full tiles");
    eprintln!("full corpus: {full_tiles} tiles");
    assert!(
        full_tiles > 1_000_000,
        "expected the 763k-building corpus (>1M tiles), got {full_tiles}"
    );

    // --- 2. pick a small AO around a real point, seed the buildings in it ---
    let (lng, lat) = remote
        .sample_point()
        .expect("sample_point query")
        .expect("corpus has at least one geo tile");
    eprintln!("AO centred on ({lng:.5}, {lat:.5})");
    let d = 0.004; // ~400 m box
    let bbox = [lng - d, lat - d, lng + d, lat + d];
    let seed = remote.ids_in_bbox(bbox, 300).expect("seed query");
    eprintln!("seed: {} buildings in the AO", seed.len());
    assert!(!seed.is_empty(), "AO seed must be non-empty");

    // --- 3. the first hop (cross-model): who references these buildings ---
    let hop = remote.neighbours(&seed).expect("hop query");
    eprintln!("hop: {} related resources (keyholders/orgs/…)", hop.len());

    let mut closure = seed.clone();
    closure.extend(hop.iter().cloned());
    closure.sort();
    closure.dedup();

    // --- 4. excise the closure to a local slice (geo-pruned on the seed) ---
    let tmp = std::env::temp_dir().join(format!("ros-slice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let written = remote
        .excise(&closure, &[], Some(bbox), &tmp, "slice")
        .expect("excise");
    eprintln!("wrote {} slice files to {}", written.len(), tmp.display());
    assert!(
        written.iter().any(|p| p.ends_with("tiles_slice.parquet")),
        "slice must include tiles"
    );
    let slice_bytes: u64 = written
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    eprintln!("slice total: {} KiB", slice_bytes / 1024);

    // --- 5. re-open the slice OFFLINE (local files only) and re-run the hop ---
    let tiles_glob = tmp.join("tiles_slice.parquet");
    let local = DuckReader::open(tiles_glob.to_str().unwrap()).expect("open local slice");

    let slice_tiles = local.count_tiles().expect("count slice tiles");
    eprintln!("slice: {slice_tiles} tiles (vs {full_tiles} full)");
    assert!(slice_tiles > 0, "slice has tiles");
    assert!(
        slice_tiles < full_tiles / 100,
        "slice ({slice_tiles}) should be <1% of full ({full_tiles})"
    );

    // The hop answered against the OFFLINE slice must equal the hop answered online.
    let mut hop_online = hop.clone();
    hop_online.sort();
    let mut hop_offline = local.neighbours(&seed).expect("offline hop");
    hop_offline.sort();
    assert_eq!(
        hop_offline, hop_online,
        "offline slice must answer the first hop identically to the live corpus"
    );
    eprintln!("OK: offline hop == online hop ({} resources)", hop_online.len());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// (B) The FREE 2-hop, proven end-to-end offline on the cache-ON heads: excise a
/// keyholder + its links from composed local layers, re-open the slice, and confirm the
/// keyholder's `__cache` (naming its org — the 2-hop) survived into the slice. No edge
/// expansion, no R2. Set `ROS_TEST_HEADS_DIR` to a dir holding `building_head/` +
/// `contact_head/` (cache-ON heads); skips if unset/absent.
#[test]
#[ignore = "local fixture: needs ROS_TEST_HEADS_DIR with cache-ON heads"]
fn free_second_hop_via_cache() {
    use std::path::Path;
    let base_dir = std::env::var("ROS_TEST_HEADS_DIR")
        .unwrap_or_else(|_| "/tmp/claude-1000/func-heads".to_string());
    let building = Path::new(&base_dir).join("building_head");
    let contact = Path::new(&base_dir).join("contact_head");
    if !contact.join("manifest.json").exists() || !building.join("manifest.json").exists() {
        eprintln!("no cache-ON heads at {base_dir} — skip");
        return;
    }
    let composed =
        DuckReader::open_layers(&[building.as_path(), contact.as_path()]).expect("open_layers");

    // Find a keyholder (a contact_head resource) with neighbours, and its 2-hop org.
    let contact_only = DuckReader::open_layers(&[contact.as_path()]).expect("open contact");
    let kh = contact_only
        .entry_resource_ids()
        .unwrap()
        .into_iter()
        .find(|id| !composed.neighbours(std::slice::from_ref(id)).unwrap_or_default().is_empty())
        .expect("a keyholder with at least one link");
    let kh_nbrs = composed.neighbours(std::slice::from_ref(&kh)).unwrap();
    eprintln!("keyholder {kh} → {} neighbours", kh_nbrs.len());

    // Excise {keyholder} ∪ its links at FULL grade (no 2-hop edge expansion, cap=0 world).
    let mut closure = vec![kh.clone()];
    closure.extend(kh_nbrs.iter().cloned());
    let tmp = std::env::temp_dir().join(format!("ros-free2hop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    composed.excise(&closure, &[], None, &tmp, "slice").expect("excise");

    // Re-open the slice OFFLINE and read the keyholder's tile data.
    let slice = DuckReader::open(tmp.join("tiles_slice.parquet").to_str().unwrap()).expect("open slice");
    let blobs = slice.resource_data_blobs(&kh).unwrap();
    let data = blobs.join("\n");
    eprintln!("keyholder slice data ({}B): {}", data.len(), data.chars().take(400).collect::<String>());

    // The free 2-hop: the keyholder's __cache rode into the slice AND names a linked
    // (2-hop) resource — so the card can show "→ Org X" with no extra fetch.
    assert!(data.contains("__cache"), "keyholder __cache must survive into the slice");
    let names_a_neighbour = kh_nbrs.iter().any(|n| data.contains(n.as_str()));
    assert!(
        names_a_neighbour,
        "the keyholder's __cache must name a 2-hop resource (free second hop)"
    );
    eprintln!("OK: free 2-hop present in slice (cache names a linked resource)");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// (A) The free 2-hop over R2: compose the cache-ON per-layer heads published at
/// `aonach-layers/` (building_head + contact_head), confirm a keyholder's `__cache`
/// (its org — the 2-hop) is intact read-over-R2, and that an excised slice carries it
/// offline. This is the fully-remote version of the free 2-hop — no edge expansion.
#[test]
#[ignore = "network: composes the cache-ON layer heads on R2"]
fn free_second_hop_over_r2_layers() {
    const BUILDING: &str = "https://r2.f-a-t.link/aonach-layers/building_head";
    const CONTACT: &str = "https://r2.f-a-t.link/aonach-layers/contact_head";

    let composed = DuckReader::open_http_layers(&[BUILDING, CONTACT]).expect("open_http_layers");
    // A keyholder = a contact-layer resource with at least one link.
    let contact = DuckReader::open_http(CONTACT).expect("open contact");
    let kh = contact
        .entry_resource_ids()
        .unwrap()
        .into_iter()
        .find(|id| !composed.neighbours(std::slice::from_ref(id)).unwrap_or_default().is_empty())
        .expect("a keyholder with a link");
    let nbrs = composed.neighbours(std::slice::from_ref(&kh)).unwrap();
    eprintln!("keyholder {kh} → {} neighbours (over R2)", nbrs.len());

    // (1) LIVE over R2: the keyholder's cache is intact and names a 2-hop resource.
    let live = composed.resource_data_blobs(&kh).unwrap().join("\n");
    assert!(live.contains("__cache"), "keyholder __cache intact read-over-R2");
    assert!(
        nbrs.iter().any(|n| live.contains(n.as_str())),
        "cache names a 2-hop resource (live over R2)"
    );

    // (2) OFFLINE slice carved over R2 still carries it (cap=0 world: no edge expansion).
    let mut closure = vec![kh.clone()];
    closure.extend(nbrs.iter().cloned());
    let tmp = std::env::temp_dir().join(format!("ros-r2free2hop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    composed.excise(&closure, &[], None, &tmp, "slice").expect("excise");
    let slice = DuckReader::open(tmp.join("tiles_slice.parquet").to_str().unwrap()).expect("open slice");
    let off = slice.resource_data_blobs(&kh).unwrap().join("\n");
    assert!(off.contains("__cache"), "keyholder __cache survived into the offline slice");
    assert!(
        nbrs.iter().any(|n| off.contains(n.as_str())),
        "offline slice cache still names the 2-hop (free second hop, over R2)"
    );
    eprintln!("OK: free 2-hop over R2 — live and in the offline slice");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// (A, option A) The LOCAL-layers export — the on-device path, NO httpfs: excise a
/// region + 1-hop from already-provisioned local head dirs, re-open the layered slice,
/// and confirm the free 2-hop (`__cache`) rode in. Proves `export_offline_layers` over
/// `LayerSpec::Local`. Set `ROS_TEST_HEADS_DIR` (dir with building_head/ + contact_head/);
/// skips if absent.
#[test]
#[ignore = "local fixture: needs ROS_TEST_HEADS_DIR with cache-ON heads"]
fn export_offline_local_layers() {
    use ros_madair_duck::{export_offline_layers, LayerSpec};
    use std::path::{Path, PathBuf};

    let base_dir =
        std::env::var("ROS_TEST_HEADS_DIR").unwrap_or_else(|_| "/tmp/claude-1000/func-heads".into());
    let building = Path::new(&base_dir).join("building_head");
    let contact = Path::new(&base_dir).join("contact_head");
    if !contact.join("manifest.json").exists() || !building.join("manifest.json").exists() {
        eprintln!("no cache-ON heads at {base_dir} — skip");
        return;
    }
    // A wide box over the (small) local corpus so the seed catches keyholded buildings.
    let composed = DuckReader::open_layers(&[building.as_path()]).unwrap();
    let (lng, lat) = composed.sample_point().unwrap().unwrap();
    let bbox = [lng - 0.1, lat - 0.1, lng + 0.1, lat + 0.1];

    let specs = vec![
        LayerSpec::Local(PathBuf::from(&building)),
        LayerSpec::Local(PathBuf::from(&contact)),
    ];
    let tmp = std::env::temp_dir().join(format!("ros-local-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let s = export_offline_layers(&specs, bbox, 5000, 0, &tmp).expect("export_offline_layers local");
    eprintln!("LOCAL export: seed={} hop1={} refs={}", s.seed, s.hop1, s.refs);
    assert_eq!(s.layer_dirs.len(), 2, "two local layers → two slice dirs");
    assert!(s.seed > 0, "seed non-empty");
    assert!(s.hop1 > 0, "1-hop keyholders present (no httpfs used)");

    // Re-open the LAYERED slice OFFLINE and confirm a keyholder's cache (the free 2-hop).
    let paths: Vec<&Path> = s.layer_dirs.iter().map(|p| p.as_path()).collect();
    let slice = DuckReader::open_layers(&paths).expect("open_layers slice");
    // a keyholder present in the slice's contact layer, with its __cache
    let contact_slice = DuckReader::open_layers(&[s.layer_dirs[1].as_path()]).unwrap();
    let kh = contact_slice
        .entry_resource_ids()
        .unwrap()
        .into_iter()
        .find(|id| {
            let d = slice.resource_data_blobs(id).unwrap_or_default().join("\n");
            d.contains("__cache")
        })
        .expect("a keyholder tile with __cache in the slice");
    let data = slice.resource_data_blobs(&kh).unwrap().join("\n");
    assert!(data.contains("__cache"), "free 2-hop cache survived into the local slice");
    eprintln!("OK: local-layers export — free 2-hop in the offline slice, no httpfs");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// (a) Does the alizarin `__cache`/`render_summary` on a 1-hop keyholder tile reach
/// the SECOND level (the keyholder's own linked resources — the org)? If so, the 2-hop
/// DISPLAY rides along free in the full-grade 1-hop tiles we already excise, and no
/// edge expansion (the thing that fanned out to ~69k) is needed for "references to 2-hop".
#[test]
#[ignore = "network: range-reads the live 763k corpus on R2"]
fn cache_gives_second_hop_free() {
    use std::collections::HashSet;

    let r = DuckReader::open_http(R2_BASE).expect("open_http");
    let (lng, lat) = r.sample_point().unwrap().unwrap();
    let d = 0.004;
    let bbox = [lng - d, lat - d, lng + d, lat + d];
    let seed = r.ids_in_bbox(bbox, 300).unwrap();
    let seedset: HashSet<&String> = seed.iter().collect();
    let hop1 = r.neighbours(&seed).unwrap();
    eprintln!("1-hop resources: {}", hop1.len());

    let mut any_second_hop_in_cache = false;
    for kh in &hop1 {
        // 2-hop candidates = this 1-hop's neighbours that are NOT seed buildings.
        let two: Vec<String> = r
            .neighbours(std::slice::from_ref(kh))
            .unwrap()
            .into_iter()
            .filter(|n| !seedset.contains(n))
            .take(25)
            .collect();
        let rs = r.render_summary_of(kh).unwrap();
        let blobs = r.resource_data_blobs(kh).unwrap();
        let hay = format!("{}\n{}", rs.clone().unwrap_or_default(), blobs.join("\n"));
        eprintln!("--- 1-hop {kh}  ({} 2-hop candidates)", two.len());
        eprintln!(
            "    render_summary: {}",
            rs.as_deref().unwrap_or("<none>").chars().take(300).collect::<String>()
        );
        if let Some(b) = blobs.first() {
            eprintln!("    data[0] ({}B): {}", b.len(), b.chars().take(500).collect::<String>());
        }
        for t in &two {
            let by_id = hay.contains(t.as_str());
            // Also check by the 2-hop's descriptor (the cache embeds names, not just ids).
            let desc = r.render_summary_of(t).ok().flatten();
            let by_name = desc
                .as_deref()
                .map(|s| !s.is_empty() && hay.contains(s))
                .unwrap_or(false);
            eprintln!("    2-hop {t}: in_cache_by_id={by_id} by_name={by_name}");
            if by_id || by_name {
                any_second_hop_in_cache = true;
            }
        }
    }
    eprintln!("CONCLUSION: second hop present in 1-hop cache = {any_second_hop_in_cache}");
}

/// The full `export_offline(layers, bbox, seed_limit, hop2_refs, out)` signature:
/// compose the chosen layers, resolve seed + 1-hop (full) + 2-hop (references), excise
/// per layer, re-open the LAYERED slice via `open_layers`, and assert the two grades.
/// (aonach763k is a single snapshot, so this exercises the degenerate 1-layer path —
/// but still through `open_http_layers` composition + the grade split + `open_layers`.)
#[test]
#[ignore = "network: range-reads the live 763k corpus on R2"]
fn export_offline_signature_and_grades() {
    use std::collections::HashSet;
    use std::path::Path;

    let remote = DuckReader::open_http(R2_BASE).expect("open_http");
    let (lng, lat) = remote.sample_point().unwrap().unwrap();
    let d = 0.004;
    let bbox = [lng - d, lat - d, lng + d, lat + d];
    let seed = remote.ids_in_bbox(bbox, 300).unwrap();
    let hop1 = remote.neighbours(&seed).unwrap();

    // The full signature, with a bounded 2-hop (the raw reference set fans out through
    // a hub to ~69k, which the cap must tame and FLAG).
    let tmp = std::env::temp_dir().join(format!("ros-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let s = ros_madair_duck::export_offline(&[R2_BASE], bbox, 300, 500, &tmp).expect("export_offline");
    eprintln!(
        "export: seed={} hop1={} refs={} capped={}",
        s.seed, s.hop1, s.refs, s.refs_capped
    );
    assert_eq!(s.layer_dirs.len(), 1, "one layer in → one slice dir out");
    assert!(s.refs_capped, "the 2-hop hub fan-out (~69k) must trip the cap");
    assert_eq!(s.refs, 500, "capped to hop2_ref_cap");

    // unit 2: the derived-provenance manifest at the slice root.
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&s.manifest).unwrap()).unwrap();
    assert_eq!(v["kind"], "offline-slice");
    assert_eq!(v["hop2_ref_cap"], 500);
    assert!(v["region"].is_array(), "records the seed region");
    assert!(
        v["derived_from"][0]["snapshot_id"].as_str().is_some_and(|x| !x.is_empty()),
        "records each source snapshot_id (derived_from)"
    );
    assert!(
        v["layers"][0]["artifacts"][0]["sha256"].as_str().is_some_and(|x| !x.is_empty()),
        "self-attests the slice's own artifact hashes"
    );
    eprintln!("manifest OK: derived_from={} snap={}", v["derived_from"].as_array().map(|a| a.len()).unwrap_or(0), v["derived_from"][0]["snapshot_id"]);

    // Re-open the LAYERED slice OFFLINE via the dynamic composition.
    let layer_paths: Vec<&Path> = s.layer_dirs.iter().map(|p| p.as_path()).collect();
    let local = DuckReader::open_layers(&layer_paths).expect("open_layers slice");

    // Functional: the 1-hop answered offline equals the 1-hop answered online.
    let mut on = hop1.clone();
    on.sort();
    let mut off = local.neighbours(&seed).unwrap();
    off.sort();
    assert_eq!(off, on, "offline 1-hop == online 1-hop");

    // --- GRADES ---
    let tile_rs: HashSet<String> = local.tile_resource_ids().unwrap().into_iter().collect();
    let entry_rs: HashSet<String> = local.entry_resource_ids().unwrap().into_iter().collect();

    // Seed (region) is full grade: in the drawn bbox, so it has tiles.
    for id in &seed {
        assert!(tile_rs.contains(id), "seed {id} must have tiles (full grade)");
    }
    // Reference grade EXISTS and is tile-free: entries named in `resources` with no
    // tiles are exactly the 2-hop references (named, linkable, located — not hydratable).
    let ref_grade: Vec<&String> = entry_rs.difference(&tile_rs).collect();
    eprintln!("reference-grade entries (named, no tiles): {}", ref_grade.len());
    assert!(
        !ref_grade.is_empty(),
        "the 2-hop references must be present at reference grade (named, no tiles)"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
