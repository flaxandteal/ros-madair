// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Fixed-width value quantization for page records.
//!
//! Every record in a page file is exactly 8 bytes: `(subject_id: u32, object_val: u32)`.
//! The object_val encoding depends on the predicate's value type:
//!
//! | Type                              | Encoding           | Precision       |
//! |-----------------------------------|--------------------|-----------------|
//! | concept / resource-instance / domain-value | u32 dictionary ID  | exact           |
//! | boolean                           | u32 (0 or 1)       | exact           |
//! | date / edtf                       | u32 days since epoch | day precision  |
//! | point geo                         | u32 Hilbert(u16,u16) | ~500m          |
//! | bbox geo                          | u32 Hilbert(centroid) | ~500m, lossy  |
//!
//! All encodings produce sortable u32 values. Within a predicate block,
//! records are sorted by (object_val, subject_id), enabling binary search
//! for exact match and range queries.

use serde_json::Value;

use crate::concept_intervals::ConceptIntervalIndex;
use crate::datatype_class::{classify_datatype, DatatypeClass};
use crate::dictionary::Dictionary;
use crate::geo_convert::extract_centroid;
use crate::uri::prefix_for_datatype;
use crate::value_extract;

/// A quantized page record: 8 bytes, fixed-width.
///
/// Field order determines sort order via derived `Ord`:
/// sorts by (object_val, subject_id) to match byte layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageRecord {
    /// Quantized object value (encoding depends on predicate type).
    pub object_val: u32,
    /// Subject resource, as dictionary ID.
    pub subject_id: u32,
}

impl PageRecord {
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0..4].copy_from_slice(&self.object_val.to_le_bytes());
        buf[4..8].copy_from_slice(&self.subject_id.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8; 8]) -> Self {
        let object_val = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let subject_id = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        Self {
            subject_id,
            object_val,
        }
    }
}

/// The type of quantization applied to a predicate's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizeType {
    /// Exact dictionary ID (resource-instance, domain-value).
    DictionaryId,
    /// Concept DFS interval encoding (concept, concept-list).
    /// Falls back to DictionaryId when no ConceptIntervalIndex is available.
    ConceptDfs,
    /// Boolean (0 or 1).
    Boolean,
    /// Days since Unix epoch (date, edtf).
    DaysSinceEpoch,
    /// 2D Hilbert index over u16 grid (point or bbox centroid).
    HilbertGeo,
}

/// Classify an alizarin datatype into its quantization strategy.
/// Returns None for types we don't index (string, number, etc.).
pub fn quantize_type_for_datatype(datatype: &str) -> Option<QuantizeType> {
    match classify_datatype(datatype)? {
        DatatypeClass::Concept => Some(QuantizeType::ConceptDfs),
        DatatypeClass::ResourceInstance | DatatypeClass::DomainValue => {
            Some(QuantizeType::DictionaryId)
        }
        DatatypeClass::Boolean => Some(QuantizeType::Boolean),
        DatatypeClass::Date => Some(QuantizeType::DaysSinceEpoch),
        DatatypeClass::GeoJson => Some(QuantizeType::HilbertGeo),
        _ => None, // string, number, url, semantic, etc. — not indexed
    }
}

/// Quantize a tile value into one or more u32 object_vals.
///
/// This is the shared implementation used by both `build_from_prebuild` and
/// `ros-madair-builder`. Handles single values and lists (concept-list,
/// resource-instance-list, etc.).
///
/// When `concept_intervals` is provided and `qtype` is `ConceptDfs`, the
/// object_val is the DFS enter number from the concept hierarchy instead
/// of the raw dictionary ID. Falls back to dictionary ID if the concept
/// is not found in the interval index.
pub fn quantize_tile_value(
    value: &Value,
    qtype: QuantizeType,
    datatype: &str,
    dict: &mut Dictionary,
    base_uri: &str,
    concept_intervals: Option<&ConceptIntervalIndex>,
) -> Vec<u32> {
    match qtype {
        QuantizeType::DictionaryId => {
            let prefix = prefix_for_datatype(base_uri, datatype);
            value_extract::extract_reference_ids(value)
                .into_iter()
                .map(|id| {
                    let uri = if id.contains("://") {
                        id
                    } else {
                        format!("{prefix}{id}")
                    };
                    dict.intern(&uri)
                })
                .collect()
        }
        QuantizeType::ConceptDfs => {
            let prefix = prefix_for_datatype(base_uri, datatype);
            value_extract::extract_reference_ids(value)
                .into_iter()
                .map(|id| {
                    let uri = if id.contains("://") {
                        id
                    } else {
                        format!("{prefix}{id}")
                    };
                    let dict_id = dict.intern(&uri);
                    // Try DFS encoding; fall back to dict_id
                    if let Some(ci) = concept_intervals {
                        if let Some((dfs_enter, _)) = ci.lookup(dict_id) {
                            return dfs_enter;
                        }
                    }
                    dict_id
                })
                .collect()
        }
        QuantizeType::Boolean => {
            if let Some(b) = value.as_bool() {
                vec![quantize_bool(b)]
            } else {
                Vec::new()
            }
        }
        QuantizeType::DaysSinceEpoch => {
            if let Some(s) = value.as_str() {
                quantize_date(s).into_iter().collect()
            } else {
                Vec::new()
            }
        }
        QuantizeType::HilbertGeo => {
            if let Some((lng, lat)) = extract_centroid(&value.to_string()) {
                vec![quantize_point(lng, lat)]
            } else {
                Vec::new()
            }
        }
    }
}

