// SPDX-License-Identifier: AGPL-3.0-or-later
//! CLI: ros-madair-emit <data_dir> <out_dir> [--base-uri URI]
//!                    [--tier <name>:<config.json>]
//!
//! data_dir layout (AlizarinProvider/Clódóir convention):
//!   graphs/*.json          {"graph": [...]} resource models
//!   resources/**/*.json    business data
//!   vocabularies/*.xml     SKOS (optional)
//!
//! Head membership is datatype-driven — concept/link fields are indexed,
//! everything else is detail-only — so there is no field-class flag. To index
//! a subset of concept fields, prune a search graph (prune_graph) upstream.
//!
//! --tier (M1.5): emit a named tier as its own complete artifact graph
//! under <out_dir>/<name>/. The config JSON is
//!   {"exclude_nodegroups": [...], "exclude_models": [...]}
//! (both keys optional; models by slug or graph id). Exclusions are
//! applied at one point before indexing and chunking, so excluded data
//! appears in NO artifact of that tier (head tables, summaries,
//! fragment directory, chunk files). Without --tier the plain
//! single-tier emit writes to <out_dir> directly.

use std::path::Path;
use std::process::ExitCode;

use serde::Deserialize;

use ros_madair_emit::{EmitOptions, TierManifest};

const USAGE: &str = "usage: ros-madair-emit <data_dir> <out_dir> \
     [--base-uri URI] [--tier <name>:<config.json>]";

/// On-disk tier config (the CLI supplies the name).
#[derive(Deserialize)]
struct TierConfig {
    #[serde(default)]
    exclude_nodegroups: Vec<String>,
    #[serde(default)]
    exclude_models: Vec<String>,
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// `ros-madair-emit sign <head_dir> <key_path>` — seal the head's manifest
/// (rebuild its content hashes / snapshot_id) and write a signed
/// `attestations.json` beside it. The build-time signing entry point.
#[cfg(feature = "attest")]
fn sign_command(args: &[String]) -> ExitCode {
    let (Some(head), Some(key)) = (args.get(2), args.get(3)) else {
        eprintln!(
            "usage: ros-madair-emit sign <head_dir> <key_path> \
             [--role derived|endorsed --actor <uri> [--actor-name <name>]]"
        );
        return ExitCode::from(2);
    };
    // Optional named attribution: a build-time packager signs `--role derived
    // --actor <F&T uri>`; the upstream publisher would sign `--role endorsed`.
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
    let attribution = actor
        .as_deref()
        .map(|a| (role, a, actor_name.as_str()));
    match ros_madair_emit::seal_and_sign(Path::new(head), Path::new(key), attribution) {
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

/// `ros-madair-emit verify <head_dir>` — recompute + verify a head's
/// attestations, print `verified` / `unsigned` / `tampered: <reason>`. Exit 0
/// only when verified, so a build/CI step can gate on it.
#[cfg(feature = "attest")]
fn verify_command(args: &[String]) -> ExitCode {
    let Some(head) = args.get(2) else {
        eprintln!("usage: ros-madair-emit verify <head_dir>");
        return ExitCode::from(2);
    };
    match ros_madair_emit::verify_head(Path::new(head)) {
        Ok(ros_madair_emit::HeadTrust::Verified { authored, attributions }) => {
            if attributions.is_empty() {
                println!("verified ({authored} attestation(s); anonymous)");
            } else {
                for a in &attributions {
                    println!("verified: {:?} by {} <{}>", a.role, a.actor_name, a.actor_id);
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
/// `sec:publicKeyMultibase` form (mints the key if absent). Use it to obtain the
/// value to PIN as F&T's root, or to list in an actor→key registry.
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
    if args.get(1).map(String::as_str) == Some("sign") {
        return sign_command(&args);
    }
    if args.get(1).map(String::as_str) == Some("verify") {
        return verify_command(&args);
    }
    if args.get(1).map(String::as_str) == Some("pubkey") {
        return pubkey_command(&args);
    }
    let mut positional = Vec::new();
    let mut base_uri = "https://example.org/".to_string();
    let mut options = EmitOptions::default();
    let mut tier_arg: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--base-uri" {
            i += 1;
            base_uri = args.get(i).cloned().unwrap_or(base_uri);
        } else if args[i] == "--tier" {
            i += 1;
            match args.get(i) {
                Some(v) => tier_arg = Some(v.clone()),
                None => return usage(),
            }
        } else {
            positional.push(args[i].clone());
        }
        i += 1;
    }
    if positional.len() != 2 {
        return usage();
    }

    let mut out_dir = positional[1].clone();
    if let Some(spec) = tier_arg {
        let Some((name, config_path)) = spec.split_once(':') else {
            eprintln!("--tier expects <name>:<config.json>, got: {spec}");
            return usage();
        };
        if name.is_empty() || config_path.is_empty() {
            eprintln!("--tier expects <name>:<config.json>, got: {spec}");
            return usage();
        }
        let config: TierConfig = match std::fs::read_to_string(config_path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to read tier config {config_path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        options.tier = Some(TierManifest {
            name: name.to_string(),
            exclude_nodegroups: config.exclude_nodegroups,
            exclude_models: config.exclude_models,
        });
        // Each tier is a complete, self-contained artifact graph in its
        // own subdirectory of out_dir.
        out_dir = Path::new(&out_dir).join(name).to_string_lossy().to_string();
    }

    // Register the CLM reference handler so the plain CLI path indexes
    // `reference` fields (core knows no `reference` — the handler does).
    let registry = ros_madair_emit::default_registry();
    // Progress to STDERR (stdout stays clean for the summary JSON). Greppable
    // `PROGRESS …` lines — watch a long emit with `… 2>&1 | grep PROGRESS`, or
    // read them live when the emit runs as a background process.
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
    match ros_madair_emit::emit_with_progress(
        &positional[0],
        &out_dir,
        &base_uri,
        &options,
        &registry,
        &mut on_progress,
    ) {
        Ok(summary) => {
            println!("{}", serde_json::to_string_pretty(&summary).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("emit failed: {e}");
            ExitCode::FAILURE
        }
    }
}
