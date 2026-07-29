// SPDX-License-Identifier: AGPL-3.0-or-later
//! A8-locality: order a model's resources by a locality key before chunking, so
//! the coarse per-chunk summaries (`chunk_geo_summary`, `chunk_value_summary`)
//! describe a TIGHT region instead of the whole extent.
//!
//! # Why
//!
//! Chunks pack resources in the order they stream. With no locality sort, a
//! chunk's 256 resources are an arbitrary slice of the corpus, so its union
//! bbox / date range spans nearly everything — measured on the Aonach corpus at
//! 0.91 of the date span (Observation) and 0.35 of the extent (Building). The
//! browser coarse-prune (which reads these summaries to decide which chunks to
//! fetch) is then near-useless. The native reader is unaffected — it uses the
//! exact `geo_bbox` / `value_tags` indexes — so this is a browser-path win only.
//!
//! This is the geo/date sequel to **A9**, which reordered resource-id INTERNING
//! for link-target ranges. A8-locality reorders resource CHUNKING for geo/date
//! ranges. The two orderings must stay the SAME (A9's invariant: a target's id is
//! its chunk position), so the emitter applies this sort in BOTH the pre-intern
//! pass and the streaming pass — see `lib.rs`.
//!
//! # What is sorted
//!
//! Per model, the sort key is derived from the model's dominant indexed locality
//! field: a `SpatialBbox` (geometry) node if it has one — spatial queries are the
//! point — else an `Ordered` (date) node. A model with neither keeps stream order
//! (so non-geo/date models, and hence A9, are untouched). Geometry maps to a
//! Hilbert index of its bbox centre (locality-preserving 2-D → 1-D); a date maps
//! to its quantized day. Resources missing the field sort last, in id order.

use std::collections::HashMap;

use alizarin_core::datatype_index_spec;
use alizarin_core::extension_type_registry::{ExtensionTypeRegistry, IndexClass};
use alizarin_core::graph::{StaticGraph, StaticNode, StaticResource};

use crate::input::LoadedModel;

/// Which field a model is ordered by (the node id it reads).
pub(crate) enum LocalityKind {
    /// No indexed geo/date field — keep stream order (A9 default).
    None,
    /// Geometry node: Hilbert index of the bbox centre.
    Geo(String),
    /// Ordered (date/edtf) node: the quantized day.
    Date(String),
}

/// Hilbert-curve order: a 2^24 grid per axis (~2.4 m cells globally), so a
/// city-scale corpus still has thousands of cells across it — enough resolution
/// that neighbouring buildings get adjacent keys. 24 bits/axis ⇒ 48-bit index,
/// comfortably inside i64.
const HILBERT_ORDER: u32 = 24;

/// A node's config as a JSON object, or `None` — mirrors the copies in `head`
/// and the query compiler (lets a handler resolve its own class).
fn node_config_value(node: &StaticNode) -> Option<serde_json::Value> {
    if node.config.is_empty() {
        return None;
    }
    Some(serde_json::Value::Object(
        node.config
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ))
}

/// Classify each model's locality field once, up front. Geometry wins over date
/// when a model has both (e.g. HazardFootprint) — spatial overlap is the pruned
/// query. Ties within a class break on lowest node id (determinism).
pub(crate) fn model_localities(
    models: &[LoadedModel],
    registry: &ExtensionTypeRegistry,
) -> Vec<LocalityKind> {
    models
        .iter()
        .map(|m| classify(&m.graph, registry))
        .collect()
}

fn classify(graph: &StaticGraph, registry: &ExtensionTypeRegistry) -> LocalityKind {
    let mut nodes: Vec<&StaticNode> = graph.nodes_slice().iter().collect();
    nodes.sort_by(|a, b| a.nodeid.cmp(&b.nodeid));
    let mut geo: Option<String> = None;
    let mut date: Option<String> = None;
    for n in nodes {
        let class = datatype_index_spec(
            &n.datatype,
            &serde_json::Value::Null,
            node_config_value(n).as_ref(),
            Some(registry),
        )
        .class;
        match class {
            IndexClass::SpatialBbox if geo.is_none() => geo = Some(n.nodeid.clone()),
            IndexClass::Ordered if date.is_none() => date = Some(n.nodeid.clone()),
            _ => {}
        }
    }
    match (geo, date) {
        (Some(g), _) => LocalityKind::Geo(g),
        (None, Some(d)) => LocalityKind::Date(d),
        (None, None) => LocalityKind::None,
    }
}

