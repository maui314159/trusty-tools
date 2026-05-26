//! Yamamoto tree visitor helper.
//!
//! Why: callers that need to inspect every value without producing a flat
//! Vec (e.g. fold/reduce patterns) want a streaming traversal API distinct
//! from `flatten_yamamoto_tree`.
//! What: `yamamoto_traversal` walks the tree and invokes a visitor on every
//! node value.
//! Test: `test_traversal_visits_every_value`.

use crate::yamamoto::tree::YamamotoTree;

/// Depth-first traversal of a yamamoto tree.
///
/// Why: a callback-driven visitor avoids the allocation `flatten_yamamoto_tree`
/// pays when the caller only wants to fold.
/// What: invokes `visitor` once per node value in DFS order.
/// Test: `test_traversal_visits_every_value`.
pub fn yamamoto_traversal<F>(tree: &YamamotoTree, mut visitor: F)
where
    F: FnMut(f64),
{
    fn inner<G: FnMut(f64)>(tree: &YamamotoTree, visitor: &mut G) {
        visitor(tree.value);
        for child in &tree.children {
            inner(child, visitor);
        }
    }
    inner(tree, &mut visitor);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_traversal_visits_every_value() {
        let t = YamamotoTree::branch(
            1.0,
            vec![
                YamamotoTree::leaf(2.0),
                YamamotoTree::branch(3.0, vec![YamamotoTree::leaf(4.0)]),
            ],
        );
        let mut seen = Vec::new();
        yamamoto_traversal(&t, |v| seen.push(v));
        assert_eq!(seen, vec![1.0, 2.0, 3.0, 4.0]);
    }
}
