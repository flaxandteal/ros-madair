// SPDX-License-Identifier: AGPL-3.0-or-later
//! The emitter: compiles alizarin-governed graphs + business data + SKOS
//! vocabularies into the DuckDB+Parquet substrate —
//!
//!   tiles_<slug>.parquet   — one row per tile; promoted typed columns
//!                            (concept_id / q_ordered / geo_*) whose row-group
//!                            min/max stats are the zone-map; a `data` JSON
//!                            column carries the hydration payload
//!   edges_<slug>.parquet   — one row per link target (the columnar edge table
//!                            the path/`OnLink` compiler semijoins)
//!   concept_catalog.parquet — concept DFS intervals (hierarchy = a range join)
//!   manifest.json          — the layout / FORMAT_VERSION contract; its
//!                            `snapshot_id` is the digest signing attests over
//!
//! Deterministic: no timestamps; the snapshot id is a hash of the artifact
//! hashes plus the (id-excluded) manifest. Full-text search is the Pagefind
//! sidecar — the substrate indexes concepts, ordered scalars, geometry, and
//! links, not text.
//!
//! Module layout: `input` (data-layout detection and loading), `closure`
//! (SKOS loading/normalization and the concept closure), `geo` (bbox
//! extraction), `parquet` (the tile-row + edge writer), `manifest` (artifact
//! hashing + snapshot id), and `attest` (build-time signing over the manifest).

#[cfg(feature = "attest")]
mod attest;
mod closure;
mod geo;
mod input;
mod manifest;
mod parquet;

#[cfg(feature = "attest")]
pub use attest::{seal_and_sign, sign_head, verify_head, SigningIdentity};
pub use closure::{build_closure, Closure, ClosureEntry};
#[cfg(feature = "attest")]
pub use ros_madair_format::attest::{
    attributions, ed25519_to_multibase, multibase_to_ed25519, verify_bundle, AttestationBundle,
    Attribution, HeadTrust, Role, Verdict,
};
/// The artifact FORMAT types (manifest contract + chunk-tile wire shape) live
/// in `ros-madair-format` — WASM-buildable, so a browser/Tauri READER can parse
/// what this crate writes without linking the emitter (and without a
/// hand-mirrored copy of the wire format, which is how they drift). Re-exported
/// here so existing callers keep working.
pub use ros_madair_format::{ArtifactEntry, Budgets, Manifest, ModelManifest, TierManifest};

pub type EmitError = Box<dyn std::error::Error>;

/// A progress signal from a running emit, delivered to the `on_progress` sink of
/// [`emit_parquet_with_progress`]. **Side-effect-only** — observing progress does not
/// change the artifact, so the snapshot id is identical whether or not anything
/// is listening.
///
/// NOTE: progress does NOT reflect peak memory. Over a single large prebuild file
/// the input parse dominates RSS and happens up front (before streaming ticks);
/// this reports *work done*, not *memory used*. See `HANDOFF-streaming-build.md`.
#[derive(Debug, Clone)]
pub enum EmitProgress {
    /// A named phase began: `"loading"`, `"interning"`, `"streaming"`,
    /// `"finalizing"`.
    Phase(&'static str),
    /// `done` of `total` resources streamed. `total` is known after the interning
    /// pass; fired at a coarse cadence (~100 ticks over the run), not per resource.
    Streaming { done: usize, total: usize },
}

/// The emitter's extension-type registry. Defined ONCE, in
/// `ros-madair-handlers`, which the query side (and any WASM consumer) uses
/// too: an emitter registry that disagreed with the query-side registry does
/// not error, it silently returns zero rows (see that crate's docs). Re-exported
/// here so existing callers keep working.
pub use ros_madair_handlers::default_registry;

// Slice 1 of the DuckDB+Parquet substrate: the additive tile-row Parquet writer.
// `crate::` disambiguates the local module from the extern `parquet` crate.
pub use crate::parquet::{
    emit_parquet, emit_parquet_with_progress, write_model_parquet, ClusterConfig, ClusterDim,
    ParquetModelSummary,
};