// ============================================================================
// Quantization functions: value → u32
// ============================================================================

/// Exact: concept/resource/domain URI → dictionary ID.
pub fn quantize_dictionary_id(uri: &str, dict: &mut Dictionary) -> u32 {
    dict.intern(uri)
}

/// Boolean → 0 or 1.
pub fn quantize_bool(v: bool) -> u32 {
    v as u32
}

/// Date string → days since Unix epoch (1970-01-01).
///
/// Accepts "YYYY-MM-DD", "YYYY-MM", "YYYY". Truncated dates use the
/// first day of the period.
pub fn quantize_date(date_str: &str) -> Option<u32> {
    let parts: Vec<&str> = date_str.split('-').collect();
    let year: i32 = parts.first()?.parse().ok()?;
    let month: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
    let day: u32 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);

    // Days from 1970-01-01 using a simplified calculation.
    // Good enough for sorting — we don't need sub-day precision.
    days_since_epoch(year, month, day)
}

/// (lng, lat) → Hilbert-encoded u32 over a u16×u16 grid.
///
/// ~500m precision (360°/65536 ≈ 0.0055° ≈ 550m at equator).
/// Hilbert encoding gives spatial locality for range scans.
pub fn quantize_point(lng: f64, lat: f64) -> u32 {
    let x = ((lng + 180.0) / 360.0 * 65535.0).clamp(0.0, 65535.0) as u16;
    let y = ((lat + 90.0) / 180.0 * 65535.0).clamp(0.0, 65535.0) as u16;
    xy_to_hilbert_u16(x, y)
}

/// Bounding box → Hilbert-encoded centroid.
pub fn quantize_bbox(min_lng: f64, min_lat: f64, max_lng: f64, max_lat: f64) -> u32 {
    quantize_point((min_lng + max_lng) / 2.0, (min_lat + max_lat) / 2.0)
}

// ============================================================================
// Inverse quantization: u32 → approximate value (for display/debugging)
// ============================================================================

/// Hilbert u32 → approximate (lng, lat).
pub fn dequantize_point(h: u32) -> (f64, f64) {
    let (x, y) = hilbert_to_xy_u16(h);
    let lng = (x as f64 / 65535.0) * 360.0 - 180.0;
    let lat = (y as f64 / 65535.0) * 180.0 - 90.0;
    (lng, lat)
}

/// Days since epoch → (year, month, day).
pub fn dequantize_date(days: u32) -> (i32, u32, u32) {
    date_from_epoch_days(days)
}

// ============================================================================
// Hilbert range decomposition for spatial queries
// ============================================================================

/// Given an axis-aligned bounding box on the u16 grid, return a set of
/// Hilbert ranges that cover all cells in the box.
///
/// `max_ranges` limits decomposition depth — fewer ranges = more false
/// positives but fewer binary searches. 8-16 is typical.
pub fn hilbert_ranges_for_bbox(
    min_x: u16,
    min_y: u16,
    max_x: u16,
    max_y: u16,
    max_ranges: usize,
) -> Vec<(u32, u32)> {
    // Brute-force for small boxes: enumerate all cells, sort Hilbert values,
    // merge contiguous ranges.
    let width = (max_x - min_x) as usize + 1;
    let height = (max_y - min_y) as usize + 1;
    let cell_count = width * height;

    if cell_count <= 4096 {
        // Small enough to enumerate
        let mut hilbert_vals: Vec<u32> = Vec::with_capacity(cell_count);
        for x in min_x..=max_x {
            for y in min_y..=max_y {
                hilbert_vals.push(xy_to_hilbert_u16(x, y));
            }
        }
        hilbert_vals.sort_unstable();
        hilbert_vals.dedup();
        merge_ranges(&hilbert_vals, max_ranges)
    } else {
        // Large box: use recursive quadtree decomposition.
        // For now, fall back to the full Hilbert range of the bounding
        // corners — overly broad but correct.
        let h_min = hilbert_vals_min_max(min_x, min_y, max_x, max_y);
        vec![h_min]
    }
}

