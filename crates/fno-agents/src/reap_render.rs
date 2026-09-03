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
                "kept_not_terminal": summary.kept_not_terminal,
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
    for id in &summary.kept_not_terminal {
        out.push_str(&format!(
            "  kept {id} (not terminal: no gate has ruled - still coming up or running, and no pid is confirmed dead)\n"
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
