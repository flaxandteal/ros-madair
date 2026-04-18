// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! GeoJSON → WKT conversion for RDF geo:asWKT literals.

use geojson::GeoJson;
use std::fmt::Write;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum GeoConvertError {
    #[error("Failed to parse GeoJSON: {0}")]
    ParseError(String),
    #[error("Empty geometry collection")]
    EmptyCollection,
}

/// Convert a GeoJSON string to WKT.
///
/// Handles FeatureCollection, Feature, and bare Geometry objects.
/// For FeatureCollections with multiple features, produces a GEOMETRYCOLLECTION.
pub fn geojson_to_wkt(geojson_str: &str) -> Result<String, GeoConvertError> {
    let geojson: GeoJson = geojson_str
        .parse()
        .map_err(|e: geojson::Error| GeoConvertError::ParseError(e.to_string()))?;

    match geojson {
        GeoJson::FeatureCollection(fc) => {
            let wkts: Vec<String> = fc
                .features
                .iter()
                .filter_map(|f| f.geometry.as_ref())
                .map(geometry_to_wkt)
                .collect::<Result<Vec<_>, _>>()?;

            match wkts.len() {
                0 => Err(GeoConvertError::EmptyCollection),
                1 => Ok(wkts.into_iter().next().unwrap()),
                _ => Ok(format!("GEOMETRYCOLLECTION({})", wkts.join(", "))),
            }
        }
        GeoJson::Feature(f) => {
            let geom = f
                .geometry
                .as_ref()
                .ok_or_else(|| GeoConvertError::ParseError("Feature has no geometry".into()))?;
            geometry_to_wkt(geom)
        }
        GeoJson::Geometry(g) => geometry_to_wkt(&g),
    }
}

/// Extract centroid (lng, lat) from a GeoJSON string.
///
/// For points, returns the point itself. For other geometries, returns
/// the bounding box centroid. Returns None on parse failure.
pub fn extract_centroid(geojson_str: &str) -> Option<(f64, f64)> {
    let geojson: GeoJson = geojson_str.parse().ok()?;
    match geojson {
        GeoJson::Geometry(ref g) => centroid_from_geometry(g),
        GeoJson::Feature(ref f) => f.geometry.as_ref().and_then(centroid_from_geometry),
        GeoJson::FeatureCollection(ref fc) => {
            let mut lng_sum = 0.0;
            let mut lat_sum = 0.0;
            let mut count = 0;
            for f in &fc.features {
                if let Some(g) = &f.geometry {
                    if let Some((lng, lat)) = centroid_from_geometry(g) {
                        lng_sum += lng;
                        lat_sum += lat;
                        count += 1;
                    }
                }
            }
            if count > 0 {
                Some((lng_sum / count as f64, lat_sum / count as f64))
            } else {
                None
            }
        }
    }
}

fn centroid_from_geometry(geom: &geojson::Geometry) -> Option<(f64, f64)> {
    use geojson::Value;
    match &geom.value {
        Value::Point(coords) => Some((coords[0], coords[1])),
        Value::MultiPoint(points) => bbox_centroid_coords(points.iter().map(|c| c.as_slice())),
        Value::LineString(coords) => bbox_centroid_coords(coords.iter().map(|c| c.as_slice())),
        Value::MultiLineString(lines) => {
            bbox_centroid_coords(lines.iter().flatten().map(|c| c.as_slice()))
        }
        Value::Polygon(rings) => {
            bbox_centroid_coords(rings.iter().flatten().map(|c| c.as_slice()))
        }
        Value::MultiPolygon(polys) => {
            bbox_centroid_coords(polys.iter().flatten().flatten().map(|c| c.as_slice()))
        }
        Value::GeometryCollection(geoms) => {
            let mut lng_sum = 0.0;
            let mut lat_sum = 0.0;
            let mut count = 0;
            for g in geoms {
                if let Some((lng, lat)) = centroid_from_geometry(g) {
                    lng_sum += lng;
                    lat_sum += lat;
                    count += 1;
                }
            }
            if count > 0 {
                Some((lng_sum / count as f64, lat_sum / count as f64))
            } else {
                None
            }
        }
    }
}

