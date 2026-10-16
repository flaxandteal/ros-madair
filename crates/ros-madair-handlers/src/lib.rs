// SPDX-License-Identifier: AGPL-3.0-or-later
//! The single source of the Rós Madair extension-type registry.
//!
//! # Why this crate exists (the silent-wrong-answer trap)
//!
//! Extension datatypes are not known to `alizarin-core`. `reference` — and its
//! `ConceptHierarchical` index class, the thing that makes a field land in
//! `concept_tags` and therefore be *filterable* — is contributed entirely by
//! the `alizarin-clm-core` handler, registered in an [`ExtensionTypeRegistry`].
//!
//! The emitter indexes with one registry; the query compiler plans against
//! another. Before this crate they were two hand-written copies. If they
//! disagreed, nothing failed loudly:
//!
//! * emit *without* the clm handler → `reference` degrades to `DetailOnly` →
//!   no `concept_tags` rows are ever written;
//! * query *with* the clm handler → the field looks head-indexed → the
//!   compiler happily emits SQL → the SQL runs → **zero rows**.
//!
//! The consumer reads "no matches" when the truth is "never indexed". So:
//!
//! 1. there is now exactly ONE [`default_registry`] (emitter, Python binding
//!    and any WASM consumer all call it), and
//! 2. the artifact *declares* the handler set it was emitted with
//!    ([`declared_handlers`] → the manifest's `handlers` block), and the query
//!    side rebuilds its registry from that declaration
//!    ([`registry_from_declarations`]), failing LOUDLY when this build cannot
//!    provide a declared handler rather than quietly answering nothing.
//!
//! # WASM
//!
//! Browser consumers query but never emit. This crate is therefore held to
//! `wasm32-unknown-unknown`: core + handler providers only, no rusqlite, no
//! filesystem. It is the seam that lets a browser get the emitter's registry
//! without the emitter.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use alizarin_core::extension_type_registry::ExtensionTypeRegistry;
use alizarin_core::extension_type_registry::{
    ExtensionError, ExtensionTypeHandler, HandlerCapabilities, IndexClass, IndexSpec,
};

/// The version of the `alizarin-clm-core` actually linked into this build
/// (read from its manifest by `build.rs`, so it cannot drift).
pub const CLM_CORE_VERSION: &str = env!("CLM_CORE_VERSION");

/// The provider crate of the `reference` handler.
pub const CLM_CORE_PROVIDER: &str = "alizarin-clm-core";

/// The datatype the built-in `number` handler is registered under.
pub const NUMBER_DATATYPE: &str = "number";

/// The provider of the `number` handler — Rós Madair itself. `number` is a
/// stock Arches datatype that `alizarin-core` happens not to classify (its
/// ordered-scalar built-ins stop at `date`/`edtf`), so RM owns it here rather
/// than patching core.
pub const RM_HANDLERS_PROVIDER: &str = "ros-madair-handlers";

/// This crate's version, recorded as the `number` handler's provenance.
pub const RM_HANDLERS_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default fixed-point precision for a `number` node when its config carries no
/// `decimals` override. Six places survives most heritage magnitudes without
/// risking i64 overflow (value·10⁶ stays in range up to ~9.2·10¹²).
pub const DEFAULT_NUMBER_DECIMALS: u32 = 6;

/// The fixed-point scale (decimal places) for a `number` node. THE shared
/// contract: emit encodes stored values at this scale, and whoever builds a
/// `Range` predicate's `lo`/`hi` MUST encode them at the SAME scale via
/// [`quantize_number`] — otherwise the `BETWEEN` compares mismatched units and
/// silently lies. Reads an optional `decimals` override from the node config,
/// else [`DEFAULT_NUMBER_DECIMALS`]; capped at 18 (the last power of ten an i64
/// can hold).
pub fn number_scale(config: Option<&Value>) -> u32 {
    config
        .and_then(|c| c.get("decimals"))
        .and_then(|d| d.as_u64())
        .map(|d| d.min(18) as u32)
        .unwrap_or(DEFAULT_NUMBER_DECIMALS)
}

