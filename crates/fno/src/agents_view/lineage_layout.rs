//! The lineage forest: depth cap, the CHILD-edge parent read, and the
//! pre-order layout the sideline paints.

use std::collections::HashMap;

use super::AgentRow;

/// Rendering cap on lineage depth: a pathological chain must not push
/// rows off-screen (same bounded-steps posture `crown_indent` held).
pub const MAX_LINEAGE_DEPTH: usize = 8;

/// The parent edge the sideline nests on: the row's `spawned_by_session`
/// only when the edge is CHILD. A PEER handoff, or a pre-v32 row with no
/// word, roots at depth 0 beside its spawner.
pub fn lineage_parent(row: &AgentRow) -> Option<&str> {
    match row.lineage_kind.as_deref() {
        Some("child") => row.spawned_by_session.as_deref(),
        _ => None,
    }
}

/// Join rows into a lineage forest and lay it out for rendering.
/// Returns `(order, depths)`: `order` is the render order as INPUT INDICES in
/// stable pre-order (each row beneath its parent), and `depths[i]` is row
/// `i`'s lineage depth. Keyed by ROW IDENTITY (the input index), never by a
/// display name - two rows can legitimately share a name (two bare panes both
/// labeled `shell`), and a name-keyed join maps both to one row's depth.
///
/// The join: `parent_of` on one row is matched against `id_of` on the others.
/// Three rules, all load-bearing on live registry data:
/// - a row whose parent is ABSENT from the set renders as a root (depth 0),
///   never an error - the parent may sit in another section, another project,
///   or predate the field entirely;
/// - depth is capped at [`MAX_LINEAGE_DEPTH`];
/// - cycles are possible: the parent value is ambient-captured from an
///   environment variable, never validated at write time, so the upward walk
///   carries its own path and breaks a revisit by rooting the cycle's entry.
///   A self-edge or an A->B->A pair terminates; it never hangs.
///
/// Deterministic: roots and siblings keep input order, so a set with no parent
/// edges lays out in input order at depth 0 (byte-identical to a flat list).
pub fn lineage_layout<T>(
    rows: &[T],
    id_of: impl Fn(&T) -> Option<&str>,
    parent_of: impl Fn(&T) -> Option<&str>,
) -> (Vec<usize>, Vec<usize>) {
    let n = rows.len();
    // id -> index; the first row wins a duplicated id (ids are only as
    // trustworthy as the env they were captured from).
    let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(n);
    for (i, r) in rows.iter().enumerate() {
        if let Some(id) = id_of(r) {
            by_id.entry(id).or_insert(i);
        }
    }
    let parent_idx: Vec<Option<usize>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            parent_of(r)
                .and_then(|p| by_id.get(p).copied())
                // A row naming its own id roots immediately.
                .filter(|&p| p != i)
        })
        .collect();

    // Depth by memoized upward walk. The walk stops at a resolved ancestor
    // (its depth is known), a root (no in-set parent -> depth 0), or a
    // revisit (cycle -> the revisited node roots at depth 0). Unwinding is
    // uniform: every path node takes its (already-assigned) parent's depth+1.
    let mut depth: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        if depth[i].is_some() {
            continue;
        }
        let mut path: Vec<usize> = Vec::new();
        let mut cur = i;
        loop {
            if depth[cur].is_some() {
                break;
            }
            if path.contains(&cur) {
                depth[cur] = Some(0);
                break;
            }
            path.push(cur);
            match parent_idx[cur] {
                Some(p) => cur = p,
                None => {
                    depth[cur] = Some(0);
                    break;
                }
            }
        }
        for &node in path.iter().rev() {
            // Root, anchor, and cycle-entry nodes already hold their depth;
            // only the still-unassigned chain nodes take parent_depth + 1.
            if depth[node].is_none() {
                let parent_depth = parent_idx[node].and_then(|p| depth[p]).unwrap_or(0);
                depth[node] = Some((parent_depth + 1).min(MAX_LINEAGE_DEPTH));
            }
        }
    }

    // Pre-order emission: roots (no in-set parent, or a cycle entry at depth
    // 0) in input order, children in input order beneath them. The emitted
    // guard covers a cycle's back-edge: a member already reached as a
    // descendant is never duplicated.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut has_parent = vec![false; n];
    for (i, p) in parent_idx.iter().enumerate() {
        if let Some(p) = p {
            children[*p].push(i);
            has_parent[i] = true;
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut emitted = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for r in 0..n {
        if has_parent[r] && depth[r] != Some(0) {
            continue;
        }
        stack.push(r);
        while let Some(x) = stack.pop() {
            if emitted[x] {
                continue;
            }
            emitted[x] = true;
            order.push(x);
            for &c in children[x].iter().rev() {
                if !emitted[c] {
                    stack.push(c);
                }
            }
        }
    }
    // Defensive tail: nothing should reach here unemitted (every row is a root
    // or a descendant of one), but a forest the walk could not classify still
    // renders rather than vanishing.
    for (i, was_emitted) in emitted.iter().enumerate().take(n) {
        if !was_emitted {
            order.push(i);
        }
    }
    let depths = (0..n).map(|i| depth[i].unwrap_or(0)).collect();
    (order, depths)
}
