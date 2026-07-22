//! Pure layout-template topology (x-c4d4). A named shape plus a slot count
//! becomes a pane tree of slot-indexed leaves - no side effects, no server
//! state, exhaustively unit-testable in isolation. The server (`server.rs`)
//! substitutes real `PaneId`s for the slot-index leaves and validates fit.
//!
//! The vocabulary is fixed by the epic (a general layout DSL is out of scope):
//! `main-left`/`main-top` (variadic, k >= 2), `row-thirds`/`col-thirds`
//! (k == 3), `grid-2x2` (k == 4). Every branch splits its children evenly,
//! matching the existing 0.5/0.5 split default; per-slot size hints are a
//! deliberate non-goal for v1 (a human resizes with the draggable dividers).

use crate::proto::TemplateName;
use crate::tree::{Axis, Node};

/// `topology` refused before producing a tree. Fit (a slot too small for the
/// viewport) is a geometric check the server runs on the realized tree, not a
/// topology failure, so it is not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The slot count does not satisfy the template. `want` is the exact arity
    /// for a fixed template, or the minimum (2) for a variadic one.
    #[error("template arity: want {want} (variadic={variadic}), got {got}")]
    Arity {
        want: usize,
        got: usize,
        /// True for `main-left`/`main-top` (want is a minimum, not exact).
        variadic: bool,
    },
}

/// Even ratios for `n` children that sum to exactly 1.0 (the last absorbs the
/// float remainder), so `check_invariants`'s 1e-4 sum tolerance is always met
/// even for thirds.
fn even(n: usize) -> Vec<f32> {
    debug_assert!(n >= 1);
    let each = 1.0 / n as f32;
    let mut r = vec![each; n];
    let rest: f32 = r[..n - 1].iter().sum();
    r[n - 1] = 1.0 - rest;
    r
}

/// A branch of consecutive slot-index leaves `[first, last)` along `axis`,
/// evenly weighted. Caller guarantees `last - first >= 2`.
fn leaf_branch(axis: Axis, first: usize, last: usize) -> Node {
    let ratios = even(last - first);
    Node::Branch {
        axis,
        children: ratios
            .into_iter()
            .zip(first..last)
            .map(|(r, i)| (r, Node::Leaf(i as u64)))
            .collect(),
    }
}

/// The "rest" of a `main-*` template: slots `[1, k)` stacked along `axis`,
/// collapsed to a bare leaf when only one slot remains (a branch must have
/// >= 2 children).
fn rest(axis: Axis, k: usize) -> Node {
    if k - 1 == 1 {
        Node::Leaf(1)
    } else {
        leaf_branch(axis, 1, k)
    }
}

