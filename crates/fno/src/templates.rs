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

use crate::proto::{
    AnchoredLayoutSpec, LayoutBinding, LayoutTreeChild, LayoutTreeSpec, TemplateName,
};
use crate::tree::{Axis, Node};
use std::collections::HashMap;

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

// ---------------------------------------------------------------------------
// typed layout tree (x-6928): template -> LayoutTreeSpec, spec validation
// ---------------------------------------------------------------------------

/// Max leaves / nesting a layout spec may carry. Bounds malformed or
/// machine-generated input so it cannot create unbounded recursion or unusable
/// terminal geometry (AC4-EDGE-TOPOLOGY-BOUNDS).
pub const MAX_LAYOUT_LEAVES: usize = 24;
pub const MAX_LAYOUT_DEPTH: usize = 16;

/// Compile a named template + ordered slot names into a typed tree whose leaves
/// are [`LayoutTreeSpec::Slot`] (AC3-HP). The shape is byte-equivalent to
/// [`topology`] after pane-id substitution: we build the slot-index `Node` and
/// relabel each `Leaf(i)` with `slot_names[i]`, so the named template is a pure
/// macro over the same typed representation the arbitrary spec uses.
pub fn template_to_tree(
    name: TemplateName,
    slot_names: &[String],
) -> Result<LayoutTreeSpec, TemplateError> {
    let node = topology(name, slot_names.len())?;
    Ok(node_to_spec(&node, slot_names))
}

