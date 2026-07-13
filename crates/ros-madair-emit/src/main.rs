// SPDX-License-Identifier: AGPL-3.0-or-later
//! CLI: ros-madair-emit <data_dir> <out_dir> [--base-uri URI]
//!                    [--field-classes JSON]
//!                    [--tier <name>:<config.json>]
//!
//! data_dir layout (AlizarinProvider/Clódóir convention):
//!   graphs/*.json          {"graph": [...]} resource models
//!   resources/**/*.json    business data
//!   vocabularies/*.xml     SKOS (optional)
//!
//! --field-classes: schema-declared head membership (M1 item 3), a JSON
//! object of {"<alias>": "filterable" | "detail-only"} overriding
//! datatype inference. "filterable" on a non-concept/non-link datatype
//! is an error (no text in the head).
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
     [--base-uri URI] [--field-classes JSON] [--tier <name>:<config.json>]";

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut base_uri = "https://example.org/".to_string();
    let mut options = EmitOptions::default();
    let mut tier_arg: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--base-uri" {
            i += 1;
            base_uri = args.get(i).cloned().unwrap_or(base_uri);
        } else if args[i] == "--field-classes" {
            i += 1;
            let Some(raw) = args.get(i) else {
                eprintln!("--field-classes requires a JSON argument\n{USAGE}");
                return ExitCode::from(2);
            };
            match serde_json::from_str(raw) {
                Ok(map) => options.field_classes = map,
                Err(e) => {
                    eprintln!(
                        "--field-classes: expected JSON object of \
                         {{\"<alias>\": \"filterable\"|\"detail-only\"}} ({e})"
                    );
                    return ExitCode::from(2);
                }
            }
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
    match ros_madair_emit::emit_with_options(&positional[0], &out_dir, &base_uri, &options, &registry)
    {
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
