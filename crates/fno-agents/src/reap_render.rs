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
        return format!(
            "{}\n",
            json!({
                "retired": retired,
                "pruned": pruned,
                "kept_operator": summary.kept_operator,
                "kept_crowned": summary.kept_crowned,
                "kept_not_spawn": pair(&summary.kept_not_spawn),
                "kept_no_provenance": summary.kept_no_provenance,
                "kept_open_work": open_work,
                "kept_open_do_row": open_do,
                "kept_active": active,
                "kept_transcript_unresolved": summary.kept_transcript_unresolved,
                "kept_graph_unreadable": summary.kept_graph_unreadable,
                "kept_dirty": pathed(&summary.kept_dirty),
                "kept_unmerged": pathed(&summary.kept_unmerged),
                "kept_unprobed": pathed(&summary.kept_unprobed),
                "stop_refused": pair(&summary.stop_refused),
                "kept_no_receipt": pair(&summary.kept_no_receipt),
                "expired_receipts": summary.expired_receipts,
                "kept_receipts": pair(&summary.kept_receipts),
                "dry_run": dry_run,
            })
        );
    }
    let verb = if dry_run { "would retire" } else { "retired" };
    let mut out = format!(
        "{verb} {} row(s); pruned {} worktree(s)\n",
        summary.retired.len(),
        summary.pruned.len(),
    );
    for (id, basis) in &summary.retired {
        out.push_str(&format!("  {verb} {id} ({basis})\n"));
    }
    for (id, path) in &summary.pruned {
        out.push_str(&format!("  pruned {id} (clean and merged: {path})\n"));
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
        out.push_str(&format!("  kept {id} (no provenance: named in no node)\n"));
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
            "kept_operator",
            "kept_crowned",
            "kept_not_spawn",
            "kept_no_provenance",
            "kept_open_work",
            "kept_open_do_row",
            "kept_active",
            "kept_transcript_unresolved",
            "kept_graph_unreadable",
            "kept_dirty",
            "kept_unmerged",
            "kept_unprobed",
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
