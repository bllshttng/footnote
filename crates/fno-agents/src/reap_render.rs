//! How `fno-agents reap` prints what it collected.
//!
//! Split out of `client.rs`, which is over the shrink-only file budget: this
//! is a pure renderer over a `GcSummary`, it has no daemon or argv knowledge,
//! and its tests are the bulk of what it costs. Keeping it here lets the
//! dispatcher stay a dispatcher.

use crate::gc_sweep::GcSummary;
use serde_json::{json, Value};

/// Render a sweep outcome. Pure, so the one property that matters here is
/// testable without a registry: every bucket appears at every pass, zero
/// counts included. A row the pass judged lands in exactly one bucket, and a
/// bucket nothing prints is not a count - the verb would report zero
/// retirements while rows were being removed.
pub fn render_reap(summary: &GcSummary, json_out: bool, dry_run: bool) -> String {
    if json_out {
        let retired: Vec<Value> = summary
            .retired
            .iter()
            .map(|(id, basis)| json!({"id": id, "basis": basis}))
            .collect();
        let pruned: Vec<Value> = summary
            .pruned
            .iter()
            .map(|(id, path)| json!({"id": id, "worktree": path}))
            .collect();
        let prune_failed: Vec<Value> = summary
            .prune_failed
            .iter()
            .map(|(id, reason)| json!({"id": id, "reason": reason}))
            .collect();
        let open_work: Vec<Value> = summary
            .kept_open_work
            .iter()
            .map(|(id, node, status)| json!({"id": id, "node": node, "status": status}))
            .collect();
        let active: Vec<Value> = summary
            .kept_active
            .iter()
            .map(|(id, age_s)| json!({"id": id, "age_s": age_s}))
            .collect();
        let pair = |rows: &Vec<(String, String)>| -> Vec<Value> {
            rows.iter()
                .map(|(id, reason)| json!({"id": id, "reason": reason}))
                .collect()
        };
        let pathed = |rows: &Vec<(String, String)>| -> Vec<Value> {
            rows.iter()
                .map(|(id, path)| json!({"id": id, "worktree": path}))
                .collect()
        };
        let open_do: Vec<Value> = summary
            .kept_open_do_row
            .iter()
            .map(|(id, node)| json!({"id": id, "node": node}))
            .collect();
        let triples = |rows: &Vec<(String, String, String)>| -> Vec<Value> {
            rows.iter()
                .map(|(id, a, b)| json!({"id": id, "detail_a": a, "detail_b": b}))
                .collect()
        };
        let settled: Vec<Value> = summary
            .settled_do_rows
            .iter()
            .map(|(node, harness, session_id)| {
                json!({"node": node, "harness": harness, "session_id": session_id})
            })
            .collect();
        return format!(
            "{}\n",
            json!({
                "retired": retired,
                "pruned": pruned,
                "prune_failed": prune_failed,
                "settled_do_rows": settled,
                "settle_refused": pair(&summary.settle_refused),
                "kept_operator": summary.kept_operator,
                "kept_crowned": summary.kept_crowned,
                "kept_not_spawn": pair(&summary.kept_not_spawn),
                "kept_no_provenance": summary.kept_no_provenance,
                "kept_node_conflict": triples(&summary.kept_node_conflict),
                "kept_pr_contradicts": triples(&summary.kept_pr_contradicts),
                "kept_open_work": open_work,
                "kept_open_do_row": open_do,
                "kept_active": active,
                "kept_transcript_unresolved": summary.kept_transcript_unresolved,
                "kept_graph_unreadable": summary.kept_graph_unreadable,
                "kept_dirty": pathed(&summary.kept_dirty),
                "kept_unmerged": pathed(&summary.kept_unmerged),
                "kept_unprobed": pathed(&summary.kept_unprobed),
                "kept_shared_tree": pair(&summary.kept_shared_tree),
                "kept_live_descendants": pair(&summary.kept_live_descendants),
                "stop_refused": pair(&summary.stop_refused),
                "kept_no_receipt": pair(&summary.kept_no_receipt),
                "expired_receipts": summary.expired_receipts,
                "kept_receipts": pair(&summary.kept_receipts),
                "dry_run": dry_run,
            })
        );
    }
    let verb = if dry_run { "would retire" } else { "retired" };
    // x-f55c: a rehearsal never calls prune_tree, so `pruned` here is a
    // PROJECTION off TreeAction::Prune alone, never a confirmed removal -
    // the verb must say so, the same way `retired` already does.
    let prune_verb = if dry_run { "would prune" } else { "pruned" };
    let mut out = format!(
        "{verb} {} row(s); {prune_verb} {} worktree(s)\n",
        summary.retired.len(),
        summary.pruned.len(),
    );
    for (id, basis) in &summary.retired {
        out.push_str(&format!("  {verb} {id} ({basis})\n"));
    }
    for (id, path) in &summary.pruned {
        out.push_str(&format!("  {prune_verb} {id} (clean and merged: {path})\n"));
    }
    for (id, reason) in &summary.prune_failed {
        out.push_str(&format!("  prune failed {id} ({reason})\n"));
    }
    let settle_verb = if dry_run { "would settle" } else { "settled" };
    for (node, harness, session_id) in &summary.settled_do_rows {
        let _ = harness; // named in the JSON; the text line carries node + session
        out.push_str(&format!(
            "  {settle_verb} {session_id} (stale open do row filled on done+merged node: {node})\n"
        ));
    }
    for (node, reason) in &summary.settle_refused {
        out.push_str(&format!("  settle refused {node} ({reason})\n"));
    }
    for id in &summary.kept_operator {
        out.push_str(&format!("  kept {id} (operator row)\n"));
    }
    for id in &summary.kept_crowned {
        out.push_str(&format!("  kept {id} (crowned)\n"));
    }
    for (id, origin) in &summary.kept_not_spawn {
        let why = if origin.is_empty() {
            "no origin recorded".to_string()
        } else {
            format!("origin {origin}")
        };
        out.push_str(&format!("  kept {id} (not a spawn row: {why})\n"));
    }
    for id in &summary.kept_no_provenance {
        out.push_str(&format!(
            "  kept {id} ({}\\n",
            crate::gc::KeepReason::NoProvenance.as_str()
        ));
    }
    for (id, a, b) in &summary.kept_node_conflict {
        out.push_str(&format!("  kept {id} (sources disagree: {a} vs {b})\n"));
    }
    for (id, node, detail) in &summary.kept_pr_contradicts {
        out.push_str(&format!(
            "  kept {id} (pr state contradicts: {node} {detail})\n"
        ));
    }
    for (id, node, status) in &summary.kept_open_work {
        out.push_str(&format!("  kept {id} (open work: {node} {status})\n"));
    }
    for (id, node) in &summary.kept_open_do_row {
        out.push_str(&format!("  kept {id} (open do row on done node: {node})\n"));
    }
    for (id, age_s) in &summary.kept_active {
        out.push_str(&format!(
            "  kept {id} (active: transcript written {age_s}s ago)\n"
        ));
    }
    for id in &summary.kept_transcript_unresolved {
        out.push_str(&format!(
            "  kept {id} (transcript unresolved: absence is not quiet)\n"
        ));
    }
    for id in &summary.kept_graph_unreadable {
        out.push_str(&format!(
            "  kept {id} (graph unreadable: never a retirement on a failed read)\n"
        ));
    }
    for (id, path) in &summary.kept_dirty {
        out.push_str(&format!("  kept tree {id} (dirty: {path})\n"));
    }
    for (id, path) in &summary.kept_unmerged {
        out.push_str(&format!(
            "  kept tree {id} (clean but the branch never merged: {path})\n"
        ));
    }
    for (id, path) in &summary.kept_unprobed {
        out.push_str(&format!(
            "  kept tree {id} (the cleanliness probe could not answer: {path})\n"
        ));
    }
    for (id, holder) in &summary.kept_shared_tree {
        out.push_str(&format!(
            "  kept tree {id} (shared with {holder}, still live)\n"
        ));
    }
    for (id, child) in &summary.kept_live_descendants {
        out.push_str(&format!("  kept {id} (live descendant: {child})\n"));
    }
    for (id, reason) in &summary.stop_refused {
        out.push_str(&format!("  kept {id} (stop refused: {reason})\n"));
    }
    for (id, reason) in &summary.kept_no_receipt {
        out.push_str(&format!("  kept {id} (no resumable receipt: {reason})\n"));
    }
    for name in &summary.expired_receipts {
        out.push_str(&format!("  expired receipt {name}\n"));
    }
    for (name, reason) in &summary.kept_receipts {
        out.push_str(&format!("  kept receipt {name} ({reason})\n"));
    }
    if dry_run {
        out.push_str("(dry-run: no changes made)\n");
    }
    out
}