/// Encode a number into the signed-i64 ordered key via fixed-point scaling
/// (`round(value · 10^decimals)`) — the encoding the melted `ordered_*` store
/// holds and the `Range` `BETWEEN` compares on. `None` for a non-finite value
/// (NaN/±inf have no place in a total order, so they go unindexed, exactly as
/// an unparseable date does). Saturates at the i64 bounds so ordering survives
/// past the representable range rather than wrapping.
pub fn quantize_number(value: f64, decimals: u32) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let scaled = (value * 10f64.powi(decimals as i32)).round();
    Some(if scaled >= i64::MAX as f64 {
        i64::MAX
    } else if scaled <= i64::MIN as f64 {
        i64::MIN
    } else {
        scaled as i64
    })
}

// ---------------------------------------------------------------------------
// Grid-cell spatial contract
// ---------------------------------------------------------------------------
//
// A resource's geometry centroid → a discrete whole-world cell. emit writes one
// `grid_<graphid>.parquet` row per resource (`cell, resource_id`) sorted by
// `cell`; the compiler lowers a `GridBox` to `cell IN (cells_in_box(...))`. Both
// MUST compute cells identically or the `IN` silently misses rows — so, exactly
// like [`number_scale`], the cell scheme lives in ONE place (here, wasm-clean)
// that emit and compile share. The scheme: a `GRID_BITS`-per-axis Z-order
// (Morton) cell, whole-world extent. 16 bits ≈ 360°/65536 ≈ 0.0055° ≈ 600 m
// cells — ample locality for a city-scale query; a box typically spans O(10)
// cells per axis.

/// Bits per axis in the grid cell. The cell is a 2·GRID_BITS Z-order code.
pub const GRID_BITS: u32 = 16;
/// A box spanning more than this many cells is almost certainly a mistake (a
/// near-global query); [`cells_in_box`] refuses it loudly rather than emit a
/// multi-thousand-element `IN` list. 16384 = a 128×128-cell window (~77 km²).
pub const MAX_BOX_CELLS: usize = 16_384;