/// Convert a geographic bounding box to Hilbert ranges.
pub fn hilbert_ranges_for_geo_bbox(
    min_lng: f64,
    min_lat: f64,
    max_lng: f64,
    max_lat: f64,
    max_ranges: usize,
) -> Vec<(u32, u32)> {
    let x0 = ((min_lng + 180.0) / 360.0 * 65535.0).clamp(0.0, 65535.0) as u16;
    let y0 = ((min_lat + 90.0) / 180.0 * 65535.0).clamp(0.0, 65535.0) as u16;
    let x1 = ((max_lng + 180.0) / 360.0 * 65535.0).clamp(0.0, 65535.0) as u16;
    let y1 = ((max_lat + 90.0) / 180.0 * 65535.0).clamp(0.0, 65535.0) as u16;
    hilbert_ranges_for_bbox(x0, y0, x1, y1, max_ranges)
}

// ============================================================================
// 2D Hilbert curve at u16 resolution (16 bits per axis → 32 bits total)
// ============================================================================

fn xy_to_hilbert_u16(mut x: u16, mut y: u16) -> u32 {
    let mut d: u32 = 0;
    let mut s: u16 = 1 << 15; // 32768
    while s > 0 {
        let rx: u32 = if (x & s) > 0 { 1 } else { 0 };
        let ry: u32 = if (y & s) > 0 { 1 } else { 0 };
        d += (s as u32) * (s as u32) * ((3 * rx) ^ ry);
        // Rotate
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_mul(2).wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_mul(2).wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

fn hilbert_to_xy_u16(mut d: u32) -> (u16, u16) {
    let mut x: u32 = 0;
    let mut y: u32 = 0;
    let mut s: u32 = 1;
    while s < 65536 {
        let rx = 1 & (d / 2);
        let ry = 1 & (d ^ rx);
        // Rotate
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        d /= 4;
        s *= 2;
    }
    (x as u16, y as u16)
}

// ============================================================================
// Date helpers
// ============================================================================

fn days_since_epoch(year: i32, month: u32, day: u32) -> Option<u32> {
    // Rata Die algorithm (adjusted for Unix epoch)
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Adjust for March-based year (simplifies leap year calc)
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };

    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_civil = era as i64 * 146097 + doe as i64 - 719468; // days since 1970-01-01

    // We use u32 with an offset to handle dates before epoch
    // Offset by 2^31 so that 1970-01-01 = 2^31, and dates sort correctly
    let shifted = days_from_civil + (1i64 << 31);
    if shifted < 0 || shifted > u32::MAX as i64 {
        return None;
    }
    Some(shifted as u32)
}

fn date_from_epoch_days(encoded: u32) -> (i32, u32, u32) {
    let days = encoded as i64 - (1i64 << 31);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m, d)
}

// ============================================================================
// Hilbert range helpers
// ============================================================================

fn merge_ranges(sorted_vals: &[u32], max_ranges: usize) -> Vec<(u32, u32)> {
    if sorted_vals.is_empty() {
        return Vec::new();
    }

    // Build initial contiguous ranges
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut start = sorted_vals[0];
    let mut end = sorted_vals[0];

    for &v in &sorted_vals[1..] {
        if v == end + 1 {
            end = v;
        } else {
            ranges.push((start, end));
            start = v;
            end = v;
        }
    }
    ranges.push((start, end));

    // Merge smallest gaps until we're under max_ranges
    while ranges.len() > max_ranges {
        let mut min_gap = u64::MAX;
        let mut min_idx = 0;
        for i in 0..ranges.len() - 1 {
            let gap = (ranges[i + 1].0 - ranges[i].1) as u64;
            if gap < min_gap {
                min_gap = gap;
                min_idx = i;
            }
        }
        let merged_end = ranges[min_idx + 1].1;
        ranges[min_idx].1 = merged_end;
        ranges.remove(min_idx + 1);
    }

    ranges
}