/// An axis-aligned geo extent `(min_lng, min_lat, max_lng, max_lat)`, per model.
pub(crate) type Extent = (f64, f64, f64, f64);

/// The geometry's bbox centre `(lng, lat)` for the given geo node, or `None`.
fn geo_centre(resource: &StaticResource, node: &str) -> Option<(f64, f64)> {
    let value = resource
        .tiles
        .as_deref()?
        .iter()
        .find_map(|t| t.data.get(node))?;
    if value.is_null() {
        return None;
    }
    // Serialize the GeoJSON object the same way the emitter's index path does.
    let (min_lng, min_lat, max_lng, max_lat) = crate::geo::extract_bbox(&value.to_string())?;
    Some(((min_lng + max_lng) / 2.0, (min_lat + max_lat) / 2.0))
}

/// The locality key for one resource under its model's `kind`. `None` when the
/// model has no locality field, or the resource does not carry it (those sort
/// last). The key is only ever compared WITHIN a model, so geo (Hilbert) and date
/// (day) keys never mix. `extent` is the model's own geo bounds (present iff the
/// model is `Geo` and any resource carried a geometry) — the Hilbert grid is
/// stretched to it, so a city-scale corpus uses the full curve resolution rather
/// than a speck of a global grid.
// Superseded by the streaming probe path (`probe` + `sort_probes`), which the
// emitter now uses instead of sorting whole resources. Retained as the canonical
// reference the probe path must match — that equivalence is pinned by the
// snapshot-id verification over real corpora (id-order via tearma, Hilbert via the
// place layer), not by a live caller.
#[allow(dead_code)]
fn locality_key(
    resource: &StaticResource,
    kind: &LocalityKind,
    extent: Option<&Extent>,
) -> Option<i64> {
    match kind {
        LocalityKind::None => None,
        LocalityKind::Geo(node) => {
            let (cx, cy) = geo_centre(resource, node)?;
            Some(hilbert_key(cx, cy, extent?))
        }
        LocalityKind::Date(node) => {
            let value = resource
                .tiles
                .as_deref()?
                .iter()
                .find_map(|t| t.data.get(node.as_str()))?;
            if value.is_null() {
                return None;
            }
            // Same quantizer the emitter indexes with (date/edtf → days-from-civil).
            alizarin_core::quantize::quantize_date(value.as_str()?)
        }
    }
}

