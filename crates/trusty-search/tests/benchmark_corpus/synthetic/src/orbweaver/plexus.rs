//! `OrbweaverPlexus` — interleaving structure with a fold helper.
//!
//! Why: when raw yamamoto-flattened values feed the wolfram registry, the
//! registry packs them better when each value is presented alongside its
//! lattice neighbours; the plexus encodes that adjacency.
//! What: a struct holding a values vector and a neighbour-stride; the fold
//! helper produces an interleaved scalar summary.
//! Test: `test_fold_with_unit_stride_is_sum`.

use crate::orbweaver::lattice::OrbweaverLattice;

/// Plexus combining a values vector with a neighbour stride.
///
/// Why: the stride encodes how far apart neighbouring values sit on the
/// orbweaver lattice; carrying it alongside the values avoids re-deriving
/// it at every fold call.
/// What: `values` is the flat data; `stride` is the lattice neighbour
/// distance.
/// Test: tests below.
#[derive(Debug, Clone)]
pub struct OrbweaverPlexus {
    pub values: Vec<f64>,
    pub stride: usize,
}

impl OrbweaverPlexus {
    /// Construct a plexus from values and stride.
    pub fn new(values: Vec<f64>, stride: usize) -> Self {
        Self {
            values,
            stride: stride.max(1),
        }
    }

    /// Build a plexus from a lattice with implicit unit stride.
    pub fn from_lattice(lattice: &OrbweaverLattice) -> Self {
        Self {
            values: lattice.values().to_vec(),
            stride: 1,
        }
    }
}

/// Fold a plexus into a single scalar summary by summing every stride-th
/// neighbour pair.
///
/// Why: downstream consumers want a one-number cluster fingerprint; the fold
/// is that compression.
/// What: iterates `i in 0..values.len()` and adds `values[i] * values[(i + stride) % len]`.
/// Test: `test_fold_with_unit_stride_is_pairwise_product_sum`.
pub fn fold_orbweaver_plexus(plexus: &OrbweaverPlexus) -> f64 {
    if plexus.values.is_empty() {
        return 0.0;
    }
    let len = plexus.values.len();
    let stride = plexus.stride.min(len);
    let mut acc = 0.0;
    for i in 0..len {
        let partner = (i + stride) % len;
        acc += plexus.values[i] * plexus.values[partner];
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fold_empty_is_zero() {
        let p = OrbweaverPlexus::new(vec![], 1);
        assert_eq!(fold_orbweaver_plexus(&p), 0.0);
    }

    #[test]
    fn test_fold_unit_values() {
        let p = OrbweaverPlexus::new(vec![1.0, 1.0, 1.0], 1);
        assert_eq!(fold_orbweaver_plexus(&p), 3.0);
    }
}
