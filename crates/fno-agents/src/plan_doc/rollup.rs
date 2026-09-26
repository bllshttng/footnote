//! Epic rollup counters and derived wave strata, ported 1:1 from
//! `cli/src/fno/plan/_rollup.py`.

use serde_json::Value;

use super::node_accessors as na;

const DONE: &str = "done";
const IN_FLIGHT: &[&str] = &["in_progress", "claimed", "in_review"];
const BLOCKED: &str = "blocked";

pub const ROLLUP_KEYS: &[&str] = &[
    "children_total",
    "children_done",
    "children_in_flight",
    "children_blocked",
    "progress",
];

/// The rollup counters for one epic.
#[derive(Debug, Clone, PartialEq)]
pub struct Rollup {
    pub total: i64,
    pub done: i64,
    pub in_flight: i64,
    pub blocked: i64,
    pub progress: String,
}

impl Rollup {
    fn zero() -> Self {
        Rollup {
            total: 0,
            done: 0,
            in_flight: 0,
            blocked: 0,
            progress: "0/0".to_string(),
        }
    }
}

/// The LEAF rollup counters for `epic_id`. A direct leaf child counts once by
/// its derived status; a direct child that is itself an epic recurses one
/// level and folds its leaves in. Depth caps at mission -> epic -> leaf; the
/// seen guard bounds an epic-parent cycle.
pub fn compute_rollup(epic_id: &str, entries: &[Value]) -> Rollup {
    compute_rollup_seen(epic_id, entries, &mut Default::default())
}

fn compute_rollup_seen(
    epic_id: &str,
    entries: &[Value],
    seen: &mut std::collections::HashSet<String>,
) -> Rollup {
    if seen.contains(epic_id) {
        return Rollup::zero();
    }
    seen.insert(epic_id.to_string());
    let mut r = Rollup::zero();
    for child in na::direct_children(entries, epic_id) {
        if na::s_field(child, "type") == Some("epic") {
            let Some(cid) = na::s_field(child, "id") else {
                // An id-less epic child would recurse on None and fold in every
                // top-level node. Skip it rather than miscount.
                continue;
            };
            let sub = compute_rollup_seen(cid, entries, seen);
            r.total += sub.total;
            r.done += sub.done;
            r.in_flight += sub.in_flight;
            r.blocked += sub.blocked;
            continue;
        }
        r.total += 1;
        let st = na::s_field(child, "status").unwrap_or("");
        if st == DONE {
            r.done += 1;
        } else if IN_FLIGHT.contains(&st) {
            r.in_flight += 1;
        } else if st == BLOCKED {
            r.blocked += 1;
        }
    }
    r.progress = format!("{}/{}", r.done, r.total);
    r
}

/// Derive topological wave strata for an epic's DIRECT children (AC4).
///
/// A child with no intra-epic blocker is wave 0; otherwise its wave is
/// 1 + max(wave of its intra-epic blockers) (longest-path strata). Only
/// blocked_by edges between siblings of the same epic count. Returns
/// (wave_by_child_id, max_wave) where max_wave is -1 for a childless epic (so
/// the caller's `waves` summary is max_wave + 1 == 0). Cycle-safe: cycle
/// members collapse to wave 0; an acyclic dependent of a cycle still lands at
/// 1 + max(blocker wave).
pub fn compute_waves(
    epic_id: &str,
    entries: &[Value],
) -> (std::collections::BTreeMap<String, i64>, i64) {
    let children = na::direct_children(entries, epic_id);
    // All child ids first, THEN the edges: a blocker list may name a sibling
    // that appears later in the entry order.
    let mut child_ids: std::collections::BTreeSet<String> = Default::default();
    for c in &children {
        if let Some(cid) = na::s_field(c, "id") {
            child_ids.insert(cid.to_string());
        }
    }
    let mut blockers: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for c in &children {
        let Some(cid) = na::s_field(c, "id") else {
            continue;
        };
        let empty: Vec<Value> = Vec::new();
        let edges: Vec<String> = na::s_list(c, "blocked_by")
            .unwrap_or(&empty)
            .iter()
            .filter_map(|b| b.as_str())
            .filter(|b| child_ids.contains(*b))
            .map(str::to_string)
            .collect();
        blockers.insert(cid.to_string(), edges);
    }

    // Kahn-style leveling: a node gets its wave only once ALL its intra-epic
    // blockers have waves, so the result is deterministic and a longest-path
    // stratification. When the fixpoint stalls, only the true cycle members
    // collapse to wave 0; an acyclic dependent of a cycle still lands at
    // 1 + max(blocker wave).
    let mut wave: std::collections::BTreeMap<String, i64> = Default::default();
    let mut remaining: std::collections::BTreeSet<String> = child_ids.clone();
    while !remaining.is_empty() {
        let mut progressed = false;
        for cid in remaining.clone() {
            let Some(bl) = blockers.get(&cid) else {
                continue;
            };
            if bl.iter().all(|b| wave.contains_key(b)) {
                let max_blocker = bl
                    .iter()
                    .filter_map(|b| wave.get(b))
                    .max()
                    .copied()
                    .unwrap_or(-1);
                wave.insert(cid.clone(), 1 + max_blocker);
                remaining.remove(&cid);
                progressed = true;
            }
        }
        if progressed {
            continue;
        }
        // Stalled: collapse only the true cycle members to 0, then loop.
        let mut cyclic: Vec<String> = remaining
            .iter()
            .filter(|c| on_cycle(c, &blockers, &remaining))
            .cloned()
            .collect();
        if cyclic.is_empty() {
            cyclic = remaining.iter().cloned().collect();
        }
        for c in &cyclic {
            wave.insert(c.clone(), 0);
            remaining.remove(c);
        }
    }
    let max_wave = wave.values().copied().max().unwrap_or(-1);
    (wave, max_wave)
}

