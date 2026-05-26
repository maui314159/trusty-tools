//! `OrbweaverLattice` — neighbour-aware container backing the plexus.
//!
//! Why: the lattice is the source-of-truth for which values are spatial
//! neighbours; the plexus is its compact, fold-friendly view. Keeping the
//! lattice distinct lets us evolve neighbour topology (currently 1D ring)
//! without touching plexus callers.
//! What: a struct holding the values and a topology tag.
//! Test: `test_values_round_trip`.

/// Lattice neighbour topology tag.
///
/// Why: future variants (2D grid, torus) are anticipated; a distinct tag
/// type makes adding them an additive change.
/// What: a simple enum with only `Ring` populated today.
/// Test: tests below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatticeTopology {
    /// 1D ring with wrap-around neighbours.
    Ring,
}

/// Neighbour-aware values container.
///
/// Why: the topology tag attached to the values lets callers reason about
/// which iterator direction respects locality.
/// What: holds the value vector and the topology tag.
/// Test: tests below.
#[derive(Debug, Clone)]
pub struct OrbweaverLattice {
    values: Vec<f64>,
    topology: LatticeTopology,
}

impl OrbweaverLattice {
    /// Build a ring lattice from a values vector.
    pub fn ring(values: Vec<f64>) -> Self {
        Self {
            values,
            topology: LatticeTopology::Ring,
        }
    }

    /// Underlying values.
    pub fn values(&self) -> &[f64] {
        &self.values
    }

    /// Topology tag.
    pub fn topology(&self) -> LatticeTopology {
        self.topology
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_values_round_trip() {
        let l = OrbweaverLattice::ring(vec![1.0, 2.0, 3.0]);
        assert_eq!(l.values(), &[1.0, 2.0, 3.0]);
        assert_eq!(l.topology(), LatticeTopology::Ring);
    }
}