/// Spread the low [`GRID_BITS`] of `x` into even bit positions of a u64.
fn spread_bits(x: u32) -> u64 {
    let mut x = x as u64 & 0xFFFF_FFFF;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Interleave `a` (even bits) and `b` (odd bits) — a 2D Morton / Z-order code.
fn morton2(a: u32, b: u32) -> u64 {
    spread_bits(a) | (spread_bits(b) << 1)
}

/// Quantize one longitude to its axis grid index (0..2^GRID_BITS − 1). Monotonic
/// in `lng`, so a point inside `[min_lng, max_lng]` always lands in
/// `[lng_index(min_lng), lng_index(max_lng)]` — the property [`cells_in_box`]
/// relies on to cover the box exactly.
fn lng_index(lng: f64) -> u32 {
    let n = ((lng + 180.0) / 360.0).clamp(0.0, 1.0);
    (n * ((1u32 << GRID_BITS) - 1) as f64).round() as u32
}

/// Quantize one latitude to its axis grid index (0..2^GRID_BITS − 1).
fn lat_index(lat: f64) -> u32 {
    let n = ((lat + 90.0) / 180.0).clamp(0.0, 1.0);
    (n * ((1u32 << GRID_BITS) - 1) as f64).round() as u32
}

/// A geometry centroid `(lng, lat)` → its Z-order grid cell. The value emit
/// stores in `grid_<graphid>.parquet`'s `cell` column. Returned as `i64` (the
/// DuckDB `BIGINT` the cell column holds); always non-negative (≤ 2^(2·GRID_BITS)).
pub fn grid_cell(lng: f64, lat: f64) -> i64 {
    morton2(lng_index(lng), lat_index(lat)) as i64
}

/// Every grid cell a bounding box overlaps — the `cell IN (…)` set a `GridBox`
/// compiles to. Enumerates the inclusive axis-index rectangle and Morton-codes
/// each; because [`lng_index`]/[`lat_index`] are monotonic, this covers exactly
/// the cells any in-box centroid can occupy (no false negatives). `Err` if the
/// box is degenerate (`min > max`) or spans more than [`MAX_BOX_CELLS`] — a loud
/// failure, never a silent truncation.
pub fn cells_in_box(
    min_lng: f64,
    min_lat: f64,
    max_lng: f64,
    max_lat: f64,
) -> Result<Vec<i64>, String> {
    if min_lng > max_lng || min_lat > max_lat {
        return Err(format!(
            "degenerate grid box: min ({min_lng}, {min_lat}) > max ({max_lng}, {max_lat})"
        ));
    }
    let (gx0, gx1) = (lng_index(min_lng), lng_index(max_lng));
    let (gy0, gy1) = (lat_index(min_lat), lat_index(max_lat));
    let count = (gx1 - gx0 + 1) as usize * (gy1 - gy0 + 1) as usize;
    if count > MAX_BOX_CELLS {
        return Err(format!(
            "grid box spans {count} cells (> {MAX_BOX_CELLS}); the query area is too large for a cell-list prune"
        ));
    }
    let mut cells = Vec::with_capacity(count);
    for gy in gy0..=gy1 {
        for gx in gx0..=gx1 {
            cells.push(morton2(gx, gy) as i64);
        }
    }
    Ok(cells)
}

/// The built-in `number` handler: a `number` node is [`IndexClass::Ordered`],
/// its value melted into the node-keyed `ordered_*` store and range-queryable.
/// Keys are the RAW numeric string; the emitter quantizes them via
/// [`quantize_number`] + [`number_scale`] (per `IndexClass::Ordered`'s contract
/// that the emitter owns the encoding).
struct NumberHandler;

impl ExtensionTypeHandler for NumberHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities {
            can_index: true,
            ..Default::default()
        }
    }

    fn index_spec(
        &self,
        tile_data: &Value,
        _config: Option<&Value>,
    ) -> Result<Option<IndexSpec>, ExtensionError> {
        // A `number` node is Ordered REGARDLESS of the concrete value — the
        // compiler probes the class with `Value::Null` (it has no value in
        // hand). A numeric value yields its raw key; null/non-numeric yields the
        // class with no key (nothing to melt for that particular tile).
        let keys = match tile_data {
            Value::Number(n) => vec![n.to_string()],
            Value::String(s) if s.trim().parse::<f64>().is_ok() => vec![s.trim().to_string()],
            _ => Vec::new(),
        };
        Ok(Some(IndexSpec {
            class: IndexClass::Ordered,
            keys,
        }))
    }
}

/// Construct the `number` handler (symmetry with
/// `alizarin_clm_core::create_reference_handler`).
pub fn create_number_handler() -> Arc<dyn ExtensionTypeHandler> {
    Arc::new(NumberHandler)
}

/// One extension datatype handler, as declared by the artifact that was
/// emitted with it (manifest `handlers[]`) — the record of *what produced this
/// index*, which is what lets the query side reconstruct the same registry
/// instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerDecl {
    /// The datatype the handler is registered under, e.g. `"reference"`.
    pub datatype: String,
    /// The crate that provides it, e.g. `"alizarin-clm-core"`.
    pub provider: String,
    /// The provider's version at emit time. Provenance only — a mismatch is
    /// not (yet) an error; an *unknown datatype* is.
    pub version: String,
}

/// The registry Rós Madair uses: the CLM `reference` handler under its
/// datatype name. THE definition — emitter and query side both call this.
pub fn default_registry() -> ExtensionTypeRegistry {
    let mut registry = ExtensionTypeRegistry::new();
    registry.register(
        alizarin_clm_core::DATATYPE_NAME,
        alizarin_clm_core::create_reference_handler(),
    );
    registry.register(NUMBER_DATATYPE, create_number_handler());
    registry
}

