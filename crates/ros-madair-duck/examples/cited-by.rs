//! Local check for the parquet cited_by reverse-lookup (Logainm placenames).
//!   cargo run --release --example cited-by --manifest-path <...> -- \
//!     <node_uuid> <target_uuid> <dir>...
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: cited-by <node_uuid> <target_uuid> <dir>...");
        std::process::exit(2);
    }
    let node_id = &args[0];
    let target = &args[1];
    let dirs: Vec<&Path> = args[2..].iter().map(|s| Path::new(s.as_str())).collect();
    let ids = ros_madair_duck::cited_by(&dirs, node_id, target).expect("cited_by");
    eprintln!("[cited-by] {} citers of {} via node {}", ids.len(), target, node_id);
    for id in ids.iter().take(8) {
        eprintln!("  {id}");
    }
}
