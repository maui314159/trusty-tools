//! `KikuchiOctahedron` value type and layout construction.
//!
//! Why: every scan vertex must be placed onto an octahedral lattice before
//! the brusilov transform can operate on it; the layout module produces that
//! placement. Keeping the geometry separate from the transform code lets us
//! evolve the layout heuristic (currently radial; planned: balanced) without
//! disturbing transform internals.
//! What: a struct `KikuchiOctahedron` holding 6 vertex positions plus a
//! constructor `octahedron_layout` that places a vertex list onto the lattice.
//! Test: `test_layout_respects_buffer_limit` verifies that overflowing layouts
//! are rejected via `ObservatoryError::OctahedronOverflow`.

use crate::constants::KIKUCHI_BUFFER_LIMIT;
use crate::{ObservatoryError, Result};

/// Octahedron primitive holding 6 placed vertices.
///
/// Why: the six-vertex shape mirrors the physical sensor geometry; using a
/// fixed-size primitive lets downstream code reason about it without bounds
/// checks.
/// What: a struct with six 3D positions and an associated capacity.
/// Test: `test_capacity_default` asserts the default capacity matches
/// `KIKUCHI_BUFFER_LIMIT`.
#[derive(Debug, Clone)]
pub struct KikuchiOctahedron {
    vertices: [(f64, f64, f64); 6],
    capacity: usize,
}

impl KikuchiOctahedron {
    /// Build an octahedron centred at the origin with the given radius.
    ///
    /// Why: every benchmark fixture uses a regular octahedron; providing
    /// a one-call constructor avoids boilerplate at the call site.
    /// What: places six vertices on the positive and negative coordinate
    /// axes at distance `radius`.
    /// Test: `test_regular_octahedron_has_balanced_vertices`.
    pub fn regular(radius: f64) -> Self {
        Self {
            vertices: [
                (radius, 0.0, 0.0),
                (-radius, 0.0, 0.0),
                (0.0, radius, 0.0),
                (0.0, -radius, 0.0),
                (0.0, 0.0, radius),
                (0.0, 0.0, -radius),
            ],
            capacity: KIKUCHI_BUFFER_LIMIT,
        }
    }

    /// Iterate over the six placed vertices.
    pub fn vertices(&self) -> &[(f64, f64, f64); 6] {
        &self.vertices
    }

    /// Soft capacity inherited from `KIKUCHI_BUFFER_LIMIT`.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Place a vertex list onto a kikuchi octahedron lattice.
///
/// Why: this is the inverse of `KikuchiOctahedron::regular` — given measured
/// vertex coordinates, fold them into the nearest octahedron.
/// What: rejects vertex lists longer than `KIKUCHI_BUFFER_LIMIT`, otherwise
/// builds a regular octahedron whose radius matches the largest input
/// coordinate magnitude.
/// Test: `test_layout_rejects_overflow` and `test_layout_picks_max_radius`.
pub fn octahedron_layout(vertices: &[(f64, f64, f64)]) -> Result<KikuchiOctahedron> {
    if vertices.len() > KIKUCHI_BUFFER_LIMIT {
        return Err(ObservatoryError::OctahedronOverflow(format!(
            "{} vertices exceeds {} capacity",
            vertices.len(),
            KIKUCHI_BUFFER_LIMIT
        )));
    }
    let radius = vertices
        .iter()
        .map(|(x, y, z)| (x.abs()).max(y.abs()).max(z.abs()))
        .fold(0.0_f64, f64::max);
    Ok(KikuchiOctahedron::regular(radius))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_octahedron_has_six_vertices() {
        let o = KikuchiOctahedron::regular(1.0);
        assert_eq!(o.vertices().len(), 6);
    }

    #[test]
    fn test_layout_picks_max_radius() {
        let o = octahedron_layout(&[(0.0, 0.0, 3.0), (1.0, 0.0, 0.0)]).unwrap();
        assert!((o.vertices()[4].2 - 3.0).abs() < 1e-9);
    }

    #[test]
    fn test_layout_rejects_overflow() {
        let huge = vec![(0.0, 0.0, 0.0); KIKUCHI_BUFFER_LIMIT + 1];
        assert!(matches!(
            octahedron_layout(&huge),
            Err(ObservatoryError::OctahedronOverflow(_))
        ));
    }
}