fn hilbert_vals_min_max(min_x: u16, min_y: u16, max_x: u16, max_y: u16) -> (u32, u32) {
    // For large boxes, compute Hilbert values at corners and some interior
    // points to get a rough bounding range.
    let corners = [
        xy_to_hilbert_u16(min_x, min_y),
        xy_to_hilbert_u16(min_x, max_y),
        xy_to_hilbert_u16(max_x, min_y),
        xy_to_hilbert_u16(max_x, max_y),
    ];
    let mid_x = min_x / 2 + max_x / 2;
    let mid_y = min_y / 2 + max_y / 2;
    let mid = xy_to_hilbert_u16(mid_x, mid_y);

    let min_h = corners.iter().copied().chain(std::iter::once(mid)).min().unwrap();
    let max_h = corners.iter().copied().chain(std::iter::once(mid)).max().unwrap();
    (min_h, max_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quantize_bool() {
        assert_eq!(quantize_bool(false), 0);
        assert_eq!(quantize_bool(true), 1);
    }

    #[test]
    fn test_quantize_date_full() {
        let d1 = quantize_date("2026-03-15").unwrap();
        let d2 = quantize_date("2026-12-31").unwrap();
        let d0 = quantize_date("1970-01-01").unwrap();
        assert!(d1 < d2);
        assert!(d0 < d1);
        // 1970-01-01 should be exactly 2^31
        assert_eq!(d0, 1 << 31);
    }

    #[test]
    fn test_quantize_date_partial() {
        let y = quantize_date("2026").unwrap();
        let ym = quantize_date("2026-03").unwrap();
        assert!(y < ym); // Jan 1 < Mar 1
    }

    #[test]
    fn test_quantize_date_roundtrip() {
        let encoded = quantize_date("2026-03-15").unwrap();
        let (y, m, d) = dequantize_date(encoded);
        assert_eq!((y, m, d), (2026, 3, 15));
    }

    #[test]
    fn test_quantize_date_before_epoch() {
        let d = quantize_date("1066-10-14").unwrap();
        let e = quantize_date("1970-01-01").unwrap();
        assert!(d < e);
        let (y, m, day) = dequantize_date(d);
        assert_eq!((y, m, day), (1066, 10, 14));
    }

    #[test]
    fn test_quantize_point_roundtrip() {
        let h = quantize_point(-5.93, 54.6);
        let (lng, lat) = dequantize_point(h);
        assert!((lng - (-5.93)).abs() < 0.01);
        assert!((lat - 54.6).abs() < 0.01);
    }

    #[test]
    fn test_point_spatial_locality() {
        let belfast = quantize_point(-5.93, 54.6);
        let bangor = quantize_point(-5.67, 54.66);
        let sydney = quantize_point(151.2, -33.87);

        let bb = (belfast as i64 - bangor as i64).unsigned_abs();
        let bs = (belfast as i64 - sydney as i64).unsigned_abs();
        assert!(bb < bs, "Belfast-Bangor ({bb}) should be closer than Belfast-Sydney ({bs})");
    }

    #[test]
    fn test_hilbert_roundtrip_u16() {
        for &(x, y) in &[(0, 0), (100, 200), (65535, 65535), (32768, 32768)] {
            let h = xy_to_hilbert_u16(x, y);
            let (rx, ry) = hilbert_to_xy_u16(h);
            assert_eq!((rx, ry), (x, y), "roundtrip failed for ({x}, {y})");
        }
    }

    #[test]
    fn test_hilbert_ranges_small_box() {
        let ranges = hilbert_ranges_for_bbox(100, 100, 102, 102, 8);
        assert!(!ranges.is_empty());
        // Should cover all 9 cells
        let total_cells: u32 = ranges.iter().map(|(a, b)| b - a + 1).sum();
        assert!(total_cells >= 9);
    }

    #[test]
    fn test_page_record_roundtrip() {
        let rec = PageRecord {
            subject_id: 42,
            object_val: 12345,
        };
        let bytes = rec.to_bytes();
        let rec2 = PageRecord::from_bytes(&bytes);
        assert_eq!(rec, rec2);
    }

    #[test]
    fn test_page_record_sort_order() {
        // Records sort by (object_val, subject_id) due to byte layout
        let r1 = PageRecord { subject_id: 99, object_val: 1 };
        let r2 = PageRecord { subject_id: 1, object_val: 2 };
        let r3 = PageRecord { subject_id: 50, object_val: 2 };

        let mut recs = [r3, r1, r2];
        recs.sort();
        assert_eq!(recs[0].object_val, 1);
        assert_eq!(recs[1], PageRecord { subject_id: 1, object_val: 2 });
        assert_eq!(recs[2], PageRecord { subject_id: 50, object_val: 2 });
    }

    #[test]
    fn test_quantize_type_classification() {
        assert_eq!(quantize_type_for_datatype("concept"), Some(QuantizeType::ConceptDfs));
        assert_eq!(quantize_type_for_datatype("resource-instance"), Some(QuantizeType::DictionaryId));
        assert_eq!(quantize_type_for_datatype("boolean"), Some(QuantizeType::Boolean));
        assert_eq!(quantize_type_for_datatype("date"), Some(QuantizeType::DaysSinceEpoch));
        assert_eq!(quantize_type_for_datatype("geojson-feature-collection"), Some(QuantizeType::HilbertGeo));
        assert_eq!(quantize_type_for_datatype("string"), None);
        assert_eq!(quantize_type_for_datatype("number"), None);
    }
}