fn node_to_spec(node: &Node, slot_names: &[String]) -> LayoutTreeSpec {
    match node {
        Node::Leaf(i) => LayoutTreeSpec::Slot(slot_names[*i as usize].clone()),
        Node::Branch { axis, children } => LayoutTreeSpec::Split {
            axis: *axis,
            children: children
                .iter()
                .map(|(w, c)| LayoutTreeChild {
                    weight: *w,
                    tree: node_to_spec(c, slot_names),
                })
                .collect(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("layout tree must have 1..={MAX} leaves, got {got}", MAX = MAX_LAYOUT_LEAVES)]
    LeafCount { got: usize },
    #[error("layout tree depth must be <= {MAX}, got {got}", MAX = MAX_LAYOUT_DEPTH)]
    Depth { got: usize },
    #[error("a split must have >= 2 children, one has {0}")]
    FewChildren(usize),
    #[error("layout weights must be finite and positive")]
    BadWeight,
    #[error("exactly one slot must bind Anchor, found {0}")]
    AnchorCount(usize),
    #[error("slot {0:?} is defined but never referenced in the tree")]
    UnreferencedSlot(String),
    #[error("the tree references unknown slot {0:?}")]
    UnknownSlot(String),
    #[error("slot {0:?} is referenced more than once in the tree")]
    DuplicateReference(String),
    #[error("anchor spec version {0} is unsupported (expected 1)")]
    Version(u32),
}

/// Validate an [`AnchoredLayoutSpec`] purely, before any pane resolution or
/// mutation (AC4-EDGE). Bounds the topology (leaves, depth, split arity,
/// weights) and checks slot integrity: exactly one `Anchor`, and a one-to-one
/// mapping between defined slots and tree references. Duplicate-live-pane
/// detection is the server's job (it needs the registry).
pub fn validate_anchored_spec(spec: &AnchoredLayoutSpec) -> Result<(), SpecError> {
    if spec.version != 1 {
        return Err(SpecError::Version(spec.version));
    }
    let leaves = count_leaves(&spec.tree);
    if leaves == 0 || leaves > MAX_LAYOUT_LEAVES {
        return Err(SpecError::LeafCount { got: leaves });
    }
    let depth = depth(&spec.tree);
    if depth > MAX_LAYOUT_DEPTH {
        return Err(SpecError::Depth { got: depth });
    }
    validate_node(&spec.tree)?;

    let mut anchor_count = 0;
    let mut defined: HashMap<&str, ()> = HashMap::new();
    for slot in &spec.slots {
        if matches!(slot.binding, LayoutBinding::Anchor) {
            anchor_count += 1;
        }
        if !defined.insert(slot.name.as_str(), ()).is_none() {
            // A duplicate definition would otherwise surface as a duplicate
            // reference below; surface it where the author wrote it.
            return Err(SpecError::DuplicateReference(slot.name.clone()));
        }
    }
    if anchor_count != 1 {
        return Err(SpecError::AnchorCount(anchor_count));
    }

    let mut ref_counts: HashMap<&str, u32> = HashMap::new();
    collect_refs(&spec.tree, &mut |name| {
        *ref_counts.entry(name).or_insert(0) += 1;
    });
    for name in ref_counts.keys() {
        if !defined.contains_key(*name) {
            return Err(SpecError::UnknownSlot((*name).to_string()));
        }
    }
    for (name, count) in &ref_counts {
        if *count > 1 {
            return Err(SpecError::DuplicateReference((*name).to_string()));
        }
    }
    for slot in &spec.slots {
        if ref_counts.get(slot.name.as_str()).copied().unwrap_or(0) == 0 {
            return Err(SpecError::UnreferencedSlot(slot.name.clone()));
        }
    }
    Ok(())
}

fn count_leaves(tree: &LayoutTreeSpec) -> usize {
    match tree {
        LayoutTreeSpec::Slot(_) => 1,
        LayoutTreeSpec::Split { children, .. } => {
            children.iter().map(|c| count_leaves(&c.tree)).sum()
        }
    }
}

fn depth(tree: &LayoutTreeSpec) -> usize {
    match tree {
        LayoutTreeSpec::Slot(_) => 1,
        LayoutTreeSpec::Split { children, .. } => {
            1 + children.iter().map(|c| depth(&c.tree)).max().unwrap_or(0)
        }
    }
}

fn validate_node(tree: &LayoutTreeSpec) -> Result<(), SpecError> {
    match tree {
        LayoutTreeSpec::Slot(_) => Ok(()),
        LayoutTreeSpec::Split { children, .. } => {
            if children.len() < 2 {
                return Err(SpecError::FewChildren(children.len()));
            }
            for child in children {
                if !child.weight.is_finite() || child.weight <= 0.0 {
                    return Err(SpecError::BadWeight);
                }
                validate_node(&child.tree)?;
            }
            Ok(())
        }
    }
}

fn collect_refs<'a>(tree: &'a LayoutTreeSpec, out: &mut impl FnMut(&'a str)) {
    match tree {
        LayoutTreeSpec::Slot(name) => out(name.as_str()),
        LayoutTreeSpec::Split { children, .. } => {
            for child in children {
                collect_refs(&child.tree, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::LayoutSlot;
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
        assert_eq!(
            ls,
            (0..k as u64).collect::<Vec<_>>(),
            "slots 0..k appear once"
        );
    }

    #[test]
    fn main_left_stacks_the_rest_on_the_right() {
        let t = topology(TemplateName::MainLeft, 4).unwrap();
        assert_valid(&t, 4);
        match &t {
            Node::Branch { axis, children } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children[0].1, Node::Leaf(0)); // main
                assert!(matches!(
                    children[1].1,
                    Node::Branch {
                        axis: Axis::Vertical,
                        ..
                    }
                ));
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
                assert!(matches!(
                    children[1].1,
                    Node::Branch {
                        axis: Axis::Horizontal,
                        ..
                    }
                ));
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
            Node::Branch {
                axis: Axis::Vertical,
                children,
            } => {
                assert_eq!(children.len(), 2);
                for (_, row) in children {
                    assert!(matches!(
                        row,
                        Node::Branch {
                            axis: Axis::Horizontal,
                            ..
                        }
                    ));
                }
            }
            _ => panic!("grid must be a vertical stack of rows"),
        }
    }

    #[test]
    fn fixed_arity_is_enforced() {
        assert_eq!(
            topology(TemplateName::Grid2x2, 3),
            Err(TemplateError::Arity {
                want: 4,
                got: 3,
                variadic: false
            })
        );
        assert_eq!(
            topology(TemplateName::RowThirds, 2),
            Err(TemplateError::Arity {
                want: 3,
                got: 2,
                variadic: false
            })
        );
    }

    #[test]
    fn variadic_minimum_arity_is_enforced() {
        assert_eq!(
            topology(TemplateName::MainLeft, 1),
            Err(TemplateError::Arity {
                want: 2,
                got: 1,
                variadic: true
            })
        );
        // Larger k is fine for a variadic template.
        assert_valid(&topology(TemplateName::MainTop, 6).unwrap(), 6);
    }

    // -- x-6928 typed-tree compilation + spec validation --

    fn child(weight: f32, tree: LayoutTreeSpec) -> LayoutTreeChild {
        LayoutTreeChild { weight, tree }
    }
    fn slot(name: &str, binding: LayoutBinding) -> LayoutSlot {
        LayoutSlot {
            name: name.into(),
            binding,
        }
    }
    fn spec(tree: LayoutTreeSpec, slots: Vec<(&str, LayoutBinding)>) -> AnchoredLayoutSpec {
        AnchoredLayoutSpec {
            version: 1,
            tree,
            slots: slots.into_iter().map(|(n, b)| slot(n, b)).collect(),
        }
    }

    #[test]
    fn template_compiles_to_layout_tree() {
        // AC3-HP: each named template compiles into a typed tree whose slot
        // leaves appear in template order, and the topology bounds hold.
        for (name, k) in [
            (TemplateName::MainLeft, 2usize),
            (TemplateName::MainLeft, 4),
            (TemplateName::MainTop, 4),
            (TemplateName::RowThirds, 3),
            (TemplateName::ColThirds, 3),
            (TemplateName::Grid2x2, 4),
        ] {
            let slots: Vec<String> = (0..k).map(|i| format!("s{i}")).collect();
            let tree = template_to_tree(name, &slots).unwrap();
            let mut order: Vec<String> = Vec::new();
            collect_refs(&tree, &mut |n| order.push(n.to_string()));
            assert_eq!(order, slots, "slot order wrong for {name:?} k={k}");
            assert_eq!(count_leaves(&tree), k);
            validate_node(&tree).unwrap();
        }
        assert!(template_to_tree(TemplateName::RowThirds, &["a".into(), "b".into()]).is_err());
        assert!(template_to_tree(TemplateName::MainLeft, &["a".into()]).is_err());
    }

    #[test]
    fn anchored_layout_spec_accepts_a_valid_topology() {
        let valid = spec(
            LayoutTreeSpec::Split {
                axis: Axis::Vertical,
                children: vec![
                    child(0.5, LayoutTreeSpec::Slot("anchor".into())),
                    child(
                        0.5,
                        LayoutTreeSpec::Split {
                            axis: Axis::Horizontal,
                            children: vec![
                                child(0.5, LayoutTreeSpec::Slot("r".into())),
                                child(0.5, LayoutTreeSpec::Slot("t".into())),
                            ],
                        },
                    ),
                ],
            },
            vec![
                ("anchor", LayoutBinding::Anchor),
                ("r", LayoutBinding::Fno("abcd".into())),
                ("t", LayoutBinding::Shell),
            ],
        );
        validate_anchored_spec(&valid).expect("valid spec passes");
    }

    fn flat(n: usize) -> LayoutTreeSpec {
        // A vertical split of n distinct slot leaves.
        LayoutTreeSpec::Split {
            axis: Axis::Vertical,
            children: (0..n)
                .map(|i| child(1.0, LayoutTreeSpec::Slot(format!("s{i}"))))
                .collect(),
        }
    }

    fn slots_for(n: usize, anchor: bool) -> Vec<LayoutSlot> {
        (0..n)
            .map(|i| {
                slot(
                    &format!("s{i}"),
                    if i == 0 && anchor {
                        LayoutBinding::Anchor
                    } else {
                        LayoutBinding::Shell
                    },
                )
            })
            .collect()
    }

    #[test]
    fn anchored_layout_spec_rejects_bad_topology() {
        // AC4-EDGE-TOPOLOGY-BOUNDS: over the 24-leaf limit.
        let over = AnchoredLayoutSpec {
            version: 1,
            tree: flat(MAX_LAYOUT_LEAVES + 1),
            slots: slots_for(MAX_LAYOUT_LEAVES + 1, true),
        };
        assert_eq!(
            validate_anchored_spec(&over),
            Err(SpecError::LeafCount {
                got: MAX_LAYOUT_LEAVES + 1
            })
        );
        // At-limit is fine.
        let at = AnchoredLayoutSpec {
            version: 1,
            tree: flat(MAX_LAYOUT_LEAVES),
            slots: slots_for(MAX_LAYOUT_LEAVES, true),
        };
        validate_anchored_spec(&at).expect("24-leaf limit is allowed");
        // Depth past 16: nest a 17-deep chain (each level a 2-child split).
        let mut tree = LayoutTreeSpec::Slot("s0".into());
        for i in 1..=(MAX_LAYOUT_DEPTH + 1) {
            tree = LayoutTreeSpec::Split {
                axis: Axis::Vertical,
                children: vec![
                    child(1.0, tree),
                    child(1.0, LayoutTreeSpec::Slot(format!("s{i}"))),
                ],
            };
        }
        let n = MAX_LAYOUT_DEPTH + 2;
        let deep = AnchoredLayoutSpec {
            version: 1,
            tree,
            slots: slots_for(n, true),
        };
        assert!(matches!(
            validate_anchored_spec(&deep),
            Err(SpecError::Depth { .. })
        ));
        // A split with one child.
        let one = spec(
            LayoutTreeSpec::Split {
                axis: Axis::Vertical,
                children: vec![child(1.0, LayoutTreeSpec::Slot("anchor".into()))],
            },
            vec![("anchor", LayoutBinding::Anchor)],
        );
        assert_eq!(validate_anchored_spec(&one), Err(SpecError::FewChildren(1)));
        // Bad weights: zero, negative, NaN, infinity.
        for bad in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            let w = spec(
                LayoutTreeSpec::Split {
                    axis: Axis::Vertical,
                    children: vec![
                        child(bad, LayoutTreeSpec::Slot("anchor".into())),
                        child(1.0, LayoutTreeSpec::Slot("r".into())),
                    ],
                },
                vec![
                    ("anchor", LayoutBinding::Anchor),
                    ("r", LayoutBinding::Shell),
                ],
            );
            assert_eq!(
                validate_anchored_spec(&w),
                Err(SpecError::BadWeight),
                "weight {bad}"
            );
        }
        // Unsupported version.
        let mut v0 = spec(
            LayoutTreeSpec::Slot("anchor".into()),
            vec![("anchor", LayoutBinding::Anchor)],
        );
        v0.version = 2;
        assert_eq!(validate_anchored_spec(&v0), Err(SpecError::Version(2)));
    }

    #[test]
    fn anchored_layout_spec_rejects_bad_slots() {
        let two = |bindings: Vec<(&str, LayoutBinding)>| {
            spec(
                LayoutTreeSpec::Split {
                    axis: Axis::Vertical,
                    children: vec![
                        child(0.5, LayoutTreeSpec::Slot("anchor".into())),
                        child(0.5, LayoutTreeSpec::Slot("r".into())),
                    ],
                },
                bindings,
            )
        };
        // Zero anchors.
        assert_eq!(
            validate_anchored_spec(&two(vec![
                ("anchor", LayoutBinding::Shell),
                ("r", LayoutBinding::Shell),
            ])),
            Err(SpecError::AnchorCount(0))
        );
        // Two anchors.
        assert_eq!(
            validate_anchored_spec(&two(vec![
                ("anchor", LayoutBinding::Anchor),
                ("r", LayoutBinding::Anchor),
            ])),
            Err(SpecError::AnchorCount(2))
        );
        // Tree references an undefined slot.
        assert_eq!(
            validate_anchored_spec(&spec(
                LayoutTreeSpec::Split {
                    axis: Axis::Vertical,
                    children: vec![
                        child(0.5, LayoutTreeSpec::Slot("anchor".into())),
                        child(0.5, LayoutTreeSpec::Slot("ghost".into())),
                    ],
                },
                vec![
                    ("anchor", LayoutBinding::Anchor),
                    ("r", LayoutBinding::Shell)
                ],
            )),
            Err(SpecError::UnknownSlot("ghost".into()))
        );
        // A defined slot the tree never references.
        assert_eq!(
            validate_anchored_spec(&spec(
                LayoutTreeSpec::Split {
                    axis: Axis::Vertical,
                    children: vec![
                        child(0.5, LayoutTreeSpec::Slot("anchor".into())),
                        child(0.5, LayoutTreeSpec::Slot("r".into())),
                    ],
                },
                vec![
                    ("anchor", LayoutBinding::Anchor),
                    ("r", LayoutBinding::Shell),
                    ("lonely", LayoutBinding::Shell),
                ],
            )),
            Err(SpecError::UnreferencedSlot("lonely".into()))
        );
        // A slot referenced twice.
        assert_eq!(
            validate_anchored_spec(&spec(
                LayoutTreeSpec::Split {
                    axis: Axis::Vertical,
                    children: vec![
                        child(0.5, LayoutTreeSpec::Slot("anchor".into())),
                        child(0.5, LayoutTreeSpec::Slot("anchor".into())),
                    ],
                },
                vec![("anchor", LayoutBinding::Anchor)],
            )),
            Err(SpecError::DuplicateReference("anchor".into()))
        );
    }
}