/// Hilbert index of a lng/lat point over the MODEL'S extent (not a global grid),
/// so the full 2^order resolution lands on the actual data spread. A degenerate
/// (zero-span) axis maps everything to 0 on that axis — all-same-point corpora
/// just keep id order, harmlessly.
fn hilbert_key(lng: f64, lat: f64, extent: &Extent) -> i64 {
    let n = 1u64 << HILBERT_ORDER;
    let (min_lng, min_lat, max_lng, max_lat) = *extent;
    let quant = |v: f64, lo: f64, hi: f64| -> u64 {
        let span = hi - lo;
        if span <= 0.0 {
            return 0;
        }
        let t = ((v - lo) / span * n as f64).floor();
        if t < 0.0 {
            0
        } else if t >= n as f64 {
            n - 1
        } else {
            t as u64
        }
    };
    let x = quant(lng, min_lng, max_lng);
    let y = quant(lat, min_lat, max_lat);
    hilbert_xy2d(HILBERT_ORDER, x, y) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical 2×2 Hilbert cell order: (0,0)→0, (0,1)→1, (1,1)→2, (1,0)→3.
    #[test]
    fn hilbert_2x2_is_canonical() {
        assert_eq!(hilbert_xy2d(1, 0, 0), 0);
        assert_eq!(hilbert_xy2d(1, 0, 1), 1);
        assert_eq!(hilbert_xy2d(1, 1, 1), 2);
        assert_eq!(hilbert_xy2d(1, 1, 0), 3);
    }

    /// Over any grid the curve is a BIJECTION: every cell maps to a distinct
    /// index in `[0, n²)`. A rotation bug would collide or skip cells.
    #[test]
    fn hilbert_is_a_bijection() {
        let order = 3;
        let n = 1u64 << order;
        let mut seen = std::collections::HashSet::new();
        for x in 0..n {
            for y in 0..n {
                let d = hilbert_xy2d(order, x, y);
                assert!(d < n * n, "d {d} out of range for {n}x{n}");
                assert!(seen.insert(d), "collision at ({x},{y}) -> {d}");
            }
        }
        assert_eq!(seen.len() as u64, n * n);
    }

    /// Locality: consecutive Hilbert indices are spatially adjacent (Manhattan
    /// distance 1) — the property the whole ordering rests on.
    #[test]
    fn consecutive_indices_are_adjacent() {
        let order = 4;
        let n = 1u64 << order;
        let mut pos = vec![(0u64, 0u64); (n * n) as usize];
        for x in 0..n {
            for y in 0..n {
                pos[hilbert_xy2d(order, x, y) as usize] = (x, y);
            }
        }
        for w in pos.windows(2) {
            let dx = w[0].0.abs_diff(w[1].0);
            let dy = w[0].1.abs_diff(w[1].1);
            assert_eq!(dx + dy, 1, "non-adjacent step {:?} -> {:?}", w[0], w[1]);
        }
    }

    /// The model-extent quant maps the extent corners to opposite ends of the
    /// grid (full resolution on the data), and a degenerate axis collapses to 0.
    #[test]
    fn hilbert_key_is_stable_and_bounded() {
        let ext = (80.24, 13.04, 80.30, 13.12); // Chennai-ish
        let a = hilbert_key(80.24, 13.04, &ext);
        let b = hilbert_key(80.30, 13.12, &ext);
        assert_ne!(a, b, "distinct corners get distinct keys");
        // A zero-span extent (all identical points) never panics, yields 0.
        let degenerate = (1.0, 2.0, 1.0, 2.0);
        assert_eq!(hilbert_key(1.0, 2.0, &degenerate), 0);
    }
}

/// Per-model geo extents, scanned from the resources being sorted. Both emit
/// passes call `sort_by_locality` on the identical resource set for a file, so
/// each computes the SAME extents — the Hilbert keys, and hence the order, match
/// (determinism, and A9's interning==chunking invariant).
#[allow(dead_code)] // reference impl; see note on `locality_key`
fn geo_extents(
    resources: &[StaticResource],
    by_graph: &HashMap<&str, usize>,
    localities: &[LocalityKind],
) -> HashMap<usize, Extent> {
    let mut ext: HashMap<usize, Extent> = HashMap::new();
    for r in resources {
        let Some(&i) = by_graph.get(r.resourceinstance.graph_id.as_str()) else {
            continue;
        };
        if let LocalityKind::Geo(node) = &localities[i] {
            if let Some((cx, cy)) = geo_centre(r, node) {
                let e = ext.entry(i).or_insert((cx, cy, cx, cy));
                e.0 = e.0.min(cx);
                e.1 = e.1.min(cy);
                e.2 = e.2.max(cx);
                e.3 = e.3.max(cy);
            }
        }
    }
    ext
}