/// What [`default_registry`] provides, in declaration form. The emitter writes
/// this into the manifest (invariant I6: the artifact explains its own handler
/// set).
pub fn declared_handlers() -> Vec<HandlerDecl> {
    let mut decls = vec![
        HandlerDecl {
            datatype: alizarin_clm_core::DATATYPE_NAME.to_string(),
            provider: CLM_CORE_PROVIDER.to_string(),
            version: CLM_CORE_VERSION.to_string(),
        },
        HandlerDecl {
            datatype: NUMBER_DATATYPE.to_string(),
            provider: RM_HANDLERS_PROVIDER.to_string(),
            version: RM_HANDLERS_VERSION.to_string(),
        },
    ];
    // Deterministic, and matches `describe_registry`'s sort so the two round-trip.
    decls.sort_by(|a, b| a.datatype.cmp(&b.datatype));
    decls
}

/// Describe the registry a caller ACTUALLY emitted with, in declaration form.
///
/// The emitter accepts an arbitrary registry, so the manifest must record that
/// registry, not what the default happens to be — otherwise the manifest would
/// itself be the lie it exists to prevent. Datatypes this crate knows are
/// described exactly; anything else is recorded honestly as an unknown
/// provider (and will then fail loudly at [`registry_from_declarations`], which
/// is correct: we cannot reproduce it).
pub fn describe_registry(registry: &ExtensionTypeRegistry) -> Vec<HandlerDecl> {
    let known = declared_handlers();
    let mut decls: Vec<HandlerDecl> = registry
        .list()
        .into_iter()
        .map(|datatype| {
            known
                .iter()
                .find(|d| d.datatype == datatype)
                .cloned()
                .unwrap_or_else(|| HandlerDecl {
                    datatype: datatype.to_string(),
                    provider: "unknown".to_string(),
                    version: "unknown".to_string(),
                })
        })
        .collect();
    // Deterministic: the manifest is hashed and diffed.
    decls.sort_by(|a, b| a.datatype.cmp(&b.datatype));
    decls
}

/// Build a registry covering EXACTLY the declared handlers — the query-side
/// counterpart of [`declared_handlers`].
///
/// Errs, loudly, if a declared datatype is unknown to this build. That error is
/// the whole point: a snapshot indexed by a handler we cannot reproduce would
/// otherwise compile and return zero rows. Never silently omit a handler.
pub fn registry_from_declarations(decls: &[HandlerDecl]) -> Result<ExtensionTypeRegistry, String> {
    let mut registry = ExtensionTypeRegistry::new();
    for decl in decls {
        match decl.datatype.as_str() {
            d if d == alizarin_clm_core::DATATYPE_NAME => {
                registry.register(
                    alizarin_clm_core::DATATYPE_NAME,
                    alizarin_clm_core::create_reference_handler(),
                );
            }
            d if d == NUMBER_DATATYPE => {
                registry.register(NUMBER_DATATYPE, create_number_handler());
            }
            other => {
                return Err(format!(
                    "snapshot was emitted with handler '{other}' (provider '{}', version '{}') \
                     which this build cannot provide; querying it would silently return no rows. \
                     Known handlers: [{}]",
                    decl.provider,
                    decl.version,
                    known_datatypes().join(", "),
                ));
            }
        }
    }
    Ok(registry)
}

