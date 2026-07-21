//! Pure n-ary pane-tree layout engine: split / close+collapse / rect tiling /
//! geometric directional navigation / pairwise resize.
//!
//! Deliberately I/O-free (no tokio, no `proto`/`server` dependency) so the
//! whole layout model is exhaustively unit-testable without a socket, a PTY,
//! or an event loop. `server.rs` (task 2.3) owns PaneId allocation (an
//! `AtomicU64`) and wires these ops into the core loop; this module only
//! knows about `Node`/`Tab` shapes and geometry.
//!
//! Model: an n-ary tree (`Warp`'s `pane_group/tree.rs` shape - `Branch` with
//! a `Vec` of ratio-tagged children, not a binary tree) so a 3-way split
//! never needs a synthetic wrapper branch. Directional navigation ports
//! herdr's `find_in_direction` geometry exactly (see `navigate` below).

use serde::{Deserialize, Serialize};

/// Monotonic allocation is the SERVER's job (task 2.3, an `AtomicU64`); the
/// tree never mints ids, callers pass a new id into [`split`].
pub type PaneId = u64;

/// Minimum pane size the layout engine will ever produce. [`split`] refuses
/// (tree unchanged) rather than emit a pane smaller than this.
pub const MIN_ROWS: u16 = 2;
pub const MIN_COLS: u16 = 8;

/// Ratio transferred per [`resize`] step. A plain constant, not a config
/// value - the caller (keys.rs, task 2.5) can always call `resize` in a loop
/// for a bigger jump; there's no product need to make this configurable.
pub const RESIZE_STEP: f32 = 0.05;

/// Split direction: `Horizontal` children sit side by side (left -> right,
/// i.e. a "split H" / vertical divider line); `Vertical` children stack top
/// -> bottom (a "split V" / horizontal divider line). Named for the axis
/// panes are arranged ALONG, matching tmux/i3 convention, not the divider's
/// own orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A navigate/resize direction. Rides the wire in `Command::FocusDir` /
/// `Command::ResizeDir` (task 2.2), hence `Serialize`/`Deserialize` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// A tiled rectangle: `(x, y)` is the top-left corner within the content
/// area, `rows`/`cols` the size in cells. u16 matches terminal dimensions
/// (`ClientMsg::Attach`/`Resize` already use u16 rows/cols).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub rows: u16,
    pub cols: u16,
}

/// A node in the pane tree. `Branch` ratios sum to 1.0 (checked by
/// [`check_invariants`]); a `Branch` always has >= 2 children - a
/// single-child branch is collapsed away by [`close`], and a same-axis
/// nested branch is normalized (merged) away, never left standing.
///
/// ponytail: the shared-type contract asks for `Copy, Eq` on every type in
/// this module, but neither derives on `Node`/`Tab`: `Vec` is never `Copy`,
/// and `f32` never implements `Eq` (NaN), so `#[derive(Eq)]` on a type
/// containing one is a compile error, not a style choice. `Axis`, `Dir`,
/// `Rect` (below) get the full derive set as specified.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Leaf(PaneId),
    Branch {
        axis: Axis,
        children: Vec<(f32, Node)>,
    },
}

/// Stable tab identity: session-scoped, monotonic, never reused (Locked
/// Decision 6 extended to tabs in Phase 3). Minted by `squad::Session`, like
/// `PaneId` the tree itself never allocates one.
pub type TabId = u64;

/// One tab: a stable id, a pane tree, plus exactly one focused (live) leaf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub root: Node,
    pub focus: PaneId,
    /// Explicit user rename (`Command::RenameTab`, x-c150). `None` means the
    /// display label derives from the focused pane's spawn-time facts
    /// (server-side); `serde(default)` keeps pre-rename serialized forms
    /// parseable.
    #[serde(default)]
    pub name: Option<String>,
}

/// [`split`] failure. The tree is left completely unchanged on `Err`.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum SplitError {
    #[error(
        "split refused: a resulting pane would be smaller than the {min_rows}x{min_cols} minimum"
    )]
    TooSmall { min_rows: u16, min_cols: u16 },
    #[error("focused pane {0} not found in the tree")]
    FocusNotFound(PaneId),
}

/// [`move_leaf`] failure. The tree is left completely unchanged on `Err`.
///
/// Each variant maps to a distinct operator-facing notice: the sizes are a
/// "make room first" problem, a gone pane is a "your view is stale" one, and
/// the two must not collapse into one message.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum MoveError {
    #[error(
        "move refused: a resulting pane would be smaller than the {min_rows}x{min_cols} minimum"
    )]
    TooSmall { min_rows: u16, min_cols: u16 },
    /// The mover or the target is no longer in the tree - a drop validated
    /// against a layout another client has since changed.
    #[error("move refused: that pane is no longer in the layout")]
    PaneGone,
    /// A drop on the pane's own origin. Not a failure the operator caused, so
    /// callers treat it as a silent cancel rather than something to announce.
    #[error("move refused: the pane is already there")]
    Origin,
}

// ---------------------------------------------------------------------------
// layout: tree -> tiled rects
// ---------------------------------------------------------------------------

/// Tile `node` into `viewport`, returning every leaf's rect. Siblings along a
/// branch's axis are separated by a 1-cell divider (`N` children consume
/// `N-1` divider cells). Integer tiling is exact and deterministic: every
/// child but the last gets `floor(available * ratio)`, and the LAST child
/// absorbs whatever remains - so the union of pane rects plus divider cells
/// always equals the input area exactly, with no gaps, no overlaps, and no
/// rounding drift accumulating toward one side.
pub fn layout(node: &Node, viewport: Rect) -> Vec<(PaneId, Rect)> {
    let mut out = Vec::new();
    layout_into(node, viewport, &mut out);
    out
}

fn layout_into(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(id) => out.push((*id, area)),
        Node::Branch { axis, children } => {
            for (i, child_area) in child_areas(*axis, area, children).into_iter().enumerate() {
                layout_into(&children[i].1, child_area, out);
            }
        }
    }
}

/// Compute each child's area for one branch level. Shared by [`layout_into`]
/// (recurses into every child) and [`area_at_path`]/[`resize`] (which only
/// need one specific child's or the branch's own area).
///
/// ponytail: a viewport smaller than `children.len() - 1` divider cells
/// saturates to a zero-width/height tail rather than panicking or going
/// negative. Real call sites never hit this: [`split`] already refuses to
/// create a pane below [`MIN_ROWS`]/[`MIN_COLS`], so a degenerate viewport
/// can only reach here via a caller handing `layout`/`resize` a viewport
/// directly - in which case a zero-sized (never overlapping, never negative)
/// rect is the honest answer, not a clamp that would fabricate space that
/// isn't there.
fn child_areas(axis: Axis, area: Rect, children: &[(f32, Node)]) -> Vec<Rect> {
    let n = children.len();
    if n == 0 {
        return Vec::new();
    }
    let dividers = (n - 1) as u16;
    let total = match axis {
        Axis::Horizontal => area.cols,
        Axis::Vertical => area.rows,
    };
    let available = total.saturating_sub(dividers);

    let mut out = Vec::with_capacity(n);
    let mut offset: u16 = 0;
    let mut used: u16 = 0;
    for (i, (ratio, _)) in children.iter().enumerate() {
        let len = if i + 1 == n {
            available.saturating_sub(used)
        } else {
            let l = ((available as f32) * ratio).floor() as u16;
            used += l;
            l
        };
        let child_area = match axis {
            Axis::Horizontal => Rect {
                x: area.x + offset,
                y: area.y,
                rows: area.rows,
                cols: len,
            },
            Axis::Vertical => Rect {
                x: area.x,
                y: area.y + offset,
                rows: len,
                cols: area.cols,
            },
        };
        out.push(child_area);
        offset += len;
        if i + 1 < n {
            offset += 1; // divider cell
        }
    }
    out
}

