//! How `fno-agents reap` prints what it collected.
//!
//! Split out of `client.rs`, which is over the shrink-only file budget: this
//! is a pure renderer over a `GcSummary`, it has no daemon or argv knowledge,
//! and its tests are the bulk of what it costs. Keeping it here lets the
//! dispatcher stay a dispatcher.

use crate::daemon::GcSummary;
use serde_json::{json, Value};

/// Render a sweep outcome. Pure, like [`render_restart`], so the one property
/// that matters here is testable without a registry: BOTH counts appear at every
/// pass, including zero.
///
/// A backstop removal bypassed the corroboration gate. Reporting it separately is
/// the entire reason it is a separate verdict, so an operator can see the bypass
/// happening and compare the two totals. A field nothing prints is not a count -
/// it reported zero reaps while rows were being deleted.
///
/// `kept_uncorroborated` (x-9de7 task 5) names the row that is stuck and
/// invisible without it: past grace, nothing positively confirms it dead yet,
/// short of the 7-day backstop. Reported at every pass so "the count of rows
/// kept with no named gate is zero" is a claim the output itself can be
/// checked against, not one that has to be taken on faith.
pub fn render_reap(summary: &GcSummary, json_out: bool, dry_run: bool) -> String {
    if json_out {
        let kept: Vec<Value> = summary
            .kept_dirty
            .iter()
            .map(|(id, path)| json!({"id": id, "worktree": path}))
            .collect();
        let refused: Vec<Value> = summary
            .cascade_refused
            .iter()
            .map(|(id, reason)| json!({"id": id, "reason": reason}))
            .collect();
        let node_refused: Vec<Value> = summary
            .node_session_refused
            .iter()
            .map(|(id, reason)| json!({"id": id, "reason": reason}))
            .collect();
        let no_receipt: Vec<Value> = summary
            .kept_no_receipt
            .iter()
            .map(|(id, reason)| json!({"id": id, "reason": reason}))
            .collect();
        let kept_receipts: Vec<Value> = summary
            .kept_receipts
            .iter()
            .map(|(id, reason)| json!({"id": id, "reason": reason}))
            .collect();
        let not_terminal: Vec<Value> = summary
            .kept_not_terminal
            .iter()
            .map(|(id, tail)| json!({"id": id, "reason": tail}))
            .collect();
        return format!(
            "{}\n",
            json!({
                "reaped": summary.reaped,
                "reaped_backstop": summary.reaped_backstop,
                "reaped_dormant": summary.reaped_dormant,
                "cascade_refused": refused,
                "node_session_refused": node_refused,
                "kept_dirty": kept,
                "kept_uncorroborated": summary.kept_uncorroborated,
                "kept_no_receipt": no_receipt,
                "kept_live": summary.kept_live,
                "kept_not_terminal": not_terminal,
                "kept_contradicted": summary.kept_contradicted,
                "cleared_contradiction": summary.cleared_contradiction,
                "expired_receipts": summary.expired_receipts,
                "kept_receipts": kept_receipts,
                "dormant_probes_escalated": summary.dormant_probes_escalated,
                "dry_run": dry_run,
            })
        );
    }
    let verb = if dry_run { "would reap" } else { "reaped" };
    let mut out = format!(
        "{verb} {} row(s) ({} by the age backstop, {} dormant done); \
         {} live-idle row(s) escalated to a truth probe\n",
        summary.reaped.len(),
        summary.reaped_backstop.len(),
        summary.reaped_dormant.len(),
        // Reported at every pass, including zero. The cap this replaced
        // truncated a large sweep silently; a spend nobody prints is the same
        // silence one step over.
        summary.dormant_probes_escalated,
    );
    for id in &summary.reaped {
        out.push_str(&format!("  {verb} {id}\n"));
    }
    for id in &summary.reaped_backstop {
        out.push_str(&format!(
            "  {verb} {id} (age backstop: nothing corroborated it)\n"
        ));
    }
    for id in &summary.reaped_dormant {
        out.push_str(&format!(
            "  {verb} {id} (dormant: transcript tail read done; resumable handle in the event)\n"
        ));
    }
    for (id, reason) in &summary.cascade_refused {
        out.push_str(&format!(
            "  harness session kept {id} (cascade refused: {reason})\n"
        ));
    }
    for (id, reason) in &summary.node_session_refused {
        out.push_str(&format!("  kept {id} (node session refused: {reason})\n"));
    }
    for (id, path) in &summary.kept_dirty {
        out.push_str(&format!("  kept {id} (dirty worktree: {path})\n"));
    }
    for id in &summary.kept_uncorroborated {
        out.push_str(&format!(
            "  kept {id} (uncorroborated: no confirmed-dead pid, no positively-stale transcript, no gone harness session yet)\n"
        ));
    }
    for (id, reason) in &summary.kept_no_receipt {
        out.push_str(&format!("  kept {id} (no resumable receipt: {reason})\n"));
    }
    for id in &summary.kept_live {
        out.push_str(&format!(
            "  kept {id} (live: liveness re-check reports it alive)\n"
        ));
    }
    for (id, tail) in &summary.kept_not_terminal {
        out.push_str(&format!(
            "  kept {id} (not terminal: no gate has ruled - still coming up or running, and no pid is confirmed dead; {tail})\n"
        ));
    }
    for id in &summary.kept_contradicted {
        out.push_str(&format!(
            "  kept {id} (contradicted: status reads live but an exited_at is on the row and the liveness ladder is silent)\n"
        ));
    }
    for id in &summary.cleared_contradiction {
        out.push_str(&format!(
            "  cleared stale exited_at on {id} (the liveness ladder proves the row alive)\n"
        ));
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
    //! `reap` outcome rendering: both counts, at every pass, including zero.
    //!
    //! These live here rather than in the `fno-agents` binary that calls
    //! `render_reap`: the binary is over the file budget, and a renderer's
    //! tests belong beside the renderer.
    use super::*;
    use crate::daemon::GcSummary;
    use serde_json::{json, Value};

    fn summary(reaped: &[&str], backstop: &[&str]) -> crate::daemon::GcSummary {
        crate::daemon::GcSummary {
            reaped: reaped.iter().map(|s| (*s).to_string()).collect(),
            reaped_backstop: backstop.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn reap_reports_the_backstop_count_even_when_it_is_zero() {
        // The always-on half of the criterion. An operator reading a quiet pass
        // must still see that the second count exists and is zero, or a later
        // nonzero one has nothing to be read against.
        let out = render_reap(&summary(&["a1"], &[]), false, false);
        assert!(
            out.starts_with("reaped 1 row(s) (0 by the age backstop, 0 dormant done)"),
            "missing the zero backstop count: {out}"
        );
    }

    #[test]
    fn reap_names_every_backstop_row_it_removed() {
        // The regression: the field existed and nothing printed it, so the verb
        // said "reaped 0 row(s)" while the backstop deleted two rows.
        let out = render_reap(&summary(&[], &["b1", "b2"]), false, false);
        assert!(
            out.starts_with("reaped 0 row(s) (2 by the age backstop, 0 dormant done)"),
            "backstop removals missing from the totals: {out}"
        );
        assert!(out.contains("  reaped b1 (age backstop"), "{out}");
        assert!(out.contains("  reaped b2 (age backstop"), "{out}");
    }

    #[test]
    fn reap_json_carries_both_lists() {
        let out = render_reap(&summary(&["a1"], &["b1"]), true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["reaped"], json!(["a1"]));
        assert_eq!(v["reaped_backstop"], json!(["b1"]));
    }

    #[test]
    fn reap_json_carries_node_session_refusals() {
        let s = crate::daemon::GcSummary {
            node_session_refused: vec![("node-1".into(), "read-back failed".into())],
            ..Default::default()
        };
        let out = render_reap(&s, true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(
            v["node_session_refused"],
            json!([{"id": "node-1", "reason": "read-back failed"}])
        );
        assert!(render_reap(&s, false, false).contains("node session refused"));
    }

    #[test]
    fn reap_json_keeps_the_backstop_key_when_empty() {
        // A key that vanishes at zero makes every consumer write a default, and
        // one of them will default to "no backstop removals ever happened".
        let out = render_reap(&summary(&[], &[]), true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["reaped_backstop"], json!([]));
    }

    // -- x-9de7 task 5: kept_uncorroborated + --dry-run ----------------------

    #[test]
    fn reap_names_the_uncorroborated_gate_in_text_and_json() {
        let s = crate::daemon::GcSummary {
            kept_uncorroborated: vec!["stuck1".to_string()],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("  kept stuck1 (uncorroborated"),
            "no named gate for the stuck row: {text}"
        );
        let out = render_reap(&s, true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["kept_uncorroborated"], json!(["stuck1"]));
    }

    // -- x-98ab: the Live keep is reported like any other --------------------

    #[test]
    fn reap_names_the_live_gate_in_text_and_json() {
        // A zero-reap pass over a fully-live fleet must never read as silence:
        // all 26 rows kept, every one named with the gate that kept it.
        let s = crate::daemon::GcSummary {
            kept_live: vec!["live1".to_string(), "live2".to_string()],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("  kept live1 (live: liveness re-check reports it alive)"),
            "no named gate for the live row: {text}"
        );
        assert!(text.contains("  kept live2 (live:"));
        let out = render_reap(&s, true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["kept_live"], json!(["live1", "live2"]));
    }

    #[test]
    fn reap_names_the_not_terminal_gate_in_text_and_json() {
        // x-91f3: the NotTerminal keep is the blanket that held the measured
        // registry - the majority verdict on a fleet of pid-less rows - and
        // it went unnamed, so the verb said "would reap 0" while keeping 26.
        let s = crate::daemon::GcSummary {
            kept_not_terminal: vec![
                ("nt1".to_string(), "tail: stalled".to_string()),
                ("nt2".to_string(), "no tail read".to_string()),
            ],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("  kept nt1 (not terminal:"),
            "no named gate for the not-terminal row: {text}"
        );
        assert!(text.contains("  kept nt2 (not terminal:"));
        // A row held because it died mid-turn and one held because nothing
        // could read its transcript are different problems with different
        // fixes, and the gate name alone spells them the same.
        assert!(text.contains("tail: stalled"), "{text}");
        assert!(text.contains("no tail read"), "{text}");
        let out = render_reap(&s, true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(
            v["kept_not_terminal"],
            json!([
                {"id": "nt1", "reason": "tail: stalled"},
                {"id": "nt2", "reason": "no tail read"},
            ])
        );
    }

    #[test]
    fn reap_dry_run_says_would_reap_not_reaped() {
        // `--dry-run` must never claim past tense on a row nothing removed.
        let out = render_reap(&summary(&["a1"], &["b1"]), false, true);
        assert!(out.starts_with("would reap 1 row(s) (1 by the age backstop, 0 dormant done)"));
        assert!(out.contains("  would reap a1"));
        assert!(out.contains("  would reap b1 (age backstop"));
        assert!(
            !out.contains("reaped a1"),
            "must not also say reaped: {out}"
        );
        assert!(out.contains("(dry-run: no changes made)"));
    }

    #[test]
    fn reap_dry_run_json_names_the_mode() {
        let out = render_reap(&summary(&["a1"], &[]), true, true);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["dry_run"], json!(true));
        assert_eq!(v["reaped"], json!(["a1"]));
    }

    #[test]
    fn reap_live_run_json_names_the_mode_false() {
        let out = render_reap(&summary(&["a1"], &[]), true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(v["dry_run"], json!(false));
    }

}