/// Turn a template name + slot count into a pure pane tree whose leaf ids ARE
/// the slot indices (`0..k`). The server maps each slot index to its resolved
/// pane. Fails only on arity; fit is the server's geometric check.
pub fn topology(name: TemplateName, k: usize) -> Result<Node, TemplateError> {
    let arity = |want: usize, variadic: bool| TemplateError::Arity {
        want,
        got: k,
        variadic,
    };
    match name {
        // H[ s0, V[ s1.. ] ] - one main pane full height on the left.
        TemplateName::MainLeft => {
            if k < 2 {
                return Err(arity(2, true));
            }
            Ok(Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(0)), (0.5, rest(Axis::Vertical, k))],
            })
        }
        // V[ s0, H[ s1.. ] ] - one main pane full width on top.
        TemplateName::MainTop => {
            if k < 2 {
                return Err(arity(2, true));
            }
            Ok(Node::Branch {
                axis: Axis::Vertical,
                children: vec![(0.5, Node::Leaf(0)), (0.5, rest(Axis::Horizontal, k))],
            })
        }
        // H[ s0, s1, s2 ] - three columns.
        TemplateName::RowThirds => {
            if k != 3 {
                return Err(arity(3, false));
            }
            Ok(leaf_branch(Axis::Horizontal, 0, 3))
        }
        // V[ s0, s1, s2 ] - three stacked rows.
        TemplateName::ColThirds => {
            if k != 3 {
                return Err(arity(3, false));
            }
            Ok(leaf_branch(Axis::Vertical, 0, 3))
        }
        // V[ H[ s0, s1 ], H[ s2, s3 ] ].
        TemplateName::Grid2x2 => {
            if k != 4 {
                return Err(arity(4, false));
            }
            Ok(Node::Branch {
                axis: Axis::Vertical,
                children: vec![
                    (0.5, leaf_branch(Axis::Horizontal, 0, 2)),
                    (0.5, leaf_branch(Axis::Horizontal, 2, 4)),
                ],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{check_invariants, leaves, Tab};

    /// A topology is only meaningful if it passes the tree invariant checker
    /// (branch >= 2 children, ratios sum ~1.0, no nested same-axis, unique
    /// leaves). Wrap the root in a Tab (focus = slot 0) and check.
    fn assert_valid(node: &Node, k: usize) {
        let tab = Tab {
            id: 0,
            root: node.clone(),
            focus: 0,
            name: None,
        };
        check_invariants(&tab).expect("topology violates a tree invariant");
        let mut ls = leaves(node);
        ls.sort_unstable();
        assert_eq!(ls, (0..k as u64).collect::<Vec<_>>(), "slots 0..k appear once");
    }

    #[test]
    fn main_left_stacks_the_rest_on_the_right() {
        let t = topology(TemplateName::MainLeft, 4).unwrap();
        assert_valid(&t, 4);
        match &t {
            Node::Branch { axis, children } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children[0].1, Node::Leaf(0)); // main
                assert!(matches!(children[1].1, Node::Branch { axis: Axis::Vertical, .. }));
            }
            _ => panic!("expected a branch"),
        }
    }

    #[test]
    fn main_left_k2_is_a_flat_pair_not_a_single_child_branch() {
        let t = topology(TemplateName::MainLeft, 2).unwrap();
        assert_valid(&t, 2);
        // No degenerate one-child branch: the "rest" collapsed to a leaf.
        assert_eq!(
            t,
            Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(0)), (0.5, Node::Leaf(1))],
            }
        );
    }

    #[test]
    fn main_top_puts_the_row_below() {
        let t = topology(TemplateName::MainTop, 4).unwrap();
        assert_valid(&t, 4);
        match &t {
            Node::Branch { axis, children } => {
                assert_eq!(*axis, Axis::Vertical);
                assert_eq!(children[0].1, Node::Leaf(0));
                assert!(matches!(children[1].1, Node::Branch { axis: Axis::Horizontal, .. }));
            }
            _ => panic!("expected a branch"),
        }
    }

    #[test]
    fn thirds_and_grid_have_the_documented_shapes() {
        assert_valid(&topology(TemplateName::RowThirds, 3).unwrap(), 3);
        assert_valid(&topology(TemplateName::ColThirds, 3).unwrap(), 3);
        let g = topology(TemplateName::Grid2x2, 4).unwrap();
        assert_valid(&g, 4);
        // grid is a V of two H rows.
        match &g {
            Node::Branch { axis: Axis::Vertical, children } => {
                assert_eq!(children.len(), 2);
                for (_, row) in children {
                    assert!(matches!(row, Node::Branch { axis: Axis::Horizontal, .. }));
                }
            }
            _ => panic!("grid must be a vertical stack of rows"),
        }
    }

    #[test]
    fn fixed_arity_is_enforced() {
        assert_eq!(
            topology(TemplateName::Grid2x2, 3),
            Err(TemplateError::Arity { want: 4, got: 3, variadic: false })
        );
        assert_eq!(
            topology(TemplateName::RowThirds, 2),
            Err(TemplateError::Arity { want: 3, got: 2, variadic: false })
        );
    }

    #[test]
    fn variadic_minimum_arity_is_enforced() {
        assert_eq!(
            topology(TemplateName::MainLeft, 1),
            Err(TemplateError::Arity { want: 2, got: 1, variadic: true })
        );
        // Larger k is fine for a variadic template.
        assert_valid(&topology(TemplateName::MainTop, 6).unwrap(), 6);
    }
}
