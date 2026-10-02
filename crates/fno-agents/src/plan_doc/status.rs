//! Plan-frontmatter status projection, ported 1:1 from `cli/src/fno/plan/_status.py`.
//!
//! The monotonic plan axis is design < ready < in_progress < in_review < done;
//! `superseded` is an off-axis terminal. The retired spellings resolve on read
//! via the shared alias table in graph_store.

/// Retired plan spellings, accepted on read and never written. Reuses the
/// store's table (the Python reader `plan._status.STATUS_ALIASES` is the same
/// three rows) instead of retyping it.
const STATUS_ALIASES: &[(&str, &str)] = crate::graph_store::PLAN_STATUS_ALIASES;

/// The full plan-status vocabulary the reconcile sweep leaves untouched: the
/// canonical axis + terminals (the store's STATUS_TO_RUNG words) plus every
/// retired spelling. A retired spelling is valid input, not drift. Built from
/// the tables that already exist; never a third retyped copy.
pub(crate) fn known_statuses() -> &'static [&'static str] {
    static KNOWN: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    KNOWN.get_or_init(|| {
        crate::graph_store::STATUS_TO_RUNG
            .iter()
            .map(|(s, _)| *s)
            .chain(
                crate::graph_store::PLAN_STATUS_ALIASES
                    .iter()
                    .map(|(s, _)| *s),
            )
            .collect()
    })
}

/// Graph derived `_status` -> plan `status`. None means "no plan write".
const GRAPH_TO_PLAN_STATUS: &[(&str, Option<&str>)] = &[
    ("idea", Some("idea")),
    ("design", Some("design")),
    ("ready", Some("ready")),
    ("in_progress", None),
    ("claimed", None),
    ("blocked", None),
    ("in_review", Some("in_review")),
    ("done", Some("done")),
    ("superseded", Some("superseded")),
    ("deferred", None),
];

/// Forward-only ordering, keyed by the plan vocabulary. `idea` sits at -1, the
/// same rank an unknown/absent status gets, so it is never a projection target.
fn projection_rank(status: &str) -> i32 {
    match status {
        "design" => 0,
        "ready" => 1,
        "in_progress" => 2,
        "in_review" => 3,
        "done" => 4,
        _ => -1,
    }
}

/// Normalized status: bare lowercase token, quotes stripped.
fn norm_status(raw: Option<&str>) -> String {
    raw.unwrap_or("")
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .to_lowercase()
}

/// Normalized status with any retired spelling resolved to its survivor.
/// Read-path translation only: callers compare and rank against the result,
/// they never write it back over the doc that supplied it.
pub fn canonical_status(raw: Option<&str>) -> String {
    let s = norm_status(raw);
    STATUS_ALIASES
        .iter()
        .find(|(from, _)| *from == s)
        .map(|(_, to)| (*to).to_string())
        .unwrap_or(s)
}

/// Plan status to WRITE for a node in `graph_status`, or None to leave it.
/// Forward-only; returns None when the graph status maps to no write, the
/// target equals the current status, or the target would be a backward move.
/// `superseded` is written over any non-terminal plan state but never over
/// `done` or `superseded`.
pub fn project_plan_status(current: Option<&str>, graph_status: &str) -> Option<String> {
    let target = GRAPH_TO_PLAN_STATUS
        .iter()
        .find(|(from, _)| *from == graph_status)
        .and_then(|(_, to)| *to)?;
    let cur = canonical_status(current);
    if target == cur {
        return None;
    }
    if target == "superseded" {
        return if cur == "done" || cur == "superseded" {
            None
        } else {
            Some("superseded".to_string())
        };
    }
    if cur == "done" || cur == "superseded" {
        return None; // terminal: never auto-rewritten forward off a terminal
    }
    if projection_rank(&target) <= projection_rank(&cur) {
        return None;
    }
    Some(target.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_on_read() {
        assert_eq!(canonical_status(Some("shipped")), "in_review");
        assert_eq!(canonical_status(Some("archived")), "superseded");
        assert_eq!(canonical_status(Some("stub")), "idea");
        assert_eq!(canonical_status(Some("Ready")), "ready");
        assert_eq!(canonical_status(Some("'done'")), "done");
        assert_eq!(canonical_status(None), "");
        assert_eq!(canonical_status(Some("weird")), "weird");
    }

    #[test]
    fn forward_only_projection() {
        assert_eq!(
            project_plan_status(Some("ready"), "in_review"),
            Some("in_review".into())
        );
        assert_eq!(
            project_plan_status(Some("shipped"), "done"),
            Some("done".into())
        );
        // Backward moves refused.
        assert_eq!(project_plan_status(Some("in_review"), "claimed"), None);
        assert_eq!(project_plan_status(Some("done"), "ready"), None);
        // Gated states write nothing.
        assert_eq!(project_plan_status(Some("ready"), "blocked"), None);
        assert_eq!(project_plan_status(Some("ready"), "deferred"), None);
        assert_eq!(project_plan_status(Some("ready"), "in_progress"), None);
        // Identity is a no-op.
        assert_eq!(project_plan_status(Some("ready"), "ready"), None);
        // Terminals never rewritten forward.
        assert_eq!(project_plan_status(Some("superseded"), "done"), None);
    }

    #[test]
    fn superseded_written_over_non_terminal_only() {
        assert_eq!(
            project_plan_status(Some("ready"), "superseded"),
            Some("superseded".into())
        );
        assert_eq!(project_plan_status(Some("done"), "superseded"), None);
        assert_eq!(project_plan_status(Some("superseded"), "superseded"), None);
    }

    #[test]
    fn idea_is_never_a_target() {
        // rank(idea) = -1: never above any current rung, and unknown == idea rank.
        assert_eq!(project_plan_status(Some("design"), "idea"), None);
        assert_eq!(project_plan_status(None, "idea"), None);
    }

    #[test]
    fn idea_advances_forward_to_every_later_rung() {
        for rung in ["design", "ready", "in_review", "done"] {
            assert_eq!(
                project_plan_status(Some("idea"), rung),
                Some(rung.to_string())
            );
        }
    }

    #[test]
    fn backward_refused_from_a_retired_spelling() {
        // `shipped` canonicalizes to in_review: a claimed node is a backward
        // move, and the doc keeps its spelling (read-path translation only).
        assert_eq!(project_plan_status(Some("shipped"), "claimed"), None);
    }

    #[test]
    fn none_gates_in_progress_and_claimed() {
        // The two rows that are not renames: a graph-side pause or claim is
        // reversible, so plan state stands.
        assert_eq!(project_plan_status(Some("design"), "in_progress"), None);
        assert_eq!(project_plan_status(Some("design"), "claimed"), None);
    }
}
