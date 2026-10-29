// SPDX-License-Identifier: AGPL-3.0-or-later
//! CLI: ros-madair-emit <data_dir> <out_dir> [--base-uri URI]
//!
//! data_dir layout (AlizarinProvider/Clódóir convention):
//!   graphs/*.json          {"graph": [...]} resource models
//!   resources/**/*.json    business data
//!   vocabularies/*.xml     SKOS (optional)
//!
//! Compiles the data_dir into the DuckDB+Parquet substrate — `tiles_*.parquet`,
//! `edges_*.parquet`, `concept_catalog.parquet`, and a self-describing
//! `manifest.json`. The `sign`/`verify`/`pubkey` subcommands attest the emitted
//! snapshot over its manifest `snapshot_id` (storage-agnostic — the same digest
//! the reader trusts).

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: ros-madair-emit <data_dir> <out_dir> [--base-uri URI] \
                     [--partition-by-nodegroup] [--no-render-cache] [--memory-limit SIZE] \
                     [--tier NAME [--exclude-nodegroup NG]... [--exclude-model GRAPHID]... \
                     [--include-resources IDS_FILE]]";

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// `ros-madair-emit sign <dir> <key_path>` — seal a snapshot's manifest (rebuild
/// its content hashes / snapshot_id) and write a signed `attestations.json`
/// beside it. The build-time signing entry point; works on any emitted snapshot.
#[cfg(feature = "attest")]
fn sign_command(args: &[String]) -> ExitCode {
    let (Some(dir), Some(key)) = (args.get(2), args.get(3)) else {
        eprintln!(
            "usage: ros-madair-emit sign <dir> <key_path> \
             [--role derived|endorsed --actor <uri> [--actor-name <name>]]"
        );
        return ExitCode::from(2);
    };
    let mut actor: Option<String> = None;
    let mut actor_name = String::new();
    let mut role = ros_madair_emit::Role::Derived;
    let mut i = 4;
    while i < args.len() {
        match args[i].as_str() {
            "--actor" => {
                i += 1;
                actor = args.get(i).cloned();
            }
            "--actor-name" => {
                i += 1;
                actor_name = args.get(i).cloned().unwrap_or_default();
            }
            "--role" => {
                i += 1;
                role = match args.get(i).map(String::as_str) {
                    Some("derived") => ros_madair_emit::Role::Derived,
                    Some("endorsed") => ros_madair_emit::Role::Endorsed,
                    other => {
                        eprintln!("--role expects derived|endorsed, got {other:?}");
                        return ExitCode::from(2);
                    }
                };
            }
            other => {
                eprintln!("unknown flag {other:?}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let attribution = actor.as_deref().map(|a| (role, a, actor_name.as_str()));
    match ros_madair_emit::seal_and_sign(Path::new(dir), Path::new(key), attribution) {
        Ok(id) => {
            println!("{id}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sign failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "attest"))]
fn sign_command(_args: &[String]) -> ExitCode {
    eprintln!("`sign` needs the `attest` feature (it is on by default)");
    ExitCode::FAILURE
}

/// `ros-madair-emit verify <dir>` — recompute + verify a snapshot's attestations,
/// print `verified` / `unsigned` / `tampered: <reason>`. Exit 0 only when
/// verified, so a build/CI step can gate on it.
#[cfg(feature = "attest")]
fn verify_command(args: &[String]) -> ExitCode {
    let Some(dir) = args.get(2) else {
        eprintln!("usage: ros-madair-emit verify <dir>");
        return ExitCode::from(2);
    };
    match ros_madair_emit::verify_head(Path::new(dir)) {
        Ok(ros_madair_emit::HeadTrust::Verified {
            authored,
            attributions,
        }) => {
            if attributions.is_empty() {
                println!("verified ({authored} attestation(s); anonymous)");
            } else {
                for a in &attributions {
                    println!(
                        "verified: {:?} by {} <{}>",
                        a.role, a.actor_name, a.actor_id
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Ok(ros_madair_emit::HeadTrust::Unsigned) => {
            println!("unsigned");
            ExitCode::from(1)
        }
        Ok(ros_madair_emit::HeadTrust::Failed { reason }) => {
            println!("tampered: {reason}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("verify failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "attest"))]
fn verify_command(_args: &[String]) -> ExitCode {
    eprintln!("`verify` needs the `attest` feature (it is on by default)");
    ExitCode::FAILURE
}

/// `ros-madair-emit pubkey <key_path>` — print the identity's public key in
/// `sec:publicKeyMultibase` form (mints the key if absent).
#[cfg(feature = "attest")]
fn pubkey_command(args: &[String]) -> ExitCode {
    let Some(key) = args.get(2) else {
        eprintln!("usage: ros-madair-emit pubkey <key_path>");
        return ExitCode::from(2);
    };
    match ros_madair_emit::SigningIdentity::load_or_create(Path::new(key)) {
        Ok(id) => {
            println!("{}", id.public_key_multibase());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "attest"))]
fn pubkey_command(_args: &[String]) -> ExitCode {
    eprintln!("`pubkey` needs the `attest` feature (it is on by default)");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("sign") => return sign_command(&args),
        Some("verify") => return verify_command(&args),
        Some("pubkey") => return pubkey_command(&args),
        _ => {}
    }

    let mut positional = Vec::new();
    let mut base_uri = "https://example.org/".to_string();
    // Tier (egress-by-emit scoped head, e.g. the insurer window). A head is tiered
    // only when `--tier <name>` is given; the exclusions physically drop rows at emit.
    let mut tier_name: Option<String> = None;
    let mut exclude_nodegroups: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut exclude_models: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut include_resources: Option<std::collections::HashSet<String>> = None;
    // Owner-declared provenance (presentation/policy, not crypto identity): the
    // display name/colour the trust UI shows beside the signature, the data's
    // owner-asserted currency, and the owner's freshness policy. All optional.
    let mut owner_name: Option<String> = None;
    let mut owner_colour: Option<String> = None;
    let mut as_of: Option<String> = None;
    let mut freshness_window_secs: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--base-uri" => {
                i += 1;
                base_uri = args.get(i).cloned().unwrap_or(base_uri);
            }
            // Tier flags — build a default-deny scoped head (see `TierFilter`).
            "--tier" => {
                i += 1;
                tier_name = args.get(i).cloned();
            }
            "--exclude-nodegroup" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    exclude_nodegroups.insert(v.clone());
                }
            }
            "--exclude-model" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    exclude_models.insert(v.clone());
                }
            }
            "--include-resources" => {
                i += 1;
                match args.get(i).map(|f| (f.clone(), std::fs::read_to_string(f))) {
                    Some((_, Ok(s))) => {
                        include_resources = Some(
                            s.lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty())
                                .map(String::from)
                                .collect(),
                        );
                    }
                    Some((f, Err(e))) => {
                        eprintln!("--include-resources: cannot read {f}: {e}");
                        return ExitCode::FAILURE;
                    }
                    None => {
                        eprintln!("--include-resources needs a file path");
                        return ExitCode::FAILURE;
                    }
                }
            }
            // Scale levers for a large server-side emit. They set the env vars
            // ros-madair-emit's `parquet` module reads, so the clustering config
            // need not be threaded through the CLI's empty config map:
            //  - partition each model's tiles per nodegroup, so each COPY sort is
            //    one nodegroup (~hundreds of k rows) not the whole model (millions);
            //  - drop the render-summary __cache (sheds ~49% of tile `data`);
            //  - raise the DuckDB memory cap above the 512MB on-device default.
            "--partition-by-nodegroup" => {
                std::env::set_var("ROS_MADAIR_EMIT_PARTITION_BY_NODEGROUP", "1")
            }
            // Provenance flags — owner-declared, domain-agnostic. Caller-provided
            // (not wall-clock) so the manifest/snapshot_id stays reproducible.
            "--owner-name" => {
                i += 1;
                owner_name = args.get(i).cloned();
            }
            "--owner-colour" | "--owner-color" => {
                i += 1;
                owner_colour = args.get(i).cloned();
            }
            "--as-of" => {
                i += 1;
                as_of = args.get(i).cloned();
            }
            "--freshness-window-secs" => {
                i += 1;
                match args.get(i).map(|v| v.parse::<u64>()) {
                    Some(Ok(n)) => freshness_window_secs = Some(n),
                    Some(Err(_)) => {
                        eprintln!("--freshness-window-secs needs an integer (seconds)");
                        return ExitCode::FAILURE;
                    }
                    None => {
                        eprintln!("--freshness-window-secs needs a value");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--no-render-cache" => std::env::set_var("ROS_MADAIR_EMIT_RENDER_CACHE", "0"),
            "--memory-limit" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    std::env::set_var("ROS_MADAIR_EMIT_MEMORY_LIMIT", v);
                }
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    if positional.len() != 2 {
        return usage();
    }
    // Tier exclusions without `--tier <name>` would silently do nothing (no manifest
    // declaration) — refuse loudly rather than emit a head that looks scoped but isn't.
    let tiered =
        !exclude_nodegroups.is_empty() || !exclude_models.is_empty() || include_resources.is_some();
    if tiered && tier_name.is_none() {
        eprintln!("tier exclusions (--exclude-nodegroup/--exclude-model/--include-resources) require --tier <name>");
        return ExitCode::FAILURE;
    }
    let tier = tier_name.map(|name| ros_madair_emit::TierFilter {
        name,
        exclude_nodegroups,
        exclude_models,
        include_resources,
    });
    // Build the provenance block only when the owner actually declared something —
    // all-None stays None, so an undeclared head's manifest is byte-identical to
    // before (no snapshot_id churn).
    let provenance = (owner_name.is_some()
        || owner_colour.is_some()
        || as_of.is_some()
        || freshness_window_secs.is_some())
    .then(|| ros_madair_emit::ProvenanceManifest {
        owner_display_name: owner_name,
        as_of,
        owner_colour,
        freshness_window_secs,
    });

    // The CLM reference handler is registered so the CLI indexes `reference`
    // fields (core knows no `reference` — the handler does). Default per-graph
    // clustering (geo ⊕ descriptor); pass per-graph config programmatically.
    let registry = ros_madair_emit::default_registry();
    let config_by_graph: HashMap<String, ros_madair_emit::ClusterConfig> = HashMap::new();
    // Progress to STDERR (stdout stays clean for the summary JSON).
    let mut on_progress = |p: ros_madair_emit::EmitProgress| {
        match p {
            ros_madair_emit::EmitProgress::Phase(name) => eprintln!("PROGRESS phase {name}"),
            ros_madair_emit::EmitProgress::Streaming { done, total } => {
                let pct = if total > 0 { done * 100 / total } else { 0 };
                eprintln!("PROGRESS streaming {done}/{total} ({pct}%)");
            }
        }
        std::ops::ControlFlow::Continue(())
    };
    match ros_madair_emit::emit_parquet_with_progress(
        &positional[0],
        &positional[1],
        &base_uri,
        &registry,
        &config_by_graph,
        tier.as_ref(),
        provenance.as_ref(),
        &mut on_progress,
    ) {
        Ok(summaries) => {
            println!("{}", serde_json::to_string_pretty(&summaries).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("emit failed: {e}");
            ExitCode::FAILURE
        }
    }
}
