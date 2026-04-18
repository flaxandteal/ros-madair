// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Flax & Teal Limited

//! Hilbert curve encoding for page assignment.
//!
//! Simplified from madder-core's 5D variant. RosMadair uses a 3D Hilbert curve
//! over (centroid_x, centroid_y, type_bucket) to sub-sort resources within each
//! graph_id group before slicing into pages.

use std::collections::HashSet;

/// Resolution for the 3D page-assignment curve: 10 bits per axis → 1024³.
const BITS_3D: u32 = 10;
const N_3D: u64 = 1 << BITS_3D;

/// Encode an N-dimensional coordinate into a Hilbert index.
///
/// Each element of `coords` must be in [0, 2^bits). Returns a u64 formed by
/// interleaving the transposed coordinate bits.
pub fn hilbert_index_nd(coords: &[u64], bits: u32) -> u64 {
    let mut axes: Vec<u64> = coords.to_vec();
    axes_to_transpose(&mut axes, bits);
    interleave_bits(&axes, bits)
}

/// 3D Hilbert index for page assignment: (lng, lat, type_bucket).
///
/// - `lng` in [-180, 180], `lat` in [-90, 90]
/// - `type_bucket` in [0.0, 1.0] — a hashed bucket for non-spatial grouping
///
/// Returns a u64 encoding all three dimensions at 10-bit resolution.
pub fn hilbert_index_3d(lng: f64, lat: f64, type_bucket: f64) -> u64 {
    let max = (N_3D - 1) as f64;
    let x = ((lng + 180.0) / 360.0 * N_3D as f64).clamp(0.0, max) as u64;
    let y = ((lat + 90.0) / 180.0 * N_3D as f64).clamp(0.0, max) as u64;
    let z = (type_bucket * N_3D as f64).clamp(0.0, max) as u64;
    hilbert_index_nd(&[x, y, z], BITS_3D)
}

/// Map a concept set to a scalar in [0.0, 1.0] for the type_bucket axis.
///
/// Simple version: hashes concept URIs to produce a deterministic coordinate.
pub fn concept_coordinate(concept_set: &HashSet<String>) -> f64 {
    if concept_set.is_empty() {
        return 0.5;
    }
    let sum: f64 = concept_set.iter().map(|c| stable_hash_f64(c)).sum();
    sum / concept_set.len() as f64
}

/// Deterministic hash of a string to [0.0, 1.0).
/// Uses FNV-1a for speed and determinism (no random seed).
fn stable_hash_f64(s: &str) -> f64 {
    let mut h: u64 = 0xcbf29ce484222325; // FNV offset basis
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3); // FNV prime
    }
    (h >> 1) as f64 / (u64::MAX >> 1) as f64
}

/// Skilling's algorithm: convert N-dimensional coordinates to Hilbert
/// transpose form (in-place).
fn axes_to_transpose(axes: &mut [u64], bits: u32) {
    let n = axes.len();
    if n == 0 || bits == 0 {
        return;
    }

    let m = 1u64 << (bits - 1);

    // Inverse undo: convert axes to transpose
    let mut q = m;
    while q > 1 {
        let p = q - 1;
        for i in 0..n {
            if axes[i] & q != 0 {
                axes[0] ^= p;
            } else {
                let t = (axes[0] ^ axes[i]) & p;
                axes[0] ^= t;
                axes[i] ^= t;
            }
        }
        q >>= 1;
    }

    // Gray encode
    for i in 1..n {
        axes[i] ^= axes[i - 1];
    }
    let mut t = 0u64;
    let mut q = m;
    while q > 1 {
        if axes[n - 1] & q != 0 {
            t ^= q - 1;
        }
        q >>= 1;
    }
    for axis in axes.iter_mut() {
        *axis ^= t;
    }
}

/// Interleave bits from each axis into a single u64.
fn interleave_bits(axes: &[u64], bits: u32) -> u64 {
    let n = axes.len();
    let mut result: u64 = 0;
    for bit in (0..bits).rev() {
        for (dim, axis) in axes.iter().enumerate() {
            let b = (axis >> bit) & 1;
            let shift = (bits - 1 - bit) as usize * n + dim;
            if shift < 64 {
                result |= b << (63 - shift);
            }
        }
    }
    let total_bits = bits as usize * n;
    if total_bits < 64 {
        result >>= 64 - total_bits;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_3d_valid_range() {
        let max_val = N_3D * N_3D * N_3D;
        assert!(hilbert_index_3d(0.0, 0.0, 0.5) < max_val);
        assert!(hilbert_index_3d(-180.0, -90.0, 0.0) < max_val);
        assert!(hilbert_index_3d(180.0, 90.0, 1.0) < max_val);
    }

    #[test]
    fn test_3d_geo_locality() {
        let a = hilbert_index_3d(-0.001, 51.477, 0.5);
        let b = hilbert_index_3d(0.001, 51.478, 0.5);
        let c = hilbert_index_3d(120.0, -30.0, 0.5);
        let ab = (a as i64 - b as i64).unsigned_abs();
        let ac = (a as i64 - c as i64).unsigned_abs();
        assert!(ab < ac, "geo locality: ab={ab}, ac={ac}");
    }

    #[test]
    fn test_concept_coordinate_determinism() {
        let mut set1 = HashSet::new();
        set1.insert("castle".to_string());
        set1.insert("fort".to_string());

        let mut set2 = HashSet::new();
        set2.insert("castle".to_string());
        set2.insert("fort".to_string());

        assert_eq!(concept_coordinate(&set1), concept_coordinate(&set2));
    }

    #[test]
    fn test_concept_coordinate_empty() {
        let empty = HashSet::new();
        assert_eq!(concept_coordinate(&empty), 0.5);
    }

    #[test]
    fn test_nd_trivial() {
        assert_eq!(hilbert_index_nd(&[0], 4), 0);
        assert_eq!(hilbert_index_nd(&[15], 4), 15);
    }
}