/// True iff `start` can reach itself over unresolved blocker edges within
/// `scope` - a cycle member, not merely a dependent of one.
fn on_cycle(
    start: &str,
    blockers: &std::collections::BTreeMap<String, Vec<String>>,
    scope: &std::collections::BTreeSet<String>,
) -> bool {
    let mut stack: Vec<String> = blockers
        .get(start)
        .into_iter()
        .flatten()
        .filter(|b| scope.contains(*b))
        .cloned()
        .collect();
    let mut seen: std::collections::HashSet<String> = Default::default();
    while let Some(cur) = stack.pop() {
        if cur == start {
            return true;
        }
        if seen.contains(&cur) {
            continue;
        }
        seen.insert(cur.clone());
        stack.extend(
            blockers
                .get(&cur)
                .into_iter()
                .flatten()
                .filter(|b| scope.contains(*b))
                .cloned()
                .collect::<Vec<_>>(),
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn n(
        id: &str,
        parent: Option<&str>,
        status: &str,
        type_: &str,
        blocked_by: Vec<&str>,
    ) -> Value {
        json!({
            "id": id, "parent": parent, "status": status, "type": type_,
            "blocked_by": blocked_by,
        })
    }

    #[test]
    fn direct_children_counted_by_status() {
        let entries = vec![
            n("e", None, "ready", "epic", vec![]),
            n("c1", Some("e"), "done", "feature", vec![]),
            n("c2", Some("e"), "claimed", "feature", vec![]),
            n("c3", Some("e"), "blocked", "feature", vec![]),
            n("c4", Some("e"), "ready", "feature", vec![]),
            n("other", Some("x"), "ready", "feature", vec![]),
        ];
        let r = compute_rollup("e", &entries);
        assert_eq!(
            r,
            Rollup {
                total: 4,
                done: 1,
                in_flight: 1,
                blocked: 1,
                progress: "1/4".to_string()
            }
        );
    }

    #[test]
    fn in_review_counts_as_in_flight() {
        let entries = vec![
            n("e", None, "ready", "epic", vec![]),
            n("c1", Some("e"), "in_review", "feature", vec![]),
        ];
        let r = compute_rollup("e", &entries);
        assert_eq!(r.in_flight, 1);
        assert_eq!(r.progress, "0/1");
    }

    #[test]
    fn childless_epic_zeroes() {
        let entries = vec![n("e", None, "ready", "epic", vec![])];
        assert_eq!(compute_rollup("e", &entries), Rollup::zero());
    }

    #[test]
    fn mission_aggregates_child_epic_leaves() {
        let entries = vec![
            n("M", None, "ready", "epic", vec![]),
            n("E", Some("M"), "ready", "epic", vec![]),
            n("e1", Some("E"), "done", "feature", vec![]),
            n("e2", Some("E"), "ready", "feature", vec![]),
            n("e3", Some("E"), "ready", "feature", vec![]),
            n("L", Some("M"), "ready", "feature", vec![]),
        ];
        let m = compute_rollup("M", &entries);
        assert_eq!(m.total, 4);
        assert_eq!(m.done, 1);
        assert_eq!(m.progress, "1/4");
        let e = compute_rollup("E", &entries);
        assert_eq!(e.total, 3);
        assert_eq!(e.done, 1);
        assert_eq!(e.progress, "1/3");
    }

    #[test]
    fn idless_epic_child_skipped_not_miscounted() {
        let entries = vec![
            n("M", None, "ready", "epic", vec![]),
            json!({"id": null, "parent": "M", "type": "epic", "status": "ready"}),
            n("top", None, "done", "feature", vec![]),
        ];
        let r = compute_rollup("M", &entries);
        assert_eq!(r.total, 0);
        assert_eq!(r.progress, "0/0");
    }

    #[test]
    fn epic_parent_cycle_terminates() {
        let entries = vec![
            n("A", Some("B"), "ready", "epic", vec![]),
            n("B", Some("A"), "ready", "epic", vec![]),
        ];
        let r = compute_rollup("A", &entries);
        assert_eq!(r.progress, "0/0");
    }

    #[test]
    fn waves_derive_from_intra_epic_edges() {
        let entries = vec![
            n("E", None, "ready", "epic", vec![]),
            n("A", Some("E"), "ready", "feature", vec![]),
            n("B", Some("E"), "ready", "feature", vec!["A"]),
            n("C", Some("E"), "ready", "feature", vec!["A"]),
            n("D", Some("E"), "ready", "feature", vec!["B"]),
        ];
        let (wave, max_wave) = compute_waves("E", &entries);
        assert_eq!(
            wave,
            BTreeMap::from([
                ("A".to_string(), 0),
                ("B".to_string(), 1),
                ("C".to_string(), 1),
                ("D".to_string(), 2),
            ])
        );
        assert_eq!(max_wave + 1, 3);
    }

    #[test]
    fn waves_recompute_on_edge_removal() {
        let entries = vec![
            n("E", None, "ready", "epic", vec![]),
            n("A", Some("E"), "ready", "feature", vec![]),
            n("B", Some("E"), "ready", "feature", vec!["A"]),
            n("D", Some("E"), "ready", "feature", vec!["B"]),
        ];
        assert_eq!(compute_waves("E", &entries).0["D"], 2);
        let entries = vec![
            n("E", None, "ready", "epic", vec![]),
            n("A", Some("E"), "ready", "feature", vec![]),
            n("B", Some("E"), "ready", "feature", vec!["A"]),
            n("D", Some("E"), "ready", "feature", vec![]),
        ];
        assert_eq!(compute_waves("E", &entries).0["D"], 0);
    }

    #[test]
    fn waves_ignore_cross_epic_blockers() {
        let entries = vec![
            n("E", None, "ready", "epic", vec![]),
            n("A", Some("E"), "ready", "feature", vec!["x-external"]),
            n("ext", None, "ready", "feature", vec![]),
        ];
        let (wave, max_wave) = compute_waves("E", &entries);
        assert_eq!(wave, BTreeMap::from([("A".to_string(), 0)]));
        assert_eq!(max_wave, 0);
    }

    #[test]
    fn waves_childless_epic() {
        let entries = vec![n("E", None, "ready", "epic", vec![])];
        let (wave, max_wave) = compute_waves("E", &entries);
        assert!(wave.is_empty());
        assert_eq!(max_wave + 1, 0);
    }

    #[test]
    fn waves_sibling_cycle_terminates() {
        let entries = vec![
            n("E", None, "ready", "epic", vec![]),
            n("A", Some("E"), "ready", "feature", vec!["B"]),
            n("B", Some("E"), "ready", "feature", vec!["A"]),
        ];
        let (wave, _) = compute_waves("E", &entries);
        assert_eq!(wave["A"], 0);
        assert_eq!(wave["B"], 0);
    }

    #[test]
    fn waves_acyclic_dependent_of_cycle_restratifies() {
        let entries = vec![
            n("E", None, "ready", "epic", vec![]),
            n("A", Some("E"), "ready", "feature", vec!["B"]),
            n("B", Some("E"), "ready", "feature", vec!["A"]),
            n("C", Some("E"), "ready", "feature", vec!["A"]),
        ];
        let (wave, max_wave) = compute_waves("E", &entries);
        assert_eq!(
            wave,
            BTreeMap::from([
                ("A".to_string(), 0),
                ("B".to_string(), 0),
                ("C".to_string(), 1),
            ])
        );
        assert_eq!(max_wave + 1, 2);
    }
}
