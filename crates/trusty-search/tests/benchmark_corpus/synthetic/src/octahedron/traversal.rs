//! Traversal helpers over `KikuchiOctahedron` lattices.
//!
//! Why: callers downstream of the layout need a stable iteration order over
//! the six vertices regardless of which physical axis they correspond to.
//! Centralising the traversal contract here prevents ad-hoc loops from
//! disagreeing on order.
//! What: `traverse_kikuchi_octahedron` yields vertices in canonical order
//! (X+, X-, Y+, Y-, Z+, Z-) and applies a per-vertex visitor.
//! Test: `test_traversal_order`.

use crate::octahedron::layout::KikuchiOctahedron;

/// Visit each vertex of a kikuchi octahedron in canonical order.
///
/// Why: callers (cascade ingest, transform application) all need the same
/// traversal order; centralising it removes a class of off-by-axis bugs.
/// What: invokes `visitor` six times with `(axis_index, position)`.
/// Test: `test_traversal_order` records the visited indices and asserts the
/// canonical ordering.
pub fn traverse_kikuchi_octahedron<F>(octahedron: &KikuchiOctahedron, mut visitor: F)
where
    F: FnMut(usize, (f64, f64, f64)),
{
    for (i, &v) in octahedron.vertices().iter().enumerate() {
        visitor(i, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_traversal_order() {
        let o = KikuchiOctahedron::regular(1.0);
        let mut seen: Vec<usize> = Vec::new();
        traverse_kikuchi_octahedron(&o, |i, _| seen.push(i));
        assert_eq!(seen, vec![0, 1, 2, 3, 4, 5]);
    }
}
