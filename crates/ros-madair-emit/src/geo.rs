// SPDX-License-Identifier: AGPL-3.0-or-later
//! A8.2: geometry → axis-aligned bounding box, at emit time.
//!
//! This is the emit-only half of the spatial index. The head stores each
//! geometry's bbox (four f64 corners); a query filters on bbox OVERLAP, which is
//! a strict SUPERSET of `sfIntersects` — a candidate set with no false negatives,
//! that the client verifies exactly. We deliberately do NOT reduce a geometry to
//! its centroid: a polygon whose centroid falls outside a query box still
//! overlaps it, and centroid indexing would drop it. Superset, never subset.
//!
//! `geojson` lives here (native emit) and nowhere near the WASM query path — the
//! query side compares four numbers and needs no geometry library.

use geojson::{GeoJson, Geometry, Value};

/// The axis-aligned bounding box `(min_lng, min_lat, max_lng, max_lat)` covering
/// every coordinate in a GeoJSON string. `None` on parse failure or a geometry
/// with no coordinates (empty collection), so the emitter writes nothing.
pub(crate) fn extract_bbox(geojson_str: &str) -> Option<(f64, f64, f64, f64)> {
    let gj: GeoJson = geojson_str.parse().ok()?;
    let mut b = Bbox::default();
    match gj {
        GeoJson::Geometry(g) => fold_geometry(&g, &mut b),
        GeoJson::Feature(f) => {
            if let Some(g) = &f.geometry {
                fold_geometry(g, &mut b);
            }
        }
        GeoJson::FeatureCollection(fc) => {
            for f in &fc.features {
                if let Some(g) = &f.geometry {
                    fold_geometry(g, &mut b);
                }
            }
        }
    }
    b.finish()
}

/// Running min/max over the coordinates seen so far. `seen` distinguishes
/// "no coordinates yet" from a legitimate `(0,0)` corner.
#[derive(Default)]
struct Bbox {
    min_lng: f64,
    min_lat: f64,
    max_lng: f64,
    max_lat: f64,
    seen: bool,
}

impl Bbox {
    fn add(&mut self, c: &[f64]) {
        // GeoJSON positions are [lng, lat, (alt)]; anything shorter is malformed.
        if c.len() < 2 {
            return;
        }
        let (lng, lat) = (c[0], c[1]);
        if !self.seen {
            self.min_lng = lng;
            self.max_lng = lng;
            self.min_lat = lat;
            self.max_lat = lat;
            self.seen = true;
        } else {
            self.min_lng = self.min_lng.min(lng);
            self.max_lng = self.max_lng.max(lng);
            self.min_lat = self.min_lat.min(lat);
            self.max_lat = self.max_lat.max(lat);
        }
    }

    fn finish(self) -> Option<(f64, f64, f64, f64)> {
        self.seen
            .then_some((self.min_lng, self.min_lat, self.max_lng, self.max_lat))
    }
}

fn fold_geometry(geom: &Geometry, b: &mut Bbox) {
    match &geom.value {
        Value::Point(c) => b.add(c),
        Value::MultiPoint(pts) => pts.iter().for_each(|c| b.add(c)),
        Value::LineString(cs) => cs.iter().for_each(|c| b.add(c)),
        Value::MultiLineString(ls) => ls.iter().flatten().for_each(|c| b.add(c)),
        Value::Polygon(rings) => rings.iter().flatten().for_each(|c| b.add(c)),
        Value::MultiPolygon(ps) => ps.iter().flatten().flatten().for_each(|c| b.add(c)),
        Value::GeometryCollection(gs) => gs.iter().for_each(|g| fold_geometry(g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_bbox_is_the_point() {
        let (min_lng, min_lat, max_lng, max_lat) =
            extract_bbox(r#"{"type":"Point","coordinates":[3.0,4.0]}"#).unwrap();
        assert_eq!((min_lng, min_lat, max_lng, max_lat), (3.0, 4.0, 3.0, 4.0));
    }

    #[test]
    fn polygon_bbox_spans_all_vertices() {
        // An L-shaped-ish ring whose centroid sits OUTSIDE the tight corner —
        // the bbox must still be the full extent, [0,10]×[0,10].
        let poly = r#"{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,2],[2,2],[2,10],[0,10],[0,0]]]}"#;
        assert_eq!(extract_bbox(poly).unwrap(), (0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn feature_collection_unions_every_feature() {
        let fc = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[-5,-5]}},
            {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[7,3]}}
        ]}"#;
        assert_eq!(extract_bbox(fc).unwrap(), (-5.0, -5.0, 7.0, 3.0));
    }

    #[test]
    fn garbage_and_empty_yield_none() {
        assert_eq!(extract_bbox("not json"), None);
        assert_eq!(
            extract_bbox(r#"{"type":"FeatureCollection","features":[]}"#),
            None
        );
    }
}
