// SPDX-License-Identifier: AGPL-3.0-or-later
//! Input loading (AlizarinProvider/Clódóir data layout): data-layout
//! detection, graph loading, business-data file discovery, slug
//! derivation.
//!
//! STREAMING: graphs load up front (small), but resources are NOT
//! accumulated here — `business_data_files` returns the sorted file list
//! and the caller streams each file's resources through the head/chunk
//! sink one at a time, discarding each after (bounded peak memory,
//! roughly independent of corpus size). The file sort is load-bearing:
//! read_dir order is filesystem-random and resource encounter order
//! decides rid assignment, interner ids, chunk grouping, and hence the
//! snapshot id.

use std::fs;
use std::path::{Path, PathBuf};

use alizarin_core::StaticGraph;

use crate::EmitError;

pub(crate) struct LoadedModel {
    pub(crate) slug: String,
    pub(crate) graph: StaticGraph,
}

fn slugify(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').to_string()
}

fn graph_name(raw: &serde_json::Value) -> String {
    let obj = raw.get("graph").and_then(|g| g.get(0)).unwrap_or(raw);
    match obj.get("name") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Object(m)) => m
            .get("en")
            .or_else(|| m.values().next())
            .and_then(|v| v.as_str())
            .unwrap_or("model")
            .to_string(),
        _ => "model".to_string(),
    }
}

/// Load graph models only (no resources). Graphs are small and load up
/// front; resources stream separately via `business_data_files`.
pub(crate) fn load_graphs(data_dir: &Path) -> Result<Vec<LoadedModel>, EmitError> {
    let mut graphs_dir = data_dir.join("graphs");
    if graphs_dir.join("resource_models").is_dir() {
        graphs_dir = graphs_dir.join("resource_models");
    }
    let mut models = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(&graphs_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && !p
                    .file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with('_'))
        })
        .collect();
    entries.sort();

    for path in entries {
        let raw: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        let graph_val = raw
            .get("graph")
            .and_then(|g| g.get(0))
            .cloned()
            .unwrap_or_else(|| raw.clone());
        let mut graph: StaticGraph = serde_json::from_value(graph_val)?;
        graph.build_indices();
        let slug = slugify(&graph_name(&raw));
        models.push(LoadedModel { slug, graph });
    }
    Ok(models)
}

/// Discover business-data files (recursive over resources/ or
/// prebuild-style business_data/), sorted. The sort is load-bearing:
/// read_dir order is filesystem-random and resource encounter order
/// decides rid assignment, interner ids, chunk grouping, and ultimately
/// the snapshot id. The caller streams these files one at a time so the
/// whole corpus is never resident at once.
pub(crate) fn business_data_files(data_dir: &Path) -> Result<Vec<PathBuf>, EmitError> {
    let mut resources_dir = data_dir.join("resources");
    if !resources_dir.is_dir() {
        resources_dir = data_dir.join("business_data");
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if resources_dir.is_dir() {
        let mut stack = vec![resources_dir];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "json")
                    && !path
                        .file_stem()
                        .is_some_and(|s| s.to_string_lossy().starts_with('_'))
                {
                    files.push(path);
                }
            }
        }
    }
    files.sort();
    Ok(files)
}
