// SPDX-License-Identifier: AGPL-3.0-or-later
//! Manifest types (the contract — see plan M1.1) plus artifact hashing
//! and snapshot id derivation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ros_madair_handlers::HandlerDecl;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::chunks::hex;
use crate::EmitError;

/// 4: adds the `handlers` block — the artifact declares the extension-type
/// handler set it was emitted with (I6).
pub(crate) const MANIFEST_VERSION: u32 = 4;

#[derive(Serialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub snapshot_id: String,
    pub base_uri: String,
    pub min_client_version: String,
    /// Present when this artifact graph was emitted as a named tier
    /// (M1.5): records the tier name and what was excluded, so the M2
    /// compiler can check tier permissions from the manifest alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<TierManifest>,
    /// The extension-type handlers this artifact was emitted with (I6). The
    /// query side rebuilds its registry from these rather than assuming a
    /// default: a registry mismatch is otherwise silent — a field that was
    /// never indexed (handler absent at emit) still compiles to valid SQL for
    /// a compiler that has the handler, and returns zero rows.
    pub handlers: Vec<HandlerDecl>,
    pub models: Vec<ModelManifest>,
    pub artifacts: Vec<ArtifactEntry>,
    pub budgets: Budgets,
}

#[derive(Serialize, Clone)]
pub struct TierManifest {
    pub name: String,
    pub exclude_nodegroups: Vec<String>,
    pub exclude_models: Vec<String>,
}

#[derive(Serialize)]
pub struct ModelManifest {
    pub slug: String,
    pub graph_id: String,
    pub spine_table: String,
    /// alias -> physical location, keyed for the query compiler (M2).
    pub fields: BTreeMap<String, FieldEntry>,
    pub resource_count: usize,
}

#[derive(Serialize)]
pub struct FieldEntry {
    pub node_id: String,
    pub datatype: String,
    /// "concept" | "link-coarse" | "detail-only".
    /// concept: exact rows in `concept_tags`; hierarchy queries via
    ///   `concept BETWEEN vocab.dfs_enter AND vocab.dfs_leave` (P10).
    /// link-coarse: NO exact head table — only
    ///   `chunk_link_summary(chunk, node, min_target, max_target, n)`;
    ///   the client plans coarse against the summary and resurfaces
    ///   exact pairs from the tile chunks (P1/P2). Exact head link
    ///   tables would be an opt-in, schema-declared field class.
    /// Everything else (strings, numbers, dates, geo, …) is
    /// detail-only in this iteration.
    pub storage: String,
    /// "filterable" | "coarse" | "detail-only". Datatype inference is
    /// only a proposal; a schema declaration
    /// (EmitOptions.field_classes / --field-classes) overrides it and
    /// is recorded here, so the M2 compiler can read head membership
    /// from the manifest alone (M1 item 3).
    pub class: String,
}

#[derive(Serialize)]
pub struct ArtifactEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Serialize)]
pub struct Budgets {
    pub max_result_rows: u32,
    pub max_group_count: u32,
}

#[derive(Serialize)]
pub struct EmitSummary {
    pub snapshot_id: String,
    pub models: usize,
    pub resources: usize,
    pub tiles: usize,
    pub chunks: usize,
    pub concepts: usize,
    pub dict_terms: usize,
    pub head_db_bytes: u64,
}

/// Hash every artifact under `out`; the snapshot id is derived from the
/// hash set. Returns the artifact entries and the snapshot id.
pub(crate) fn hash_artifacts(out: &Path) -> Result<(Vec<ArtifactEntry>, String), EmitError> {
    let mut artifacts = Vec::new();
    let mut hasher_all = Sha256::new();
    let mut paths: Vec<PathBuf> = vec![out.join("head.sqlite"), out.join("closure.json")];
    let mut chunk_paths: Vec<PathBuf> = fs::read_dir(out.join("chunks"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    chunk_paths.sort();
    paths.extend(chunk_paths);
    for path in &paths {
        let bytes = fs::read(path)?;
        let hash = hex(&Sha256::digest(&bytes));
        hasher_all.update(hash.as_bytes());
        artifacts.push(ArtifactEntry {
            path: path
                .strip_prefix(out)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string(),
            bytes: bytes.len() as u64,
            sha256: hash,
        });
    }
    let snapshot_id = hex(&hasher_all.finalize())[..16].to_string();
    Ok((artifacts, snapshot_id))
}