fn bbox_centroid_coords<'a>(coords: impl Iterator<Item = &'a [f64]>) -> Option<(f64, f64)> {
    let mut min_lng = f64::MAX;
    let mut max_lng = f64::MIN;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut has_any = false;

    for c in coords {
        if c.len() >= 2 {
            has_any = true;
            min_lng = min_lng.min(c[0]);
            max_lng = max_lng.max(c[0]);
            min_lat = min_lat.min(c[1]);
            max_lat = max_lat.max(c[1]);
        }
    }

    if has_any {
        Some(((min_lng + max_lng) / 2.0, (min_lat + max_lat) / 2.0))
    } else {
        None
    }
}

fn geometry_to_wkt(geom: &geojson::Geometry) -> Result<String, GeoConvertError> {
    use geojson::Value;

    match &geom.value {
        Value::Point(coords) => Ok(format!("POINT({} {})", coords[0], coords[1])),
        Value::MultiPoint(points) => {
            let pts: Vec<String> = points
                .iter()
                .map(|c| format!("({} {})", c[0], c[1]))
                .collect();
            Ok(format!("MULTIPOINT({})", pts.join(", ")))
        }
        Value::LineString(coords) => {
            let pts = coords_to_wkt_list(coords);
            Ok(format!("LINESTRING({})", pts))
        }
        Value::MultiLineString(lines) => {
            let parts: Vec<String> = lines
                .iter()
                .map(|line| format!("({})", coords_to_wkt_list(line)))
                .collect();
            Ok(format!("MULTILINESTRING({})", parts.join(", ")))
        }
        Value::Polygon(rings) => {
            let parts: Vec<String> = rings
                .iter()
                .map(|ring| format!("({})", coords_to_wkt_list(ring)))
                .collect();
            Ok(format!("POLYGON({})", parts.join(", ")))
        }
        Value::MultiPolygon(polygons) => {
            let polys: Vec<String> = polygons
                .iter()
                .map(|rings| {
                    let parts: Vec<String> = rings
                        .iter()
                        .map(|ring| format!("({})", coords_to_wkt_list(ring)))
                        .collect();
                    format!("({})", parts.join(", "))
                })
                .collect();
            Ok(format!("MULTIPOLYGON({})", polys.join(", ")))
        }
        Value::GeometryCollection(geoms) => {
            let wkts: Vec<String> = geoms
                .iter()
                .map(geometry_to_wkt)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("GEOMETRYCOLLECTION({})", wkts.join(", ")))
        }
    }
}

fn coords_to_wkt_list(coords: &[Vec<f64>]) -> String {
    let mut buf = String::new();
    for (i, coord) in coords.iter().enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        let _ = write!(buf, "{} {}", coord[0], coord[1]);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_point() {
        let geojson = r#"{"type": "Point", "coordinates": [-5.93, 54.60]}"#;
        let wkt = geojson_to_wkt(geojson).unwrap();
        assert_eq!(wkt, "POINT(-5.93 54.6)");
    }

    #[test]
    fn test_polygon() {
        let geojson = r#"{
            "type": "Polygon",
            "coordinates": [[[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]]
        }"#;
        let wkt = geojson_to_wkt(geojson).unwrap();
        assert_eq!(wkt, "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))");
    }

    #[test]
    fn test_extract_centroid_point() {
        let geojson = r#"{"type": "Point", "coordinates": [-5.93, 54.60]}"#;
        let c = extract_centroid(geojson).unwrap();
        assert!((c.0 - (-5.93)).abs() < 0.001);
        assert!((c.1 - 54.60).abs() < 0.001);
    }

    #[test]
    fn test_extract_centroid_polygon() {
        let geojson = r#"{
            "type": "Polygon",
            "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]]
        }"#;
        let c = extract_centroid(geojson).unwrap();
        assert!((c.0 - 1.0).abs() < 0.001);
        assert!((c.1 - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_feature_collection_single() {
        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [-5.93, 54.60]},
                "properties": {}
            }]
        }"#;
        let wkt = geojson_to_wkt(geojson).unwrap();
        assert_eq!(wkt, "POINT(-5.93 54.6)");
    }
}