/// Render a sweep outcome plus, for the dry-run JSON read, the census the
/// projection exists to expose (x-70e1 task 4): the complete per-session
/// identity, its observed surfaces, and the source coverage that says how
/// complete the enumeration is. `None` renders exactly like
/// [`render_reap`].
pub fn render_reap_with_inventory(
    summary: &GcSummary,
    inventory: Option<&crate::gc_inventory::Inventory>,
    json_out: bool,
    dry_run: bool,
) -> String {
    let base = render_reap(summary, json_out, dry_run);
    let Some(inv) = inventory else {
        return base;
    };
    if !json_out {
        return base;
    }
    // Splice the census into the summary object: one JSON read carries both
    // the would-retire verdicts and the world they were judged against.
    let mut value: Value = match serde_json::from_str(base.trim()) {
        Ok(v) => v,
        Err(_) => return base,
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "inventory".into(),
            serde_json::to_value(inv).unwrap_or(Value::Null),
        );
    }
    format!("{}\n", value)
}

#[cfg(test)]
mod tests {
    //! `reap` outcome rendering: every bucket, at every pass, including zero.
    use super::*;
    use serde_json::{json, Value};

    fn summary(retired: &[(&str, &str)]) -> GcSummary {
        GcSummary {
            retired: retired
                .iter()
                .map(|(id, basis)| ((*id).to_string(), (*basis).to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn reap_reports_every_bucket_even_when_all_are_zero() {
        // A key that vanishes at zero makes every consumer write a default,
        // and one of them will default to "no retirements ever happened".
        let out = render_reap(&summary(&[]), true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        for key in [
            "retired",
            "pruned",
            "prune_failed",
            "settled_do_rows",
            "settle_refused",
            "kept_operator",
            "kept_crowned",
            "kept_not_spawn",
            "kept_no_provenance",
            "kept_node_conflict",
            "kept_pr_contradicts",
            "kept_open_work",
            "kept_open_do_row",
            "kept_active",
            "kept_transcript_unresolved",
            "kept_graph_unreadable",
            "kept_dirty",
            "kept_unmerged",
            "kept_unprobed",
            "kept_shared_tree",
            "kept_live_descendants",
            "stop_refused",
            "kept_no_receipt",
            "expired_receipts",
            "kept_receipts",
        ] {
            assert!(
                v.get(key).is_some(),
                "bucket {key} missing from json: {out}"
            );
        }
    }

    #[test]
    fn reap_names_every_retired_row_with_its_basis() {
        let out = render_reap(
            &summary(&[("a1", "every named node done: N1")]),
            false,
            false,
        );
        assert!(
            out.starts_with("retired 1 row(s); pruned 0 worktree(s)"),
            "{out}"
        );
        assert!(
            out.contains("  retired a1 (every named node done: N1)"),
            "{out}"
        );
    }

    #[test]
    fn reap_dry_run_says_would_retire_not_retired() {
        // `--dry-run` must never claim past tense on a row nothing removed.
        let out = render_reap(
            &summary(&[("a1", "every named node done: N1")]),
            false,
            true,
        );
        assert!(out.starts_with("would retire 1 row(s)"));
        assert!(out.contains("  would retire a1"));
        assert!(
            !out.contains("retired a1"),
            "must not also say retired: {out}"
        );
        assert!(out.contains("(dry-run: no changes made)"));
    }

    #[test]
    fn reap_dry_run_says_would_prune_not_pruned() {
        // x-f55c: a rehearsal never confirms a removal - the `pruned` line
        // must carry the same "would" verb the `retired` line already does.
        let s = GcSummary {
            pruned: vec![("a1".into(), "/tmp/wt".into())],
            ..Default::default()
        };
        let dry = render_reap(&s, false, true);
        assert!(dry.contains("would prune a1"), "{dry}");
        assert!(!dry.contains("  pruned a1"), "must not say pruned: {dry}");
        let live = render_reap(&s, false, false);
        assert!(live.contains("  pruned a1"), "{live}");
        assert!(!live.contains("would prune"), "{live}");
    }

    #[test]
    fn reap_dry_run_json_names_the_mode() {
        let out = render_reap(&summary(&[("a1", "x")]), true, true);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["dry_run"], json!(true));
        assert_eq!(v["retired"], json!([{"id": "a1", "basis": "x"}]));
    }

    #[test]
    fn reap_live_run_json_names_the_mode_false() {
        let out = render_reap(&summary(&[]), true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["dry_run"], json!(false));
    }

    #[test]
    fn reap_names_open_work_with_its_node_and_status() {
        let s = GcSummary {
            kept_open_work: vec![("b1".into(), "N3".into(), "in_review".into())],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("  kept b1 (open work: N3 in_review)"),
            "{text}"
        );
        let out = render_reap(&s, true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(
            v["kept_open_work"],
            json!([{"id": "b1", "node": "N3", "status": "in_review"}])
        );
    }

    #[test]
    fn reap_names_active_with_the_transcript_age() {
        let s = GcSummary {
            kept_active: vec![("c1".into(), 10)],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("  kept c1 (active: transcript written 10s ago)"),
            "{text}"
        );
    }

    #[test]
    fn reap_no_bucket_reads_an_exit_vocabulary_word() {
        // The retired vocabulary (x-c672): no bucket, reason string, or
        // receipt field reads exited_at, not-terminal, contradicted,
        // within-grace, uncorroborated, or backstop.
        let s = GcSummary {
            retired: vec![("a1".into(), "every named node done: N1".into())],
            kept_open_work: vec![("b1".into(), "N3".into(), "in_review".into())],
            kept_active: vec![("c1".into(), 10)],
            kept_no_provenance: vec!["d1".into()],
            ..Default::default()
        };
        for dry_run in [false, true] {
            let text = render_reap(&s, false, dry_run);
            for word in [
                "exited_at",
                "not-terminal",
                "contradicted",
                "within-grace",
                "uncorroborated",
                "backstop",
                "dormant",
            ] {
                assert!(!text.contains(word), "{word} leaked into: {text}");
            }
        }
    }
}
