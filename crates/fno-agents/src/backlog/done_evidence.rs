//! The close-evidence rule. A write that sets `completed_at` on an
//! existing open row must leave that row a finish-line record: a PR ref,
//! a completion note, an artifact link or a retired stamp. The check runs
//! at both publication seams beside the cap invariants, so a bare close
//! on a node that shipped nothing reads as the refusal it is instead of a
//! done line on the board. Keyed on `completed_at`, never on the derived
//! status: the recompute rolls a container to `status: done` with no
//! `completed_at` when its children finish, and that roll is not a close.

use serde_json::Value;

/// Does this row carry a PR ref a human could open? Moved from the backlog
/// drain, where it answered "is this node shipped"; here it answers "is
/// this close evidenced". A `pr_number` counts even without a URL: the CLI
/// derives the URL from it on read. An empty `pr_url` is not evidence of a
/// ship.
pub(crate) fn has_pr_ref(row: &Value) -> bool {
    if row.get("pr_number").and_then(|n| n.as_u64()).is_some() {
        return true;
    }
    if row
        .get("pr_url")
        .and_then(|u| u.as_str())
        .is_some_and(|u| !u.trim().is_empty())
    {
        return true;
    }
    row.get("additional_prs")
        .and_then(|a| a.as_array())
        .is_some_and(|a| !a.is_empty())
}

fn nonempty_text(row: &Value, field: &str) -> bool {
    row.get(field)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn has_evidence(row: &Value) -> bool {
    if has_pr_ref(row) {
        return true;
    }
    ["completion_note", "artifact_url", "retired"]
        .iter()
        .any(|field| nonempty_text(row, field))
}

fn completed_at_set(row: &Value) -> bool {
    row.get("completed_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Refuse the first row this write closes done while leaving it with no
/// record of why. `pre` is the begin snapshot, `post` the candidate state.
/// A row absent from `pre` was born done (an import or an archive
/// restore), and a row already carrying `completed_at` was closed by an
/// earlier write: neither is a close this write performed, so neither is
/// judged. Already-done rows ride through every later write; only the
/// write that would create the next evidence-less close refuses.
pub fn enforce(pre: &[Value], post: &[Value]) -> Result<(), String> {
    // Borrowed id index over the pre-image, not a per-row find(): every
    // graph write reaches this seam, so a per-row scan is quadratic over
    // the whole graph.
    let pre_by_id = crate::graph_store::index_by_id(pre);
    for row in post {
        let (Some(id), true) = (crate::graph_store::entry_id(row), row.is_object()) else {
            continue;
        };
        if !completed_at_set(row) || has_evidence(row) {
            continue;
        }
        let Some(before) = pre_by_id.get(id).copied() else {
            continue;
        };
        if completed_at_set(before) {
            continue;
        }
        return Err(format!(
            "refused: {id} would close done with no record of why. It carries no \
             pr_number, pr_url or additional_prs, no completion_note and no \
             artifact_url. Nothing was written. Close it with one of: \
             fno backlog done {id} --pr-number <n>, --note \"<why it is done>\", \
             or --link <artifact url>. For a forced close, write the note first: \
             fno backlog update {id} --completion-note \"<why>\"."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_row(id: &str) -> Value {
        json!({"id": id, "slug": id, "title": "open", "type": "feature",
               "status": "in_progress", "priority": "p2", "domain": "code"})
    }

    fn closed_row(id: &str) -> Value {
        json!({"id": id, "slug": id, "title": "open", "type": "feature",
               "status": "done", "priority": "p2", "domain": "code",
               "completed_at": "2026-09-23T00:00:00+00:00"})
    }

    #[test]
    fn a_bare_evidence_less_close_is_refused_and_names_the_repairs() {
        let pre = vec![open_row("ab-evd0001")];
        let post = vec![closed_row("ab-evd0001")];
        let error = enforce(&pre, &post).unwrap_err();
        assert!(error.contains("refused: ab-evd0001"), "{error}");
        for flag in ["--pr-number", "--note", "--link", "--completion-note"] {
            assert!(error.contains(flag), "{flag} must appear: {error}");
        }
    }

    #[test]
    fn every_evidence_kind_passes_the_same_close() {
        let kinds: Vec<Value> = vec![
            json!({"pr_number": 42}),
            json!({"pr_url": "https://example.test/pull/42"}),
            json!({"additional_prs": [{"number": 7}]}),
            json!({"completion_note": "shipped as a docs change"}),
            json!({"artifact_url": "https://example.test/artifact"}),
            json!({"retired": "stale-postmortem-receipt"}),
        ];
        for kind in &kinds {
            let mut post = closed_row("ab-evd0002");
            post.as_object_mut()
                .unwrap()
                .extend(kind.as_object().unwrap().clone());
            enforce(&[open_row("ab-evd0002")], &[post]).expect("evidence must pass");
        }
    }

    #[test]
    fn an_empty_string_is_not_evidence() {
        let mut post = closed_row("ab-evd0003");
        post.as_object_mut()
            .unwrap()
            .insert("completion_note".to_string(), json!("   "));
        enforce(&[open_row("ab-evd0003")], &[post]).unwrap_err();
    }

    #[test]
    fn a_row_born_done_is_not_judged() {
        // An import or an archive restore re-inserts a done row: this write
        // did not close anything, so the rule does not fire.
        enforce(&[], &[closed_row("ab-evd0004")]).unwrap();
    }

    #[test]
    fn an_already_done_row_passes_unchanged() {
        // A row closed by an earlier write rides through every later write.
        let pre = vec![closed_row("ab-evd0005")];
        enforce(&pre, &[closed_row("ab-evd0005")]).unwrap();
    }

    #[test]
    fn a_container_rolled_done_without_completed_at_is_not_judged() {
        // AC1-EDGE: the recompute rolls a container to done with no
        // completed_at when its children finish. That roll is not a close.
        let pre = vec![open_row("ab-evd0006")];
        let mut post = open_row("ab-evd0006");
        post.as_object_mut()
            .unwrap()
            .insert("status".to_string(), json!("done"));
        enforce(&pre, &[post]).unwrap();
    }

    #[test]
    fn a_write_that_never_touches_the_close_passes() {
        let rows = vec![open_row("ab-evd0007")];
        enforce(&rows, &rows).unwrap();
    }

    #[test]
    fn the_drain_and_the_store_share_one_predicate() {
        assert!(has_pr_ref(&json!({"pr_number": 3})));
        assert!(!has_pr_ref(&open_row("ab-evd0008")));
        assert!(!has_pr_ref(&json!({"pr_url": "   "})));
    }
}