/// Every leaf id in `node`, in tree order. The cheap membership walk callers
/// (squad.rs pane lookup) use when they need ids but not geometry.
pub fn leaves(node: &Node) -> Vec<PaneId> {
    fn walk(node: &Node, out: &mut Vec<PaneId>) {
        match node {
            Node::Leaf(id) => out.push(*id),
            Node::Branch { children, .. } => {
                for (_, child) in children {
                    walk(child, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(node, &mut out);
    out
}

/// The area of the node reached by descending `path` (a chain of child
/// indices from the root). Used by [`resize`] to find a branch's own area
/// (to compute its minimum-size ratio) without walking every leaf.
fn area_at_path(node: &Node, area: Rect, path: &[usize]) -> Rect {
    match path.split_first() {
        None => area,
        Some((&idx, rest)) => match node {
            Node::Branch { axis, children } => {
                let areas = child_areas(*axis, area, children);
                area_at_path(&children[idx].1, areas[idx], rest)
            }
            Node::Leaf(_) => area,
        },
    }
}

/// The node reached by descending `path`.
fn node_at_path<'a>(node: &'a Node, path: &[usize]) -> &'a Node {
    match path.split_first() {
        None => node,
        Some((&idx, rest)) => match node {
            Node::Branch { children, .. } => node_at_path(&children[idx].1, rest),
            Node::Leaf(_) => node,
        },
    }
}

// ---------------------------------------------------------------------------
// split
// ---------------------------------------------------------------------------

/// Split the focused leaf along `axis`, inserting a new leaf carrying
/// `new_id`. The new pane takes focus.
///
/// - Same-axis parent: the new leaf is inserted adjacent to the target
///   inside the SAME branch, halving the target's ratio (`r -> r/2, r/2`).
/// - Cross-axis (or the target is a lone leaf with no matching-axis parent):
///   the target leaf is wrapped in a new `Branch` of the requested axis with
///   two `[0.5, 0.5]` children.
///
/// Refuses (tree unchanged) if any resulting pane would drop below
/// [`MIN_ROWS`]x[`MIN_COLS`] in `viewport`.
pub fn split(tab: &mut Tab, viewport: Rect, axis: Axis, new_id: PaneId) -> Result<(), SplitError> {
    let direction = match axis {
        Axis::Horizontal => Dir::Right,
        Axis::Vertical => Dir::Down,
    };
    split_directional(tab, viewport, direction, new_id)
}

/// Split the focused leaf and place the new pane on the requested side.
pub fn split_directional(
    tab: &mut Tab,
    viewport: Rect,
    direction: Dir,
    new_id: PaneId,
) -> Result<(), SplitError> {
    let (axis, before) = match direction {
        Dir::Left => (Axis::Horizontal, true),
        Dir::Right => (Axis::Horizontal, false),
        Dir::Up => (Axis::Vertical, true),
        Dir::Down => (Axis::Vertical, false),
    };
    let candidate = split_node(&tab.root, tab.focus, axis, before, new_id)
        .ok_or(SplitError::FocusNotFound(tab.focus))?;

    // Per-pane, PER-AXIS: a pane fails on either dimension independently. A
    // single "smallest pane by min dimension" reduction conflates the axes -
    // a tall-thin pane (cols below minimum) hides behind a short-wide pane
    // whose min dimension is smaller but which passes both checks.
    if layout(&candidate, viewport)
        .iter()
        .any(|(_, r)| r.rows < MIN_ROWS || r.cols < MIN_COLS)
    {
        return Err(SplitError::TooSmall {
            min_rows: MIN_ROWS,
            min_cols: MIN_COLS,
        });
    }

    tab.root = candidate;
    tab.focus = new_id;
    Ok(())
}

/// Build the post-split tree. `None` means `target` isn't in this subtree.
fn split_node(
    node: &Node,
    target: PaneId,
    split_axis: Axis,
    before: bool,
    new_id: PaneId,
) -> Option<Node> {
    match node {
        Node::Leaf(id) if *id == target => Some(Node::Branch {
            axis: split_axis,
            children: if before {
                vec![(0.5, Node::Leaf(new_id)), (0.5, Node::Leaf(*id))]
            } else {
                vec![(0.5, Node::Leaf(*id)), (0.5, Node::Leaf(new_id))]
            },
        }),
        Node::Leaf(_) => None,
        Node::Branch { axis, children } => {
            // Same-axis parent with the target as a DIRECT child: insert
            // adjacent inside this branch rather than wrapping.
            if *axis == split_axis {
                if let Some(idx) = children
                    .iter()
                    .position(|(_, n)| matches!(n, Node::Leaf(id) if *id == target))
                {
                    let half = children[idx].0 / 2.0;
                    let mut new_children = children.clone();
                    new_children[idx] = (half, Node::Leaf(target));
                    new_children.insert(
                        if before { idx } else { idx + 1 },
                        (half, Node::Leaf(new_id)),
                    );
                    return Some(Node::Branch {
                        axis: *axis,
                        children: new_children,
                    });
                }
            }
            // Not a direct same-axis match: recurse (handles both
            // cross-axis leaf-wrap and deeper nesting).
            for (i, (ratio, child)) in children.iter().enumerate() {
                if let Some(new_child) = split_node(child, target, split_axis, before, new_id) {
                    let mut new_children = children.clone();
                    new_children[i] = (*ratio, new_child);
                    return Some(Node::Branch {
                        axis: *axis,
                        children: new_children,
                    });
                }
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// replace_leaf (open-here, x-9f75)
// ---------------------------------------------------------------------------

/// Repoint the leaf hosting `old` at `new`, leaving geometry (branch structure, ratios) untouched - only the
/// pane id at that slot changes; focus follows when `old` held it. Returns `false` (tree unchanged) if `old`
/// is absent. The open-here primitive: unlike [`split_directional`] it creates no `Branch`, so a swap-in-place
/// always fits and can never fail on min-size.
pub fn replace_leaf(tab: &mut Tab, old: PaneId, new: PaneId) -> bool {
    if replace_leaf_node(&mut tab.root, old, new) {
        if tab.focus == old {
            tab.focus = new;
        }
        true
    } else {
        false
    }
}

fn replace_leaf_node(node: &mut Node, old: PaneId, new: PaneId) -> bool {
    match node {
        Node::Leaf(id) if *id == old => {
            *id = new;
            true
        }
        Node::Leaf(_) => false,
        Node::Branch { children, .. } => children
            .iter_mut()
            .any(|(_, child)| replace_leaf_node(child, old, new)),
    }
}

// ---------------------------------------------------------------------------
// move_leaf (drag-to-relocate + keyboard move-pane, x-aa95)
// ---------------------------------------------------------------------------

/// Relocate the `mover` leaf to sit adjacent to `target` on its `dir` side.
/// Every `Err` leaves the tab COMPLETELY unchanged, so a rejected drop needs no
/// re-sync - the client flashes and keeps drawing what it already has.
///
/// The error is typed rather than a bool because a refused drop has to explain
/// itself: "a pane would be too small" and "that pane is gone" send the operator
/// to different fixes, and a drag that dies silently reads as a broken feature.
///
/// Deliberately a remove-then-insert composed from [`remove_leaf`] (which owns
/// ratio redistribution and single-child collapse) and [`split_node`] (which
/// owns adjacent insertion and cross-axis wrapping), rather than rect swapping
/// or bespoke ratio arithmetic. Relocation is exactly a close plus a split at a
/// different slot, and reusing both halves is why the ratio sum, the
/// no-single-child-branch rule, and the same-axis merge all keep holding here
/// for free.
///
pub fn move_leaf(
    tab: &mut Tab,
    viewport: Rect,
    mover: PaneId,
    target: PaneId,
    dir: Dir,
) -> Result<(), MoveError> {
    if mover == target {
        return Err(MoveError::Origin);
    }
    // Both ends are validated BEFORE any surgery: the target must survive the
    // removal to be insertable next to, and checking after would mean undoing.
    let present = leaves(&tab.root);
    if !present.contains(&mover) || !present.contains(&target) {
        return Err(MoveError::PaneGone);
    }
    // Defensive: `None` (absent) and `Some(None)` (the root WAS the mover, so
    // the tab held one pane) are both already excluded by the guards above - a
    // lone pane cannot coexist with a present, different target. Folded into
    // `PaneGone` rather than given a variant of its own, which would advertise
    // a refusal reason no caller can ever actually receive.
    let Some(Some(without)) = remove_leaf(&tab.root, mover) else {
        return Err(MoveError::PaneGone);
    };
    // Normalizing HERE is load-bearing: the removal can leave a nested
    // same-axis branch, and inserting into one would place `mover` in the
    // wrong branch relative to the seam the operator pointed at.
    let without = normalize(without);

    let (axis, before) = match dir {
        Dir::Left => (Axis::Horizontal, true),
        Dir::Right => (Axis::Horizontal, false),
        Dir::Up => (Axis::Vertical, true),
        Dir::Down => (Axis::Vertical, false),
    };
    let Some(candidate) = split_node(&without, target, axis, before, mover) else {
        return Err(MoveError::PaneGone);
    };

    // Same per-pane, per-axis check as `split_directional`: a pane fails on
    // either dimension independently.
    if layout(&candidate, viewport)
        .iter()
        .any(|(_, r)| r.rows < MIN_ROWS || r.cols < MIN_COLS)
    {
        return Err(MoveError::TooSmall {
            min_rows: MIN_ROWS,
            min_cols: MIN_COLS,
        });
    }

    tab.root = candidate;
    tab.focus = mover;
    Ok(())
}

// ---------------------------------------------------------------------------
// close
// ---------------------------------------------------------------------------

/// Close the `target` leaf. Returns `true` if that was the tab's last pane -
/// the caller must discard the `Tab` (a `Node` cannot represent "empty").
///
/// A `target` not present in the tree is a no-op (`false`, tree unchanged) -
/// idempotent by construction so a racing double-close (task 2.3's AC4-ERR)
/// never double-reaps here.
///
/// Redistributes the closed leaf's ratio proportionally to its siblings
/// (`scale = 1 / (1 - r)`), collapses any branch that drops to a single
/// child, normalizes any resulting nested same-axis branch, and - if the
/// closed pane held focus - re-anchors focus to the geometrically nearest
/// surviving leaf relative to the closed pane's last rect.
pub fn close(tab: &mut Tab, viewport: Rect, target: PaneId) -> bool {
    if matches!(&tab.root, Node::Leaf(id) if *id == target) {
        return true;
    }

    let before = layout(&tab.root, viewport);
    let closed_rect = before.iter().find(|(id, _)| *id == target).map(|(_, r)| *r);

    match remove_leaf(&tab.root, target) {
        None => false, // not found: idempotent no-op
        Some(None) => unreachable!("remove_leaf(root) only returns Some(None) for a root Leaf"),
        Some(Some(new_root)) => {
            tab.root = normalize(new_root);
            if tab.focus == target {
                let after = layout(&tab.root, viewport);
                let from = closed_rect.unwrap_or(viewport);
                if let Some(new_focus) = nearest_surviving(&after, from) {
                    tab.focus = new_focus;
                }
            }
            false
        }
    }
}

/// `None`: `target` not found. `Some(None)`: `node` itself was the target
/// leaf (only reachable when `node` is a direct `Branch` child). `Some(Some
/// (n))`: `node` is replaced by `n` in its parent's slot (same ratio).
fn remove_leaf(node: &Node, target: PaneId) -> Option<Option<Node>> {
    match node {
        Node::Leaf(id) if *id == target => Some(None),
        Node::Leaf(_) => None,
        Node::Branch { axis, children } => {
            for (i, (ratio, child)) in children.iter().enumerate() {
                match remove_leaf(child, target) {
                    None => continue,
                    Some(None) => {
                        // Direct child removed entirely: redistribute its
                        // ratio to the survivors, then collapse if only one
                        // remains.
                        let removed_ratio = *ratio;
                        let scale = if (1.0 - removed_ratio).abs() > f32::EPSILON {
                            1.0 / (1.0 - removed_ratio)
                        } else {
                            1.0
                        };
                        let mut remaining: Vec<(f32, Node)> = children
                            .iter()
                            .enumerate()
                            .filter(|(j, _)| *j != i)
                            .map(|(_, (r, n))| (r * scale, n.clone()))
                            .collect();
                        return Some(Some(if remaining.len() == 1 {
                            remaining.pop().unwrap().1
                        } else {
                            Node::Branch {
                                axis: *axis,
                                children: remaining,
                            }
                        }));
                    }
                    Some(Some(new_child)) => {
                        let mut new_children = children.clone();
                        new_children[i] = (*ratio, new_child);
                        return Some(Some(Node::Branch {
                            axis: *axis,
                            children: new_children,
                        }));
                    }
                }
            }
            None
        }
    }
}

/// Merge any `Branch` whose child is a `Branch` of the SAME axis into its
/// parent, multiplying ratios through so relative proportions are preserved
/// (`grandchild_ratio * child_ratio`). Runs bottom-up so a chain of nested
/// same-axis branches (which [`close`]'s collapse can produce) flattens in
/// one pass.
fn normalize(node: Node) -> Node {
    match node {
        Node::Leaf(id) => Node::Leaf(id),
        Node::Branch { axis, children } => {
            let mut merged: Vec<(f32, Node)> = Vec::with_capacity(children.len());
            for (ratio, child) in children {
                match normalize(child) {
                    Node::Branch {
                        axis: child_axis,
                        children: grandchildren,
                    } if child_axis == axis => {
                        for (gratio, gnode) in grandchildren {
                            merged.push((gratio * ratio, gnode));
                        }
                    }
                    other => merged.push((ratio, other)),
                }
            }
            if merged.len() == 1 {
                merged.pop().unwrap().1
            } else {
                Node::Branch {
                    axis,
                    children: merged,
                }
            }
        }
    }
}

/// Nearest surviving leaf to `from` (the closed pane's last rect), reusing
/// the directional-navigation geometry: try all four directions and take
/// whichever candidate has the smallest edge distance overall. Falls back to
/// an arbitrary survivor if geometry finds nothing (defensive only - a
/// tiled tree always has SOME pane in some direction from any point inside
/// its former viewport).
fn nearest_surviving(panes: &[(PaneId, Rect)], from: Rect) -> Option<PaneId> {
    [Dir::Left, Dir::Right, Dir::Up, Dir::Down]
        .into_iter()
        .filter_map(|dir| {
            let id = find_in_direction(panes, from, None, dir)?;
            let rect = panes.iter().find(|(pid, _)| *pid == id)?.1;
            Some((id, edge_distance(from, rect, dir)))
        })
        .min_by_key(|(_, dist)| *dist)
        .map(|(id, _)| id)
        .or_else(|| panes.first().map(|(id, _)| *id))
}

// ---------------------------------------------------------------------------
// navigate
// ---------------------------------------------------------------------------

/// Geometric adjacency, ported EXACTLY from herdr's `find_in_direction`
/// (`~/code/tools/herdr/src/layout.rs`): filter to panes strictly beyond the
/// focused rect in `dir` with perpendicular-range overlap, then pick the min
/// by `(edge_distance, Reverse(overlap_amount), center_distance, index)`.
pub fn navigate(node: &Node, viewport: Rect, focus: PaneId, dir: Dir) -> Option<PaneId> {
    let panes = layout(node, viewport);
    let from = panes.iter().find(|(id, _)| *id == focus)?.1;
    find_in_direction(&panes, from, Some(focus), dir)
}

fn find_in_direction(
    panes: &[(PaneId, Rect)],
    from: Rect,
    exclude: Option<PaneId>,
    dir: Dir,
) -> Option<PaneId> {
    panes
        .iter()
        .enumerate()
        .filter(|(_, (id, _))| Some(*id) != exclude)
        .filter(|(_, (_, r))| match dir {
            Dir::Left => r.x + r.cols <= from.x && ranges_overlap(r.y, r.rows, from.y, from.rows),
            Dir::Right => {
                r.x >= from.x + from.cols && ranges_overlap(r.y, r.rows, from.y, from.rows)
            }
            Dir::Up => r.y + r.rows <= from.y && ranges_overlap(r.x, r.cols, from.x, from.cols),
            Dir::Down => {
                r.y >= from.y + from.rows && ranges_overlap(r.x, r.cols, from.x, from.cols)
            }
        })
        .min_by_key(|(index, (_, r))| {
            let overlap = match dir {
                Dir::Left | Dir::Right => range_overlap_amount(r.y, r.rows, from.y, from.rows),
                Dir::Up | Dir::Down => range_overlap_amount(r.x, r.cols, from.x, from.cols),
            };
            let center_distance = match dir {
                Dir::Left | Dir::Right => range_center_distance(r.y, r.rows, from.y, from.rows),
                Dir::Up | Dir::Down => range_center_distance(r.x, r.cols, from.x, from.cols),
            };
            (
                edge_distance(from, *r, dir),
                std::cmp::Reverse(overlap),
                center_distance,
                *index,
            )
        })
        .map(|(_, (id, _))| *id)
}

fn edge_distance(from: Rect, r: Rect, dir: Dir) -> u16 {
    match dir {
        Dir::Left => from.x.saturating_sub(r.x + r.cols),
        Dir::Right => r.x.saturating_sub(from.x + from.cols),
        Dir::Up => from.y.saturating_sub(r.y + r.rows),
        Dir::Down => r.y.saturating_sub(from.y + from.rows),
    }
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start + b_len && a_start + a_len > b_start
}

fn range_overlap_amount(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn range_center_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_center = a_start.saturating_mul(2).saturating_add(a_len);
    let b_center = b_start.saturating_mul(2).saturating_add(b_len);
    a_center.abs_diff(b_center)
}

// ---------------------------------------------------------------------------
// resize
// ---------------------------------------------------------------------------

/// Grow/shrink the focused pane along `dir` by transferring `step` of ratio
/// between it and its neighbor in the nearest ancestor branch whose axis
/// matches (`Left`/`Right` -> `Horizontal`, `Up`/`Down` -> `Vertical`).
/// Returns `true` if anything changed (caller sounds BEL only on `false`).
///
/// Direction convention (the brief leaves the exact edge semantics to the
/// implementer): `Right`/`Down` grow the focused pane by taking ratio from
/// its NEXT sibling; `Left`/`Up` shrink the focused pane, giving ratio to
/// its PREVIOUS sibling. No sibling in that direction (focus is already the
/// edge child) is a no-op, same as "no matching ancestor".
///
/// The transfer clamps so neither side drops below the minimum cell size for
/// the branch's own area (partial application allowed, per AC3-ERR) and
/// conserves the branch's ratio sum exactly (what one side gains is exactly
/// what the other loses - no separate renormalize pass needed).
pub fn resize(tab: &mut Tab, viewport: Rect, dir: Dir, step: f32) -> bool {
    let want_axis = match dir {
        Dir::Left | Dir::Right => Axis::Horizontal,
        Dir::Up | Dir::Down => Axis::Vertical,
    };

    let mut path = Vec::new();
    if !find_path(&tab.root, tab.focus, &mut path) {
        return false;
    }

    let Some(prefix_len) = deepest_matching_ancestor(&tab.root, &path, want_axis) else {
        return false;
    };
    let branch_path = &path[..prefix_len];
    let child_idx = path[prefix_len];

    let Node::Branch { children, .. } = node_at_path(&tab.root, branch_path) else {
        return false;
    };
    let n = children.len();
    let neighbor_idx = match dir {
        Dir::Right | Dir::Down => child_idx.checked_add(1).filter(|&i| i < n),
        Dir::Left | Dir::Up => child_idx.checked_sub(1),
    };
    let Some(neighbor_idx) = neighbor_idx else {
        return false;
    };

    let branch_area = area_at_path(&tab.root, viewport, branch_path);
    let axis_len = match want_axis {
        Axis::Horizontal => branch_area.cols,
        Axis::Vertical => branch_area.rows,
    };
    let dividers = (n as u16).saturating_sub(1);
    let available = axis_len.saturating_sub(dividers).max(1) as f32;
    let min_len = match want_axis {
        Axis::Horizontal => MIN_COLS,
        Axis::Vertical => MIN_ROWS,
    };
    let min_ratio = (min_len as f32 / available).min(1.0);

    let focus_ratio = children[child_idx].0;
    let neighbor_ratio = children[neighbor_idx].0;
    let focus_delta = match dir {
        Dir::Right | Dir::Down => step,
        Dir::Left | Dir::Up => -step,
    };
    let transfer = if focus_delta > 0.0 {
        focus_delta.min((neighbor_ratio - min_ratio).max(0.0))
    } else {
        focus_delta.max(-(focus_ratio - min_ratio).max(0.0))
    };
    if transfer == 0.0 {
        return false;
    }

    let updates = [
        (child_idx, focus_ratio + transfer),
        (neighbor_idx, neighbor_ratio - transfer),
    ];
    tab.root = set_children_ratios(&tab.root, branch_path, &updates);
    true
}

/// Move the seam between the branch children holding `a` and `b` so the
/// divider lands on `pos` (a content-area coordinate along the branch's axis:
/// a column for `Horizontal`, a row for `Vertical`). Returns `true` if
/// anything changed.
///
/// The seam is addressed by the panes flanking it because that is what the
/// client can see - `ServerMsg::Layout` carries rects, never the tree. Any
/// pane from each side names the same two branch children, and resolving the
/// pair here is also what validates it: panes that have gone, or that are no
/// longer adjacent children of a common branch, refuse with the tree
/// untouched. A concurrent split or close therefore cannot land a resize on
/// the wrong seam.
///
/// The target is a POSITION rather than a ratio because only this side can
/// convert one into the other. A caller holding just the rects cannot: it sees
/// the flanking PANES, and a pane is not its branch child. Alternating axes
/// nest legally (`Horizontal` -> `Vertical` -> `Horizontal`), so a pane two
/// levels down spans a fraction of its child's extent, and a ratio derived
/// from the pane's rect would move the divider somewhere the operator did not
/// point. `child_areas` below gives the child's true extent, so the conversion
/// is exact at any nesting depth.
///
/// The position is ABSOLUTE: repeated sets are idempotent, a dropped one
/// self-heals on the next, and no drift accumulates the way a stream of deltas
/// would. What one child gains the other loses exactly, so the branch's ratio
/// sum is conserved without a renormalize pass.
pub fn set_seam_pos(tab: &mut Tab, viewport: Rect, a: PaneId, b: PaneId, pos: u16) -> bool {
    let (mut pa, mut pb) = (Vec::new(), Vec::new());
    if !find_path(&tab.root, a, &mut pa) || !find_path(&tab.root, b, &mut pb) {
        return false;
    }
    // The deepest branch they share; the children they descend from must be
    // adjacent there, in order, or the two panes do not flank one seam.
    let Some(depth) = (0..pa.len().min(pb.len())).find(|&k| pa[k] != pb[k]) else {
        return false;
    };
    let (i, j) = (pa[depth], pb[depth]);
    if i + 1 != j {
        return false;
    }
    let branch_path = &pa[..depth];
    let Node::Branch { axis, children } = node_at_path(&tab.root, branch_path) else {
        return false;
    };
    let (axis, n) = (*axis, children.len());

    let branch_area = area_at_path(&tab.root, viewport, branch_path);
    let axis_len = match axis {
        Axis::Horizontal => branch_area.cols,
        Axis::Vertical => branch_area.rows,
    };
    let available = axis_len.saturating_sub((n as u16).saturating_sub(1)).max(1) as f32;
    let min_len = match axis {
        Axis::Horizontal => MIN_COLS,
        Axis::Vertical => MIN_ROWS,
    };
    let min_ratio = (min_len as f32 / available).min(1.0);

    // The child's OWN extent, not the flanking pane's: this is the step that
    // makes the conversion exact when a pane sits deeper than its branch child.
    let child_start = {
        let areas = child_areas(axis, branch_area, children);
        match axis {
            Axis::Horizontal => areas[i].x,
            Axis::Vertical => areas[i].y,
        }
    };
    // The length child `i` must take for the divider to land on `pos`.
    let want_len = pos.saturating_sub(child_start) as f32;

    let held = children[i].0;
    let pair_total = held + children[j].0;
    // Both sides need room; a pair too small to seat two minimums has no
    // legal seam position at all, so leave it where it is.
    //
    // Written `!(lo <= hi)` rather than `lo > hi` so a NaN refuses instead of
    // falling through: every comparison against NaN is false, and `f32::clamp`
    // panics outright on a NaN bound. A NaN ratio is not merely hypothetical -
    // `check_invariants` shares the blind spot (`(sum - 1.0).abs() > 1e-4` is
    // also false for NaN), so one could reach here unflagged from a persisted
    // or deserialized tree.
    let (lo, hi) = (min_ratio, pair_total - min_ratio);
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    let no_room = !(lo <= hi);
    if no_room {
        return false;
    }
    let target = (want_len / available).clamp(lo, hi);
    // A drag reports many cells per branch cell; only a real move is a write.
    if (target - held).abs() < 1e-4 {
        return false;
    }
    let updates = [(i, target), (j, pair_total - target)];
    tab.root = set_children_ratios(&tab.root, branch_path, &updates);
    true
}

/// Populate `path` with the child-index chain from `node` down to `target`.
/// Returns `false` (and leaves `path` unchanged in length) if not found.
fn find_path(node: &Node, target: PaneId, path: &mut Vec<usize>) -> bool {
    match node {
        Node::Leaf(id) => *id == target,
        Node::Branch { children, .. } => {
            for (i, (_, child)) in children.iter().enumerate() {
                path.push(i);
                if find_path(child, target, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
    }
}

/// The deepest prefix of `path` whose branch axis matches `want_axis` -
/// "nearest ancestor" means closest to the leaf, so scan from the end.
fn deepest_matching_ancestor(root: &Node, path: &[usize], want_axis: Axis) -> Option<usize> {
    (0..path.len())
        .rev()
        .find(|&level| match node_at_path(root, &path[..level]) {
            Node::Branch { axis, .. } => *axis == want_axis,
            Node::Leaf(_) => false,
        })
}

/// Rebuild `node` with `updates` (index, new_ratio) applied to the branch at
/// `path`.
fn set_children_ratios(node: &Node, path: &[usize], updates: &[(usize, f32)]) -> Node {
    match path.split_first() {
        None => match node {
            Node::Branch { axis, children } => {
                let mut new_children = children.clone();
                for &(idx, r) in updates {
                    new_children[idx].0 = r;
                }
                Node::Branch {
                    axis: *axis,
                    children: new_children,
                }
            }
            leaf => leaf.clone(),
        },
        Some((&idx, rest)) => match node {
            Node::Branch { axis, children } => {
                let mut new_children = children.clone();
                new_children[idx] = (
                    new_children[idx].0,
                    set_children_ratios(&children[idx].1, rest, updates),
                );
                Node::Branch {
                    axis: *axis,
                    children: new_children,
                }
            }
            leaf => leaf.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// invariants
// ---------------------------------------------------------------------------

/// Debug-assert-style checker, run after every mutation in tests: ratio sums
/// == 1.0 (within an f32 epsilon) in every branch, focus is a live leaf, no
/// single-child branch survives, no branch nests a same-axis child, and
/// every `PaneId` in the tree is unique.
pub fn check_invariants(tab: &Tab) -> Result<(), String> {
    fn walk(
        node: &Node,
        ids: &mut std::collections::HashSet<PaneId>,
        errors: &mut Vec<String>,
        parent_axis: Option<Axis>,
    ) {
        match node {
            Node::Leaf(id) => {
                if !ids.insert(*id) {
                    errors.push(format!("duplicate PaneId {id}"));
                }
            }
            Node::Branch { axis, children } => {
                if children.len() < 2 {
                    errors.push(format!(
                        "branch with {} child(ren), must be collapsed",
                        children.len()
                    ));
                }
                if parent_axis == Some(*axis) {
                    errors.push("nested same-axis branch was not normalized".to_string());
                }
                let sum: f32 = children.iter().map(|(r, _)| r).sum();
                if (sum - 1.0).abs() > 1e-4 {
                    errors.push(format!("branch ratios sum to {sum}, expected 1.0"));
                }
                for (_, child) in children {
                    walk(child, ids, errors, Some(*axis));
                }
            }
        }
    }

    let mut ids = std::collections::HashSet::new();
    let mut errors = Vec::new();
    walk(&tab.root, &mut ids, &mut errors, None);
    if !ids.contains(&tab.focus) {
        errors.push(format!("focus {} is not a live leaf", tab.focus));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: Rect = Rect {
        x: 0,
        y: 0,
        rows: 21,
        cols: 21,
    };

    fn leaf_rect(panes: &[(PaneId, Rect)], id: PaneId) -> Rect {
        panes.iter().find(|(pid, _)| *pid == id).unwrap().1
    }

    // -- layout ---------------------------------------------------------

    #[test]
    fn tree_layout_single_leaf_fills_viewport() {
        let node = Node::Leaf(1);
        let panes = layout(&node, VIEWPORT);
        assert_eq!(panes, vec![(1, VIEWPORT)]);
    }

    #[test]
    fn tree_layout_two_leaf_horizontal_split_no_gaps_no_overlap() {
        let node = Node::Branch {
            axis: Axis::Horizontal,
            children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
        };
        let panes = layout(&node, VIEWPORT);
        let r1 = leaf_rect(&panes, 1);
        let r2 = leaf_rect(&panes, 2);
        assert_eq!(r1.x, 0);
        assert_eq!(r1.y, 0);
        assert_eq!(r1.rows, VIEWPORT.rows);
        assert_eq!(r2.rows, VIEWPORT.rows);
        // exactly one divider cell between them, no gap, no overlap
        assert_eq!(r2.x, r1.x + r1.cols + 1);
        assert_eq!(r1.cols + 1 + r2.cols, VIEWPORT.cols);
    }

    #[test]
    fn tree_layout_integer_tiling_exact_union_no_gaps() {
        let node = Node::Branch {
            axis: Axis::Horizontal,
            children: vec![
                (0.33, Node::Leaf(1)),
                (0.33, Node::Leaf(2)),
                (0.34, Node::Leaf(3)),
            ],
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            rows: 30,
            cols: 101,
        };
        let panes = layout(&node, viewport);
        let mut by_x: Vec<Rect> = panes.iter().map(|(_, r)| *r).collect();
        by_x.sort_by_key(|r| r.x);
        // contiguous with exactly 1-cell dividers, and the union covers the
        // full viewport width with no gap and no overlap.
        for w in by_x.windows(2) {
            assert_eq!(
                w[1].x,
                w[0].x + w[0].cols + 1,
                "gap or overlap between panes"
            );
        }
        assert_eq!(by_x[0].x, 0);
        let last = by_x.last().unwrap();
        assert_eq!(last.x + last.cols, viewport.cols);
        for r in &by_x {
            assert_eq!(r.rows, viewport.rows);
        }
    }

    // -- split ------------------------------------------------------------

    #[test]
    fn tree_split_horizontal_on_lone_leaf_wraps_and_focuses_new_pane() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Leaf(1),
            focus: 1,
        };
        split(&mut tab, VIEWPORT, Axis::Horizontal, 2).unwrap();
        assert_eq!(tab.focus, 2);
        match &tab.root {
            Node::Branch { axis, children } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
                assert!((children[0].0 - 0.5).abs() < 1e-6);
                assert!((children[1].0 - 0.5).abs() < 1e-6);
            }
            other => panic!("expected a Branch, got {other:?}"),
        }
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_split_same_axis_inserts_adjacent_halving_ratio() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.6, Node::Leaf(1)), (0.4, Node::Leaf(2))],
            },
            focus: 1,
        };
        // Wide enough that a 3-way split still clears MIN_COLS (VIEWPORT is
        // sized for the 2x2 nav grid, too narrow for a 3-way split here).
        let viewport = Rect {
            x: 0,
            y: 0,
            rows: 24,
            cols: 41,
        };
        split(&mut tab, viewport, Axis::Horizontal, 3).unwrap();
        assert_eq!(tab.focus, 3);
        match &tab.root {
            Node::Branch { children, .. } => {
                assert_eq!(children.len(), 3);
                assert_eq!(children[0].1, Node::Leaf(1));
                assert_eq!(children[1].1, Node::Leaf(3));
                assert_eq!(children[2].1, Node::Leaf(2));
                assert!((children[0].0 - 0.3).abs() < 1e-6);
                assert!((children[1].0 - 0.3).abs() < 1e-6);
                assert!((children[2].0 - 0.4).abs() < 1e-6);
            }
            other => panic!("expected a Branch, got {other:?}"),
        }
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_split_directional_inserts_on_requested_side() {
        let viewport = Rect {
            x: 0,
            y: 0,
            rows: 41,
            cols: 41,
        };
        for (direction, expected) in [(Dir::Left, vec![3, 1, 2]), (Dir::Right, vec![1, 3, 2])] {
            let mut tab = Tab {
                name: None,
                id: 0,
                root: Node::Branch {
                    axis: Axis::Horizontal,
                    children: vec![(0.6, Node::Leaf(1)), (0.4, Node::Leaf(2))],
                },
                focus: 1,
            };
            split_directional(&mut tab, viewport, direction, 3).unwrap();
            assert_eq!(leaves(&tab.root), expected);
            assert_eq!(tab.focus, 3);
            check_invariants(&tab).unwrap();
        }

        for (direction, expected) in [(Dir::Up, vec![3, 1, 2]), (Dir::Down, vec![1, 3, 2])] {
            let mut tab = Tab {
                name: None,
                id: 0,
                root: Node::Branch {
                    axis: Axis::Vertical,
                    children: vec![(0.6, Node::Leaf(1)), (0.4, Node::Leaf(2))],
                },
                focus: 1,
            };
            split_directional(&mut tab, viewport, direction, 3).unwrap();
            assert_eq!(leaves(&tab.root), expected);
            assert_eq!(tab.focus, 3);
            check_invariants(&tab).unwrap();
        }
    }

    #[test]
    fn tree_split_directional_wraps_lone_leaf_in_requested_order() {
        for (direction, axis, expected) in [
            (Dir::Left, Axis::Horizontal, vec![2, 1]),
            (Dir::Right, Axis::Horizontal, vec![1, 2]),
            (Dir::Up, Axis::Vertical, vec![2, 1]),
            (Dir::Down, Axis::Vertical, vec![1, 2]),
        ] {
            let mut tab = Tab {
                name: None,
                id: 0,
                root: Node::Leaf(1),
                focus: 1,
            };
            split_directional(&mut tab, VIEWPORT, direction, 2).unwrap();
            assert_eq!(leaves(&tab.root), expected);
            assert_eq!(tab.focus, 2);
            assert!(matches!(tab.root, Node::Branch { axis: actual, .. } if actual == axis));
            check_invariants(&tab).unwrap();
        }
    }

    #[test]
    fn tree_split_cross_axis_wraps_leaf_in_new_branch() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        };
        split(&mut tab, VIEWPORT, Axis::Vertical, 3).unwrap();
        match &tab.root {
            Node::Branch { axis, children } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(children.len(), 2);
                assert!(
                    (children[0].0 - 0.5).abs() < 1e-6,
                    "outer ratio for the split slot unchanged"
                );
                match &children[0].1 {
                    Node::Branch { axis, children } => {
                        assert_eq!(*axis, Axis::Vertical);
                        assert_eq!(children[0].1, Node::Leaf(1));
                        assert_eq!(children[1].1, Node::Leaf(3));
                    }
                    other => panic!("expected a nested V branch, got {other:?}"),
                }
            }
            other => panic!("expected a Branch, got {other:?}"),
        }
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_split_refused_below_min_size_leaves_tree_unchanged() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Leaf(1),
            focus: 1,
        };
        let tiny = Rect {
            x: 0,
            y: 0,
            rows: 24,
            cols: 16,
        };
        let before = tab.clone();
        let err = split(&mut tab, tiny, Axis::Horizontal, 2).unwrap_err();
        assert_eq!(
            err,
            SplitError::TooSmall {
                min_rows: MIN_ROWS,
                min_cols: MIN_COLS
            }
        );
        assert_eq!(tab, before, "tree must be unchanged on refusal");
    }

    #[test]
    fn tree_split_refusal_checks_each_axis_independently() {
        // Regression (spec review): the guard must be per-pane per-axis. A
        // short-wide pane (rows == MIN_ROWS exactly, passes both) has a
        // SMALLER min dimension than the tall-thin panes a split would
        // create (cols < MIN_COLS, rows huge) - a smallest-pane-by-min-dim
        // reduction checks only the former and wrongly allows the split.
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Vertical,
                children: vec![
                    (0.08, Node::Leaf(1)), // rows floor(29*.08)=2, cols 80
                    (
                        0.92,
                        Node::Branch {
                            axis: Axis::Horizontal,
                            children: vec![(0.9, Node::Leaf(2)), (0.1, Node::Leaf(3))],
                        },
                    ),
                ],
            },
            focus: 3, // 8 cols: exactly at minimum - any H split of it must refuse
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            rows: 30,
            cols: 80,
        };
        let before = tab.clone();
        let err = split(&mut tab, viewport, Axis::Horizontal, 4).unwrap_err();
        assert!(matches!(err, SplitError::TooSmall { .. }), "{err:?}");
        assert_eq!(tab, before, "tree must be unchanged on refusal");
    }

    // -- navigate -----------------------------------------------------------

    fn grid_2x2() -> Node {
        Node::Branch {
            axis: Axis::Vertical,
            children: vec![
                (
                    0.5,
                    Node::Branch {
                        axis: Axis::Horizontal,
                        children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
                    },
                ),
                (
                    0.5,
                    Node::Branch {
                        axis: Axis::Horizontal,
                        children: vec![(0.5, Node::Leaf(3)), (0.5, Node::Leaf(4))],
                    },
                ),
            ],
        }
    }

    #[test]
    fn tree_navigate_2x2_grid_geometric_corners() {
        // 1=TL 2=TR 3=BL 4=BR
        let node = grid_2x2();
        assert_eq!(navigate(&node, VIEWPORT, 1, Dir::Right), Some(2));
        assert_eq!(navigate(&node, VIEWPORT, 1, Dir::Down), Some(3));
        assert_eq!(navigate(&node, VIEWPORT, 2, Dir::Left), Some(1));
        assert_eq!(navigate(&node, VIEWPORT, 2, Dir::Down), Some(4));
        assert_eq!(navigate(&node, VIEWPORT, 3, Dir::Up), Some(1));
        assert_eq!(navigate(&node, VIEWPORT, 3, Dir::Right), Some(4));
        assert_eq!(navigate(&node, VIEWPORT, 4, Dir::Up), Some(2));
        assert_eq!(navigate(&node, VIEWPORT, 4, Dir::Left), Some(3));
        // no pane exists beyond an edge
        assert_eq!(navigate(&node, VIEWPORT, 1, Dir::Left), None);
        assert_eq!(navigate(&node, VIEWPORT, 1, Dir::Up), None);
    }

    #[test]
    fn tree_navigate_partial_overlap_picks_largest_overlap() {
        // Left column split V: pane 1 gets 70% of the height (tall), pane 2
        // gets 30% (short). Pane 3 spans the full height on the right.
        // Both 1 and 2 are "left of" 3 with the same edge distance, so the
        // tie-break must pick 1 (the larger vertical overlap with 3).
        let node = Node::Branch {
            axis: Axis::Horizontal,
            children: vec![
                (
                    0.4,
                    Node::Branch {
                        axis: Axis::Vertical,
                        children: vec![(0.7, Node::Leaf(1)), (0.3, Node::Leaf(2))],
                    },
                ),
                (0.6, Node::Leaf(3)),
            ],
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            rows: 20,
            cols: 20,
        };
        assert_eq!(navigate(&node, viewport, 3, Dir::Left), Some(1));
    }

    // -- close ----------------------------------------------------------

    #[test]
    fn tree_close_middle_of_three_redistributes_ratios_proportionally() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![
                    (0.2, Node::Leaf(1)),
                    (0.3, Node::Leaf(2)),
                    (0.5, Node::Leaf(3)),
                ],
            },
            focus: 2,
        };
        let empty = close(&mut tab, VIEWPORT, 2);
        assert!(!empty);
        match &tab.root {
            Node::Branch { children, .. } => {
                assert_eq!(children.len(), 2);
                let scale = 1.0 / (1.0 - 0.3_f32);
                assert!((children[0].0 - 0.2 * scale).abs() < 1e-5);
                assert!((children[1].0 - 0.5 * scale).abs() < 1e-5);
            }
            other => panic!("expected a Branch, got {other:?}"),
        }
        assert!(
            tab.focus == 1 || tab.focus == 3,
            "focus must re-anchor to a surviving sibling"
        );
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_close_collapses_nested_same_axis_branch() {
        // GP(V) [ P(H) [ Leaf(A), Q(V)[Leaf(B), Leaf(C)] ], Leaf(X) ]
        // Closing A collapses P to its single remaining child Q (axis V),
        // which nests inside GP (also axis V) - normalize must flatten it.
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Vertical,
                children: vec![
                    (
                        0.3,
                        Node::Branch {
                            axis: Axis::Horizontal,
                            children: vec![
                                (0.4, Node::Leaf(100)), // A
                                (
                                    0.6,
                                    Node::Branch {
                                        axis: Axis::Vertical,
                                        children: vec![
                                            (0.5, Node::Leaf(200)),
                                            (0.5, Node::Leaf(300)),
                                        ], // B, C
                                    },
                                ),
                            ],
                        },
                    ),
                    (0.7, Node::Leaf(400)), // X
                ],
            },
            focus: 400,
        };
        let empty = close(&mut tab, VIEWPORT, 100);
        assert!(!empty);
        match &tab.root {
            Node::Branch { axis, children } => {
                assert_eq!(*axis, Axis::Vertical);
                assert_eq!(children.len(), 3, "no nested same-axis branch must remain");
                assert_eq!(children[0].1, Node::Leaf(200));
                assert_eq!(children[1].1, Node::Leaf(300));
                assert_eq!(children[2].1, Node::Leaf(400));
                assert!((children[0].0 - 0.15).abs() < 1e-5);
                assert!((children[1].0 - 0.15).abs() < 1e-5);
                assert!((children[2].0 - 0.7).abs() < 1e-5);
            }
            other => panic!("expected a flattened Branch, got {other:?}"),
        }
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_close_last_pane_reports_tab_empty() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Leaf(1),
            focus: 1,
        };
        assert!(close(&mut tab, VIEWPORT, 1));
    }

    #[test]
    fn tree_close_unknown_pane_is_idempotent_noop() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        };
        let before = tab.clone();
        assert!(!close(&mut tab, VIEWPORT, 999));
        assert_eq!(tab, before);
    }

    // -- resize -----------------------------------------------------------

    #[test]
    fn tree_resize_transfers_ratio_between_neighbors() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        };
        let changed = resize(&mut tab, VIEWPORT, Dir::Right, RESIZE_STEP);
        assert!(changed);
        match &tab.root {
            Node::Branch { children, .. } => {
                assert!((children[0].0 - (0.5 + RESIZE_STEP)).abs() < 1e-5);
                assert!((children[1].0 - (0.5 - RESIZE_STEP)).abs() < 1e-5);
            }
            other => panic!("expected a Branch, got {other:?}"),
        }
        check_invariants(&tab).unwrap();
    }

    // -- set_seam_pos (x-d807) ------------------------------------------

    /// A wide viewport so MIN_COLS is a small fraction and the clamp does not
    /// dominate the assertions. 2 children => 1 divider => 200 available.
    const WIDE: Rect = Rect {
        x: 0,
        y: 0,
        rows: 41,
        cols: 201,
    };

    fn ratios(tab: &Tab, path: &[usize]) -> Vec<f32> {
        match node_at_path(&tab.root, path) {
            Node::Branch { children, .. } => children.iter().map(|(r, _)| *r).collect(),
            other => panic!("expected a Branch, got {other:?}"),
        }
    }

    /// Where the divider between the first two children actually lands.
    fn divider_x(tab: &Tab, a: PaneId) -> u16 {
        let r = leaf_rect(&layout(&tab.root, WIDE), a);
        r.x + r.cols
    }

    fn pair_tab() -> Tab {
        Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        }
    }

    #[test]
    fn tree_set_seam_pos_puts_the_divider_on_the_asked_for_cell() {
        let mut tab = pair_tab();
        assert!(set_seam_pos(&mut tab, WIDE, 1, 2, 150));
        assert_eq!(divider_x(&tab, 1), 150, "the divider lands where asked");
        let r = ratios(&tab, &[]);
        assert!((r[0] - 0.75).abs() < 1e-4, "150 of 200 available");
        assert!((r[0] + r[1] - 1.0).abs() < 1e-4, "sum conserved");
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_set_seam_pos_is_idempotent_and_drift_free() {
        // Absolute positions, so re-sending one changes nothing and a long drag
        // accumulates no error - the reason the wire carries a target rather
        // than a delta.
        let mut tab = pair_tab();
        assert!(set_seam_pos(&mut tab, WIDE, 1, 2, 140));
        assert!(
            !set_seam_pos(&mut tab, WIDE, 1, 2, 140),
            "re-sending the same target is not a change"
        );
        for p in [60, 160, 90, 124] {
            set_seam_pos(&mut tab, WIDE, 1, 2, p);
        }
        assert!(set_seam_pos(&mut tab, WIDE, 1, 2, 100));
        assert_eq!(
            divider_x(&tab, 1),
            100,
            "returning to a cell lands exactly, whatever the path there"
        );
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_set_seam_pos_clamps_at_minimum_size() {
        // AC5-EDGE: the divider stops at the clamp; it never crushes a pane to
        // zero or drives a ratio negative.
        let mut tab = pair_tab();
        assert!(set_seam_pos(&mut tab, WIDE, 1, 2, 5_000));
        let r = ratios(&tab, &[]);
        assert!(r[1] > 0.0, "the far pane keeps a minimum: {r:?}");
        assert!((r[0] + r[1] - 1.0).abs() < 1e-4, "sum conserved: {r:?}");
        assert!(
            divider_x(&tab, 1) <= WIDE.cols - MIN_COLS,
            "the far pane keeps at least MIN_COLS of room"
        );
        check_invariants(&tab).unwrap();
        // Already clamped: pushing further the same way does nothing.
        assert!(!set_seam_pos(&mut tab, WIDE, 1, 2, 5_000));
    }

    #[test]
    fn tree_set_seam_pos_measures_the_branch_child_not_the_flanking_pane() {
        // The pane that flanks a seam is NOT its branch child once axes
        // alternate more than one level: Horizontal -> Vertical -> Horizontal
        // is legal, so P3 below spans only part of A's width. Measuring from
        // P3's rect instead of A's would land the divider well short of the
        // cell the operator pointed at, which is why the position-to-ratio
        // conversion lives here rather than client-side.
        //
        //   Root(H): [ A , B ]   A(V): [ P1 , D ]   D(H): [ P2 , P3 ]   B = P4
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![
                    (
                        0.5,
                        Node::Branch {
                            axis: Axis::Vertical,
                            children: vec![
                                (0.5, Node::Leaf(1)),
                                (
                                    0.5,
                                    Node::Branch {
                                        axis: Axis::Horizontal,
                                        children: vec![(0.5, Node::Leaf(2)), (0.5, Node::Leaf(3))],
                                    },
                                ),
                            ],
                        },
                    ),
                    (0.5, Node::Leaf(4)),
                ],
            },
            focus: 1,
        };
        check_invariants(&tab).unwrap();
        let panes = layout(&tab.root, WIDE);
        let p3 = leaf_rect(&panes, 3);
        let p1 = leaf_rect(&panes, 1);
        assert!(
            p3.x > p1.x && p3.cols < p1.cols,
            "P3 sits deeper than its branch child, so its extent is a strict \
             subset of A's: p3={p3:?} p1={p1:?}"
        );

        // Grab the outer A|B divider at a row inside D, which flanks P3 and P4.
        assert!(set_seam_pos(&mut tab, WIDE, 3, 4, 120));
        assert_eq!(
            divider_x(&tab, 1),
            120,
            "the outer divider lands on the asked-for cell, measured from A"
        );
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_set_seam_pos_is_exact_at_arbitrary_nesting_depth() {
        // The fix must generalise, not just handle the one shape that exposed
        // it. Four levels of alternating axes, with the seam addressed by the
        // DEEPEST pane on each side - the worst case for confusing a pane's
        // extent with its branch child's.
        //
        //   Root(H): [ A , B ]
        //     A(V): [ P1 , D ]          D(H): [ P2 , E ]      E(V): [ P3, P4 ]
        //     B(V): [ P5 , F ]          F(H): [ P6 , G ]      G(V): [ P7, P8 ]
        let deep = |l: PaneId, m: PaneId, r1: PaneId, r2: PaneId| Node::Branch {
            axis: Axis::Vertical,
            children: vec![
                (0.5, Node::Leaf(l)),
                (
                    0.5,
                    Node::Branch {
                        axis: Axis::Horizontal,
                        children: vec![
                            (0.5, Node::Leaf(m)),
                            (
                                0.5,
                                Node::Branch {
                                    axis: Axis::Vertical,
                                    children: vec![(0.5, Node::Leaf(r1)), (0.5, Node::Leaf(r2))],
                                },
                            ),
                        ],
                    },
                ),
            ],
        };
        let mk = || Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, deep(1, 2, 3, 4)), (0.5, deep(5, 6, 7, 8))],
            },
            focus: 1,
        };
        let mut tab = mk();
        check_invariants(&tab).unwrap();

        // Panes 4 and 7 are three levels deep on either side of the OUTER seam.
        let panes = layout(&tab.root, WIDE);
        let (p4, p7) = (leaf_rect(&panes, 4), leaf_rect(&panes, 7));
        assert!(
            p4.cols < leaf_rect(&panes, 1).cols,
            "pane 4 spans a strict subset of its branch child"
        );
        assert!(p4.x + p4.cols < p7.x, "and does not even abut the seam");

        // Every legal target cell must land exactly, addressed by deep panes.
        for target in [60u16, 140, 75] {
            let mut t = mk();
            assert!(
                set_seam_pos(&mut t, WIDE, 4, 7, target),
                "deep pair addresses the outer seam"
            );
            let r = leaf_rect(&layout(&t.root, WIDE), 1);
            assert_eq!(
                r.x + r.cols,
                target,
                "outer divider lands on {target} regardless of how deep the \
                 addressing panes sit"
            );
            check_invariants(&t).unwrap();
        }

        // And an INNER seam addressed by its own deep pair stays independent.
        assert!(set_seam_pos(&mut tab, WIDE, 3, 4, 0));
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_set_seam_pos_refuses_a_nan_ratio_instead_of_panicking() {
        // f32::clamp panics on a NaN bound, and `lo > hi` cannot catch one -
        // every comparison against NaN is false, so it would sail through.
        // check_invariants has the same blind spot, so a NaN can reach here
        // unflagged; refusing is the only safe answer.
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(f32::NAN, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        };
        assert!(
            !set_seam_pos(&mut tab, WIDE, 1, 2, 120),
            "refuses rather than panicking in clamp"
        );
        // Note the tree cannot be compared with assert_eq! here: NaN != NaN, so
        // a tree holding one never equals itself. That the derived PartialEq is
        // useless on such a tree is the same blind spot the guard exists for.
        let r = ratios(&tab, &[]);
        assert!(r[0].is_nan(), "the NaN child is untouched");
        assert_eq!(r[1], 0.5, "and so is its neighbour");
    }

    #[test]
    fn tree_set_seam_pos_refuses_a_stale_or_non_adjacent_pair() {
        // AC4-ERR: the address IS the validation. A pane that has gone, or a
        // pair that does not flank one seam, leaves the tree untouched.
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![
                    (0.34, Node::Leaf(1)),
                    (0.33, Node::Leaf(2)),
                    (0.33, Node::Leaf(3)),
                ],
            },
            focus: 1,
        };
        let before = tab.root.clone();
        assert!(!set_seam_pos(&mut tab, WIDE, 1, 99, 120), "pane 99 is gone");
        assert!(
            !set_seam_pos(&mut tab, WIDE, 1, 3, 120),
            "1 and 3 are not adjacent"
        );
        assert!(
            !set_seam_pos(&mut tab, WIDE, 2, 1, 120),
            "reversed order is not a seam"
        );
        assert!(
            !set_seam_pos(&mut tab, WIDE, 1, 1, 120),
            "a pane does not flank itself"
        );
        assert_eq!(tab.root, before, "every refusal left the tree untouched");
        // The genuinely adjacent pairs still work, and only touch their pair.
        assert!(set_seam_pos(&mut tab, WIDE, 2, 3, 150));
        let r = ratios(&tab, &[]);
        assert!(
            (r[0] - 0.34).abs() < 1e-4,
            "the uninvolved child is untouched"
        );
        assert!((r.iter().sum::<f32>() - 1.0).abs() < 1e-4);
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_set_seam_pos_resolves_a_seam_flanked_by_nested_panes() {
        // A seam runs past every pane in the children it separates, so naming
        // any pane from each side must pick the same seam. Here the left child
        // is a vertical stack of 2 and 3; both name the same boundary with 4.
        let stacked = Node::Branch {
            axis: Axis::Vertical,
            children: vec![(0.5, Node::Leaf(2)), (0.5, Node::Leaf(3))],
        };
        let mk = || Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, stacked.clone()), (0.5, Node::Leaf(4))],
            },
            focus: 4,
        };
        let (mut via_top, mut via_bottom) = (mk(), mk());
        assert!(set_seam_pos(&mut via_top, WIDE, 2, 4, 140));
        assert!(set_seam_pos(&mut via_bottom, WIDE, 3, 4, 140));
        assert_eq!(
            via_top.root, via_bottom.root,
            "either flanking pane addresses the same seam"
        );
        assert!((ratios(&via_top, &[])[0] - 0.7).abs() < 1e-4);
        // The stack's own inner ratios are untouched by the outer resize.
        assert_eq!(ratios(&via_top, &[0]), vec![0.5, 0.5]);
        check_invariants(&via_top).unwrap();
    }

    #[test]
    fn tree_resize_noop_when_no_matching_ancestor() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Leaf(1),
            focus: 1,
        };
        let before = tab.clone();
        assert!(!resize(&mut tab, VIEWPORT, Dir::Right, RESIZE_STEP));
        assert_eq!(tab, before);
    }

    // -- replace_leaf (open-here, x-9f75) --------------------------------

    #[test]
    fn tree_replace_leaf_lone_root_swaps_and_moves_focus() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Leaf(1),
            focus: 1,
        };
        assert!(replace_leaf(&mut tab, 1, 9));
        assert_eq!(tab.root, Node::Leaf(9));
        assert_eq!(tab.focus, 9);
    }

    #[test]
    fn tree_replace_leaf_nested_preserves_geometry() {
        // A displaced pane deep in the tree is swapped in place: ratios,
        // branch axes, and sibling ids are all untouched - only pane 2 flips.
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![
                    (0.6, Node::Leaf(1)),
                    (
                        0.4,
                        Node::Branch {
                            axis: Axis::Vertical,
                            children: vec![(0.5, Node::Leaf(2)), (0.5, Node::Leaf(3))],
                        },
                    ),
                ],
            },
            focus: 2,
        };
        let before_layout = layout(&tab.root, VIEWPORT);
        assert!(replace_leaf(&mut tab, 2, 9));
        assert_eq!(tab.focus, 9);
        // Same rects, only the id at pane 2's slot changed.
        let after_layout = layout(&tab.root, VIEWPORT);
        let rect_of =
            |ls: &[(PaneId, Rect)], id| ls.iter().find(|(p, _)| *p == id).map(|(_, r)| *r);
        assert_eq!(rect_of(&before_layout, 2), rect_of(&after_layout, 9));
        assert_eq!(rect_of(&before_layout, 1), rect_of(&after_layout, 1));
        assert_eq!(rect_of(&before_layout, 3), rect_of(&after_layout, 3));
        assert_eq!(rect_of(&after_layout, 2), None);
        check_invariants(&tab).unwrap();
    }

    #[test]
    fn tree_replace_leaf_absent_is_noop() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        };
        let before = tab.clone();
        assert!(!replace_leaf(&mut tab, 42, 9));
        assert_eq!(tab, before);
    }

    #[test]
    fn tree_replace_leaf_unfocused_keeps_focus() {
        let mut tab = Tab {
            name: None,
            id: 0,
            root: Node::Branch {
                axis: Axis::Horizontal,
                children: vec![(0.5, Node::Leaf(1)), (0.5, Node::Leaf(2))],
            },
            focus: 1,
        };
        assert!(replace_leaf(&mut tab, 2, 9));
        assert_eq!(tab.focus, 1);
    }

    // -- move_leaf (x-aa95) -------------------------------------------------

    /// Wider than [`VIEWPORT`] because a move concentrates panes into one row:
    /// three panes side by side need `3 * MIN_COLS` plus two divider cells, so
    /// the shared 21-column viewport would refuse every relocation on size
    /// alone and prove nothing about the surgery.
    const MOVE_VIEWPORT: Rect = Rect {
        x: 0,
        y: 0,
        rows: 21,
        cols: 40,
    };

    fn grid_2x2_tab() -> Tab {
        Tab {
            id: 0,
            root: grid_2x2(),
            focus: 2,
            name: None,
        }
    }

    #[test]
    fn tree_move_leaf_relocates_a_pane_into_a_sibling_row() {
        // AC1-HP: grid 1 2 / 3 4; move 2 to the seam between 3 and 4 =>
        // 1 spans the top, 3 2 4 share the bottom.
        let mut tab = grid_2x2_tab();
        move_leaf(&mut tab, MOVE_VIEWPORT, 2, 3, Dir::Right).expect("the move is legal here");
        let Node::Branch { axis, children } = &tab.root else {
            panic!("root collapsed to a leaf: {:?}", tab.root);
        };
        assert_eq!(*axis, Axis::Vertical);
        assert_eq!(children.len(), 2, "still a top row and a bottom row");
        assert_eq!(children[0].1, Node::Leaf(1), "1 absorbed the whole top row");
        let Node::Branch {
            axis: bottom_axis,
            children: bottom,
        } = &children[1].1
        else {
            panic!("bottom row is not a branch: {:?}", children[1].1);
        };
        assert_eq!(*bottom_axis, Axis::Horizontal);
        assert_eq!(
            bottom.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(),
            vec![Node::Leaf(3), Node::Leaf(2), Node::Leaf(4)],
            "2 landed between 3 and 4, in that order"
        );
    }

    #[test]
    fn tree_move_leaf_preserves_the_pane_set_and_invariants() {
        // Invariant: relocation is pure surgery - no pane is lost, duplicated,
        // or minted, and the tree stays well-formed.
        let mut tab = grid_2x2_tab();
        let before: Vec<PaneId> = {
            let mut v = leaves(&tab.root);
            v.sort_unstable();
            v
        };
        move_leaf(&mut tab, MOVE_VIEWPORT, 2, 3, Dir::Right).expect("the move is legal here");
        let after: Vec<PaneId> = {
            let mut v = leaves(&tab.root);
            v.sort_unstable();
            v
        };
        assert_eq!(before, after, "the pane set must be identical");
        check_invariants(&tab).expect("tree must stay well-formed after a move");
    }

    #[test]
    fn tree_move_leaf_focus_follows_the_moved_pane() {
        let mut tab = grid_2x2_tab();
        tab.focus = 1;
        move_leaf(&mut tab, MOVE_VIEWPORT, 2, 3, Dir::Right).expect("the move is legal here");
        assert_eq!(tab.focus, 2, "the relocated pane takes focus");
    }

    #[test]
    fn tree_move_leaf_refuses_a_move_that_would_undersize_a_pane() {
        // AC4-ERR: the refusal leaves the tree byte-identical, so the client
        // can flash a rejection without re-syncing.
        let mut tab = grid_2x2_tab();
        let before = tab.clone();
        // Wide enough for the 2x2 it starts as, too narrow for the three-pane
        // row the move would produce - so the refusal is the move's doing, not
        // a viewport that was already illegal.
        let cramped = Rect {
            x: 0,
            y: 0,
            rows: 8,
            cols: 20,
        };
        assert!(
            layout(&tab.root, cramped)
                .iter()
                .all(|(_, r)| r.rows >= MIN_ROWS && r.cols >= MIN_COLS),
            "precondition: the starting grid must itself be legal here"
        );
        assert_eq!(
            move_leaf(&mut tab, cramped, 2, 3, Dir::Right),
            Err(MoveError::TooSmall {
                min_rows: MIN_ROWS,
                min_cols: MIN_COLS
            }),
            "three panes cannot share a row this narrow"
        );
        assert_eq!(tab, before, "tree must be unchanged on refusal");
    }

    #[test]
    fn tree_move_leaf_is_a_no_op_for_ids_not_in_the_tree() {
        // AC6-FR: a stale drop names a pane another client already closed.
        let mut tab = grid_2x2_tab();
        let before = tab.clone();
        assert_eq!(
            move_leaf(&mut tab, MOVE_VIEWPORT, 99, 3, Dir::Right),
            Err(MoveError::PaneGone)
        );
        assert_eq!(tab, before);
        assert_eq!(
            move_leaf(&mut tab, MOVE_VIEWPORT, 2, 99, Dir::Right),
            Err(MoveError::PaneGone)
        );
        assert_eq!(tab, before);
    }

    #[test]
    fn tree_move_leaf_onto_itself_is_a_no_op() {
        // AC5-EDGE: dropping a pane on its own origin cancels rather than
        // running a remove+reinsert that would churn ratios for no reason.
        let mut tab = grid_2x2_tab();
        let before = tab.clone();
        assert_eq!(
            move_leaf(&mut tab, MOVE_VIEWPORT, 2, 2, Dir::Right),
            Err(MoveError::Origin)
        );
        assert_eq!(tab, before);
    }

    #[test]
    fn tree_move_leaf_refuses_every_move_a_single_pane_tab_can_express() {
        // A lone pane has nowhere to go. Both addressable forms refuse, and
        // neither reaches the `Some(None)` removal branch - which is why that
        // branch folds into PaneGone instead of carrying its own variant.
        let mut tab = Tab {
            id: 0,
            root: Node::Leaf(1),
            focus: 1,
            name: None,
        };
        let before = tab.clone();
        assert_eq!(
            move_leaf(&mut tab, MOVE_VIEWPORT, 1, 1, Dir::Right),
            Err(MoveError::Origin),
            "naming itself as the target is an origin drop"
        );
        assert_eq!(
            move_leaf(&mut tab, MOVE_VIEWPORT, 1, 2, Dir::Right),
            Err(MoveError::PaneGone),
            "there is no second pane to target"
        );
        assert_eq!(tab, before);
    }

    #[test]
    fn tree_move_leaf_collapses_the_branch_it_emptied() {
        // Moving 2 out of the top row leaves that row single-child; the
        // collapse+normalize pass must fold it away rather than leave a
        // one-child branch standing (which check_invariants rejects).
        let mut tab = grid_2x2_tab();
        move_leaf(&mut tab, MOVE_VIEWPORT, 2, 3, Dir::Right).expect("the move is legal here");
        check_invariants(&tab).expect("no single-child branch may survive");
        // And the fold is real: the top row is the bare leaf, not a wrapper.
        let Node::Branch { children, .. } = &tab.root else {
            panic!("root collapsed unexpectedly");
        };
        assert!(matches!(children[0].1, Node::Leaf(1)));
    }

    #[test]
    fn tree_move_leaf_across_axes_wraps_the_target() {
        // Moving below a target whose parent runs Horizontal must wrap that
        // target in a Vertical branch, the same way split_directional does.
        let mut tab = grid_2x2_tab();
        move_leaf(&mut tab, MOVE_VIEWPORT, 2, 3, Dir::Down).expect("the move is legal here");
        check_invariants(&tab).expect("cross-axis relocation stays well-formed");
        let panes = layout(&tab.root, MOVE_VIEWPORT);
        let r2 = leaf_rect(&panes, 2);
        let r3 = leaf_rect(&panes, 3);
        assert!(r2.y > r3.y, "2 sits below 3 after a Down move");
    }
}
