//! `YamamotoTree` — clustered tree of post-transform contributions, with a
//! flatten helper for the wolfram registry handoff.
//!
//! Why: the wolfram registry consumes a flat slice but produces best results
//! when the slice preserves local cluster order; building the tree explicitly
//! and then flattening preserves that order without baking the clustering
//! algorithm into wolfram itself.
//! What: a tree with `value` + `children`, plus `flatten_yamamoto_tree` that
//! produces a depth-first ordering with fan-out clamped to `YAMAMOTO_FANOUT_CAP`.
//! Test: `test_flatten_preserves_order`, `test_fanout_cap_truncates`.

use crate::constants::YAMAMOTO_FANOUT_CAP;

/// A node in the yamamoto cluster tree.
///
/// Why: clusters can have arbitrary nesting depth depending on the input
/// shape; representing them as a tree (rather than flat with parent
/// pointers) keeps recursion straightforward.
/// What: holds an f64 value and a vector of children.
/// Test: tests below.
#[derive(Debug, Clone, PartialEq)]
pub struct YamamotoTree {
    pub value: f64,
    pub children: Vec<YamamotoTree>,
}

impl YamamotoTree {
    /// Build a leaf with the given value.
    pub fn leaf(value: f64) -> Self {
        Self {
            value,
            children: Vec::new(),
        }
    }

    /// Build an internal node with the given value and children.
    pub fn branch(value: f64, children: Vec<YamamotoTree>) -> Self {
        Self { value, children }
    }
}

/// Flatten a yamamoto tree into a depth-first vector of values.
///
/// Why: the wolfram registry needs a flat slice; depth-first preserves the
/// "near things go near each other" property the tree captures.
/// What: recursively visits children, clamping per-node fan-out to
/// `YAMAMOTO_FANOUT_CAP` to avoid unbounded queues on malformed inputs.
/// Test: `test_flatten_preserves_order`, `test_fanout_cap_truncates`.
pub fn flatten_yamamoto_tree(tree: &YamamotoTree) -> Vec<f64> {
    let mut out = Vec::new();
    flatten_inner(tree, &mut out);
    out
}

fn flatten_inner(tree: &YamamotoTree, out: &mut Vec<f64>) {
    out.push(tree.value);
    for child in tree.children.iter().take(YAMAMOTO_FANOUT_CAP) {
        flatten_inner(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flatten_preserves_order() {
        let t = YamamotoTree::branch(
            1.0,
            vec![
                YamamotoTree::leaf(2.0),
                YamamotoTree::branch(3.0, vec![YamamotoTree::leaf(4.0)]),
            ],
        );
        assert_eq!(flatten_yamamoto_tree(&t), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn test_fanout_cap_truncates() {
        let leaves: Vec<YamamotoTree> =
            (0..(YAMAMOTO_FANOUT_CAP + 8)).map(|i| YamamotoTree::leaf(i as f64)).collect();
        let t = YamamotoTree::branch(-1.0, leaves);
        let flat = flatten_yamamoto_tree(&t);
        assert_eq!(flat.len(), YAMAMOTO_FANOUT_CAP + 1);
    }
}
