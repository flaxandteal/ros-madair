//! Local hydrate perf harness - reproduces v2_hydrate_layers' hydrate cost against
//! the on-disk parquet datasets, no device in the loop.
//!
//!   RM_HYDRATE_PERF=1 cargo run --release --example hydrate-perf \
//!     --manifest-path crates/ros-madair-duck/Cargo.toml -- \
//!     <uuid> <base-dir> <overlay-dir>...
//!
//! e.g. (from the Greasan repo, dirs base-first as the app composes them):
//!   RM_HYDRATE_PERF=1 cargo run --release --example hydrate-perf \
//!     --manifest-path ../magic/RosMadair-sandbox-parquet/crates/ros-madair-duck/Cargo.toml -- \
//!     9f29742a-33e7-57d5-99ea-6febb7d8dffb \
//!     data/parquet-wiktionary data/parquet-macbain data/parquet-gramadan-forms \
//!     data/parquet-place data/parquet-concept data/parquet-example-tatoeba \
//!     data/parquet-example-gaois data/parquet-example-udt data/parquet-person \
//!     data/parquet-note data/parquet-layer

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use alizarin_core::graph::StaticGraph;
use alizarin_core::LayeredGraph;

fn load_graph(dir: &Path) -> StaticGraph {
    let raw = std::fs::read_to_string(dir.join("graph.json")).expect("read graph.json");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("parse graph.json");
    let v = json
        .get("graph")
        .and_then(|g| g.get(0))
        .cloned()
        .unwrap_or(json);
    let mut g: StaticGraph = serde_json::from_value(v).expect("graph schema");
    g.build_indices();
    g
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: hydrate-perf <uuid> <base-dir> <overlay-dir>...");
        std::process::exit(2);
    }
    let uuid = args[0].clone();
    let dirs: Vec<&Path> = args[1..].iter().map(|s| Path::new(s.as_str())).collect();
    let langs = ["ga", "gd", "en"];
    let layer_ids: Vec<String> = dirs
        .iter()
        .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    // Same composition as v2_hydrate_layers: base + fxg-bearing overlays.
    let base = Arc::new(load_graph(dirs[0]));
    let overlays: Vec<Arc<StaticGraph>> = dirs[1..]
        .iter()
        .filter_map(|d| {
            let g = load_graph(d);
            if g.functions_x_graphs.as_ref().is_some_and(|v| !v.is_empty()) {
                Some(Arc::new(g))
            } else {
                None
            }
        })
        .collect();
    eprintln!(
        "[perf] {} layers, {} fxg overlays; resolving {}",
        dirs.len(),
        overlays.len(),
        uuid
    );
    let composed = LayeredGraph::new(base, overlays);
    // Empty registry: the derive is one word's paradigm (cheap); this isolates the
    // gather / labels / tree cost that dominates the ~1s.
    let registry = alizarin_core::default_functions_registry();

    // One warm run (builds the LayeredGraph merged index + duck warmup)...
    let _ = ros_madair_duck::hydrate_layers(&dirs, &uuid, &composed, &langs, Some(&layer_ids), &registry)
        .expect("warm hydrate");
    // ...then time N.
    let n = 5;
    let t = Instant::now();
    for _ in 0..n {
        let _ = ros_madair_duck::hydrate_layers(&dirs, &uuid, &composed, &langs, Some(&layer_ids), &registry)
            .expect("hydrate");
    }
    eprintln!("[perf] {n} runs, {:.1}ms/run", t.elapsed().as_millis() as f64 / n as f64);
}