/// (x,y) → Hilbert distance `d`, the canonical Wikipedia algorithm. `x`, `y` are
/// in `[0, 2^order)`, so the `n-1 - v` reflections never underflow.
fn hilbert_xy2d(order: u32, mut x: u64, mut y: u64) -> u64 {
    let n = 1u64 << order;
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u64::from((x & s) > 0);
        let ry = u64::from((y & s) > 0);
        d += s * s * ((3 * rx) ^ ry);
        // rotate the quadrant
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

/// Reorder a file's resources into locality order, in place. Deterministic and
/// identical for a given input, so the pre-intern and streaming passes stay in
/// lockstep (A9's invariant). Resources are grouped by model first (so keys are
/// only compared within a model), then by locality key, then by id (a total order
/// — no reliance on sort stability). A model with no locality field contributes a
/// constant key, so its resources keep their relative (id) order.
#[allow(dead_code)] // reference impl; see note on `locality_key`
pub(crate) fn sort_by_locality(
    resources: &mut [StaticResource],
    by_graph: &HashMap<&str, usize>,
    localities: &[LocalityKind],
) {
    let extents = geo_extents(resources, by_graph, localities);
    resources.sort_by_cached_key(|r| {
        let (group, key) = match by_graph.get(r.resourceinstance.graph_id.as_str()) {
            Some(&i) => (i as i64, locality_key(r, &localities[i], extents.get(&i))),
            // Unknown model (not emitted): shove to the end, id-ordered.
            None => (i64::MAX, None),
        };
        // `key.is_none()` sorts field-less resources after keyed ones within a
        // model; the id is the final deterministic tiebreak.
        (
            group,
            key.is_none(),
            key.unwrap_or(0),
            r.resourceinstance.resourceinstanceid.clone(),
        )
    });
}

// ============================================================================
// Streaming-friendly variant (memory-bounded emit)
// ============================================================================
//
// `sort_by_locality` needs every `StaticResource` resident to sort them. The
// streaming emit instead collects a tiny [`Probe`] per resource (id + the one
// locality field, a few dozen bytes) while scanning the file, then orders the
// PROBES with [`sort_probes`] and seeks the resources back in that order. The
// order it produces is byte-identical to sorting the resources directly — the
// extent is computed from the same centres, the key from the same formula, the
// tiebreak from the same id — so the snapshot id is unchanged.

/// A minimal locality fingerprint for one resource, collected in the streaming
/// pass so the emitter can order disk offsets without holding whole resources.
/// `centre` is the geometry bbox centre (geo models); `date` is the quantized day
/// (date models); both `None` for no-locality models or resources missing the
/// field — those sort last, in id order, exactly as in `sort_by_locality`.
pub(crate) struct Probe {
    pub group: usize,
    pub id: String,
    pub centre: Option<(f64, f64)>,
    pub date: Option<i64>,
}

/// Extract a [`Probe`] for `resource` (already known to belong to model `group`,
/// whose ordering is `kind`). Reads exactly the fields `sort_by_locality` reads —
/// `geo_centre` for geo, the quantized date for date — so probe order reproduces
/// resource order.
pub(crate) fn probe(resource: &StaticResource, group: usize, kind: &LocalityKind) -> Probe {
    let (centre, date) = match kind {
        LocalityKind::None => (None, None),
        LocalityKind::Geo(node) => (geo_centre(resource, node), None),
        LocalityKind::Date(node) => {
            let d = resource
                .tiles
                .as_deref()
                .and_then(|ts| ts.iter().find_map(|t| t.data.get(node.as_str())))
                .filter(|v| !v.is_null())
                .and_then(|v| v.as_str())
                .and_then(alizarin_core::quantize::quantize_date);
            (None, d)
        }
    };
    Probe {
        group,
        id: resource.resourceinstance.resourceinstanceid.clone(),
        centre,
        date,
    }
}

/// Order the indices of `probes` the SAME way `sort_by_locality` orders the
/// resources they came from: grouped by model, then locality key, then id. Geo
/// extents are recomputed from the probes' own centres — identical to
/// `geo_extents` over the same set — so a geo model's Hilbert keys, and the order,
/// match the whole-parse path. Callers pass a per-file probe set (mirroring the
/// current per-file `sort_by_locality`), keeping cross-file order untouched.
pub(crate) fn sort_probes(probes: &[Probe]) -> Vec<usize> {
    let mut extents: HashMap<usize, Extent> = HashMap::new();
    for p in probes {
        if let Some((cx, cy)) = p.centre {
            let e = extents.entry(p.group).or_insert((cx, cy, cx, cy));
            e.0 = e.0.min(cx);
            e.1 = e.1.min(cy);
            e.2 = e.2.max(cx);
            e.3 = e.3.max(cy);
        }
    }
    let mut idx: Vec<usize> = (0..probes.len()).collect();
    idx.sort_by_cached_key(|&i| {
        let p = &probes[i];
        // Geo key needs the group extent; date key is already computed; no field
        // ⇒ None (sorts last). Same precedence as `locality_key`.
        let key: Option<i64> = match p.centre {
            Some((cx, cy)) => extents.get(&p.group).map(|e| hilbert_key(cx, cy, e)),
            None => p.date,
        };
        (p.group as i64, key.is_none(), key.unwrap_or(0), p.id.clone())
    });
    idx
}