/// The datatypes this build can provide handlers for.
pub fn known_datatypes() -> Vec<&'static str> {
    vec![alizarin_clm_core::DATATYPE_NAME, NUMBER_DATATYPE]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_handlers_match_the_default_registry() {
        let decls = declared_handlers();
        assert_eq!(decls.len(), 2);
        // Sorted: "number" precedes "reference".
        assert_eq!(decls[0].datatype, "number");
        assert_eq!(decls[1].datatype, "reference");
        // Every declaration must be reproducible by this build.
        let registry = match registry_from_declarations(&decls) {
            Ok(r) => r,
            Err(e) => panic!("own declarations must rebuild: {e}"),
        };
        assert!(registry.get("reference").is_some());
        assert!(registry.get("number").is_some());
        let def = default_registry();
        assert!(def.get("reference").is_some());
        assert!(def.get("number").is_some());
    }

    #[test]
    fn number_quantizes_fixed_point_and_preserves_order() {
        assert_eq!(quantize_number(3.14, 2), Some(314));
        assert_eq!(quantize_number(3.149, 2), Some(315)); // rounds up
        assert_eq!(quantize_number(-2.5, 0), Some(-3)); // half away from zero
        assert_eq!(quantize_number(0.0, 6), Some(0));
        assert!(quantize_number(f64::NAN, 2).is_none());
        assert!(quantize_number(f64::INFINITY, 2).is_none());
        // Numeric order survives the encoding — the whole point of a zone-map.
        assert!(quantize_number(1.0, 6).unwrap() < quantize_number(1.000_001, 6).unwrap());
        assert!(quantize_number(-5.0, 2).unwrap() < quantize_number(-4.99, 2).unwrap());
    }

    #[test]
    fn number_scale_reads_config_else_default() {
        assert_eq!(number_scale(None), DEFAULT_NUMBER_DECIMALS);
        assert_eq!(number_scale(Some(&serde_json::json!({ "decimals": 2 }))), 2);
        assert_eq!(
            number_scale(Some(&serde_json::json!({ "decimals": 99 }))),
            18
        ); // capped
    }

    #[test]
    fn number_node_is_ordered_on_null_probe_and_keys_the_value() {
        use alizarin_core::datatype_index::datatype_index_spec;
        let reg = default_registry();
        // The compiler's class probe (Value::Null) must still see Ordered.
        let probe = datatype_index_spec("number", &serde_json::Value::Null, None, Some(&reg));
        assert_eq!(probe.class, IndexClass::Ordered);
        assert!(probe.keys.is_empty());
        // A concrete value yields its raw key for the emitter to quantize.
        let valued = datatype_index_spec("number", &serde_json::json!(42.5), None, Some(&reg));
        assert_eq!(valued.class, IndexClass::Ordered);
        assert_eq!(valued.keys, vec!["42.5".to_string()]);
    }

    #[test]
    fn describe_registry_round_trips_the_default() {
        assert_eq!(describe_registry(&default_registry()), declared_handlers());
    }

    #[test]
    fn version_is_real() {
        assert_ne!(CLM_CORE_VERSION, "unknown");
    }

    #[test]
    fn grid_cell_is_deterministic_and_nonnegative() {
        // Same point → same cell (the emit/compile contract rests on this).
        let a = grid_cell(80.27, 13.08);
        let b = grid_cell(80.27, 13.08);
        assert_eq!(a, b);
        assert!(a >= 0);
        // Distinct faraway points → distinct cells.
        assert_ne!(grid_cell(80.27, 13.08), grid_cell(-122.4, 37.8));
    }

    #[test]
    fn cells_in_box_covers_every_contained_point() {
        // Any centroid inside the box must have its grid_cell present in the
        // box's cell set — the no-false-negative property the IN prune needs.
        let (min_lng, min_lat, max_lng, max_lat) = (80.20, 13.00, 80.30, 13.10);
        let cells = cells_in_box(min_lng, min_lat, max_lng, max_lat).unwrap();
        let set: std::collections::HashSet<i64> = cells.iter().copied().collect();
        assert!(!set.is_empty());
        for i in 0..=10 {
            for j in 0..=10 {
                let lng = min_lng + (max_lng - min_lng) * (i as f64 / 10.0);
                let lat = min_lat + (max_lat - min_lat) * (j as f64 / 10.0);
                assert!(
                    set.contains(&grid_cell(lng, lat)),
                    "cell for ({lng},{lat}) missing from box set"
                );
            }
        }
    }

    #[test]
    fn cells_in_box_rejects_degenerate_and_oversize() {
        assert!(cells_in_box(80.3, 13.0, 80.2, 13.1).is_err()); // min_lng > max_lng
        assert!(cells_in_box(-180.0, -90.0, 180.0, 90.0).is_err()); // whole world
    }

    #[test]
    fn unknown_declared_handler_is_loud() {
        let err = match registry_from_declarations(&[HandlerDecl {
            datatype: "spatial-hex".into(),
            provider: "some-other-crate".into(),
            version: "9.9.9".into(),
        }]) {
            Ok(_) => panic!("unknown handler must not silently rebuild"),
            Err(e) => e,
        };
        assert!(err.contains("spatial-hex"), "{err}");
        assert!(err.contains("cannot provide"), "{err}");
    }
}
