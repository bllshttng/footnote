//! How `fno-agents reap` prints what it collected.
//!
//! Split out of `client.rs`, which is over the shrink-only file budget: this
//! is a pure renderer over a `GcSummary`, it has no daemon or argv knowledge,
//! and its tests are the bulk of what it costs. Keeping it here lets the
//! dispatcher stay a dispatcher.

use crate::gc_sweep::{GcSummary, StateFilesReapSummary, StateReapFamilySummary, UnresolvedHold};
use serde_json::{json, Value};

/// Render the file-only reap receipt independently from row retirement.
pub fn render_state_files_reap(summary: &StateFilesReapSummary, json_out: bool) -> String {
    if json_out {
        return format!(
            "{}\n",
            json!({
                "families": {
                    "expired_claims": summary.expired_claims,
                    "plan_locks": summary.plan_locks,
                    "agent_locks": summary.agent_locks,
                    "pr_status_cache": summary.pr_status_cache,
                    "claim_tmp": summary.claim_tmp,
                },
                "totals": summary.totals,
                "applied": summary.applied,
                "dry_run": summary.dry_run,
                "skip_reason": summary.skip_reason,
            })
        );
    }

    fn family_line(name: &str, family: &StateReapFamilySummary) -> String {
        let mut reason_counts = std::collections::BTreeMap::new();
        for entry in &family.kept {
            *reason_counts.entry(entry.reason.as_str()).or_insert(0usize) += 1;
        }
        let kept = reason_counts
            .into_iter()
            .map(|(reason, count)| format!("{reason}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let oldest = family
            .oldest_age_s
            .map(|age| age.to_string())
            .unwrap_or_else(|| "none".to_string());
        format!(
            "{name}: scanned {}; deleted {}; would_delete {}; bytes {}; oldest_age_s {oldest}; kept [{}]\n",
            family.scanned, family.deleted, family.would_delete, family.bytes, kept
        )
    }

    let mut out = String::new();
    for (name, family) in [
        ("expired_claims", &summary.expired_claims),
        ("plan_locks", &summary.plan_locks),
        ("agent_locks", &summary.agent_locks),
        ("pr_status_cache", &summary.pr_status_cache),
        ("claim_tmp", &summary.claim_tmp),
    ] {
        out.push_str(&family_line(name, family));
    }
    let oldest = summary
        .totals
        .oldest_age_s
        .map(|age| age.to_string())
        .unwrap_or_else(|| "none".to_string());
    out.push_str(&format!(
        "total: scanned {}; deleted {}; would_delete {}; bytes {}; oldest_age_s {oldest}; kept {}; skip_reason {}\n",
        summary.totals.scanned,
        summary.totals.deleted,
        summary.totals.would_delete,
        summary.totals.bytes,
        summary.totals.kept,
        summary.skip_reason.as_deref().unwrap_or("none")
    ));
    if summary.dry_run {
        out.push_str("(dry-run: no changes made)\n");
    }
    out
}

/// Human clock for a hold line: `2h00m`, `3m48s`, `41s`. The shape the
/// escalation suffix and the release refusals both print (x-e3cc).
pub(crate) fn human_duration(secs: i64) -> String {
    let s = secs.max(0) as u64;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Render a sweep outcome. Pure, so the one property that matters here is
/// testable without a registry: every bucket appears at every pass, zero
/// counts included. A row the pass judged lands in exactly one bucket, and a
/// bucket nothing prints is not a count - the verb would report zero
/// retirements while rows were being removed.
pub fn render_reap(summary: &GcSummary, json_out: bool, dry_run: bool) -> String {
    // The hold clock (x-e3cc): the age suffix on a hold line. Reads the
    // summary's own `holds` projection, so a bucket line and its hold entry
    // can never disagree; a row with no hold entry renders bare, exactly as
    // before this change.
    let hold_line = |summary: &GcSummary, id: &str| -> String {
        let Some(h) = summary.holds.iter().find(|h| h.id == id) else {
            return String::new();
        };
        let Some(age) = h.age_s else {
            return " [held unmeasured]".to_string();
        };
        let mut s = format!(" [held {}, {}]", human_duration(age), h.age_basis);
        if h.escalated {
            s.push_str(&format!(
                "; past {}: fno agents reap --release {id}",
                human_duration(summary.hold_escalate_after_s.unwrap_or(0) as i64)
            ));
        }
        s
    };
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
            .map(|(id, node, status, reader)| {
                json!({"id": id, "node": node, "status": status, "reader": reader})
            })
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
        let planning_unclosed: Vec<Value> = summary
            .kept_planning_unclosed
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
        let holds: Vec<Value> = summary
            .holds
            .iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "reason": h.reason,
                    "detail": h.detail,
                    "age_s": h.age_s,
                    "age_basis": h.age_basis,
                    "escalated": h.escalated,
                })
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
                "kept_planning_unclosed": planning_unclosed,
                "kept_active": active,
                "kept_transcript_unresolved": summary.kept_transcript_unresolved,
                "kept_graph_unreadable": summary.kept_graph_unreadable,
                "kept_dirty": pathed(&summary.kept_dirty),
                "kept_unmerged": pathed(&summary.kept_unmerged),
                "kept_unprobed": pathed(&summary.kept_unprobed),
                "kept_shared_tree": pair(&summary.kept_shared_tree),
                "kept_live_descendants": pair(&summary.kept_live_descendants),
                "stop_refused": pair(&summary.stop_refused),
                "needs_live_stop": pair(&summary.needs_live_stop),
                "kept_no_receipt": pair(&summary.kept_no_receipt),
                "expired_receipts": summary.expired_receipts,
                "kept_receipts": pair(&summary.kept_receipts),
                "holds": holds,
                "hold_escalate_after_s": summary.hold_escalate_after_s,
                "release_refused": summary.release_refused,
                "dry_run": dry_run,
            })
        );
    }
    let verb = if dry_run { "would retire" } else { "retired" };
    // A rehearsal never calls prune_tree, so `pruned` here is a PROJECTION
    // off TreeAction::Prune alone, never a confirmed removal - the verb
    // must say so, the same way `retired` already does.
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
        // x-2774 change 4: this line used to emit an unbalanced paren and a
        // literal backslash-n; invisible only while the bucket measured
        // empty. One spelling with every other kept line.
        out.push_str(&format!(
            "  kept {id} ({})\n",
            crate::gc::KeepReason::NoProvenance.as_str()
        ));
    }
    for (id, a, b) in &summary.kept_node_conflict {
        out.push_str(&format!(
            "  kept {id} (sources disagree: {a} vs {b}){}\n",
            hold_line(summary, id)
        ));
    }
    for (id, node, detail) in &summary.kept_pr_contradicts {
        out.push_str(&format!(
            "  kept {id} (pr state contradicts: {node} {detail})\n"
        ));
    }
    for (id, node, status, reader) in &summary.kept_open_work {
        out.push_str(&format!(
            "  kept {id} (open work: {node} {status}; read via {reader})\n"
        ));
    }
    for (id, node) in &summary.kept_open_do_row {
        let detail = summary
            .holds
            .iter()
            .find(|h| h.id == *id)
            .map(|h| h.detail.as_str())
            .unwrap_or("");
        out.push_str(&format!(
            "  kept {id} (open do row on done node: {node}: {detail}){}\n",
            hold_line(summary, id)
        ));
    }
    for (id, node) in &summary.kept_planning_unclosed {
        out.push_str(&format!(
            "  kept {id} (planning assignment never closed by this session: {node})\n"
        ));
    }
    for (id, age_s) in &summary.kept_active {
        out.push_str(&format!(
            "  kept {id} (active: transcript written {age_s}s ago)\n"
        ));
    }
    for hold in &summary.kept_transcript_unresolved {
        let age = hold_age(hold.held_s);
        let decide = hold.nodes_done && hold.held_s > crate::gc_sweep::UNRESOLVED_HOLD_DECIDE_S;
        let suffix = if decide {
            format!("; needs a decision: fno agents rm {}", hold.id)
        } else {
            String::new()
        };
        // Main's x-1b90 line carries the clock and the 6h rm ask; the hold
        // projection renders no second age here, so one line reads one
        // clock. The escalated hold still asks for its release through the
        // [reap-hold] question lane.
        out.push_str(&format!(
            "  kept {} (transcript unresolved for {}: absence is not quiet{suffix})\n",
            hold.id, age
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
        out.push_str(&format!(
            "  kept {id} (stop refused: {reason}){}\n",
            hold_line(summary, id)
        ));
    }
    for (id, reason) in &summary.needs_live_stop {
        out.push_str(&format!(
            "  held {id} (needs live stop: {reason}){}\n",
            hold_line(summary, id)
        ));
    }
    for refused in &summary.release_refused {
        out.push_str(&format!("  {refused}\n"));
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
///
/// `mux` (x-91eb) is the second surface this verb sweeps: the mux tab
/// sideline through `fno mux workspace prune --tabs-only
/// --include-used-shells`. It renders in one of three distinguishable states
/// - ran, unread, skipped - in both the JSON object and the human receipt.
pub fn render_reap_with_inventory(
    summary: &GcSummary,
    inventory: Option<&crate::gc_inventory::Inventory>,
    mux: Option<&MuxSweep>,
    json_out: bool,
    dry_run: bool,
) -> String {
    let base = render_reap(summary, json_out, dry_run);
    let Some(mux) = mux else {
        return base;
    };
    if !json_out {
        return format!("{base}{}", mux_sweep_text_line(mux, dry_run));
    }
    // Splice the census and the mux half into the summary object: one JSON
    // read carries both the would-retire verdicts and the world they were
    // judged against.
    let mut value: Value = match serde_json::from_str(base.trim()) {
        Ok(v) => v,
        Err(_) => return base,
    };
    if let Some(obj) = value.as_object_mut() {
        if let Some(inv) = inventory {
            obj.insert(
                "inventory".into(),
                serde_json::to_value(inv).unwrap_or(Value::Null),
            );
        }
        obj.insert("mux".into(), mux_sweep_json(mux));
    }
    format!("{}\n", value)
}

/// The parsed receipt of one `fno mux workspace prune --json` reading: the
/// tab fold's outcome, named per tab, plus the sessions that never answered
/// the pane probe.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneReceipt {
    pub closed: usize,
    pub would_close: usize,
    pub close_named: Vec<String>,
    pub sessions_unreachable: Vec<String>,
    pub notice: Option<String>,
}

/// Parse the prune verb's JSON receipt, fail-closed: `None` over a zeroed
/// report. An unparsable stdout must never read as a clean zero - the same
/// rule `parse_stale_sweep` (daemon.rs) already states (AC3-EDGE).
pub fn parse_prune_receipt(stdout: &str) -> Option<PruneReceipt> {
    let line = stdout
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with('{'))?;
    let v: Value = serde_json::from_str(line).ok()?;
    let count = |k: &str| -> Option<usize> { usize::try_from(v.get(k)?.as_u64()?).ok() };
    let names = |k: &str| -> Option<Vec<String>> {
        Some(
            v.get(k)?
                .as_array()?
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect(),
        )
    };
    Some(PruneReceipt {
        closed: count("tabs_closed")?,
        would_close: count("tabs_would_close")?,
        close_named: names("tabs_close_named")?,
        sessions_unreachable: names("sessions_unreachable")?,
        notice: v.get("notice").and_then(|n| n.as_str()).map(String::from),
    })
}

/// The mux half of one reap pass (x-91eb), in one of three distinguishable
/// states. `Unread` carries no count field at all, so an unparsable sweep can
/// never render as a measured zero; `Skipped` names the flag that asked for
/// it.
#[derive(Debug, Clone, PartialEq)]
pub enum MuxSweep {
    Ran {
        receipt: PruneReceipt,
    },
    Unread {
        exit_code: Option<i32>,
        stderr_first: String,
    },
    Skipped,
}

impl MuxSweep {
    pub fn state(&self) -> &'static str {
        match self {
            MuxSweep::Ran { .. } => "ran",
            MuxSweep::Unread { .. } => "unread",
            MuxSweep::Skipped => "skipped",
        }
    }
}

/// The `mux` object spliced into the reap JSON receipt. `would_close` exists
/// only in the `ran` state (the plan's readers assert its ABSENCE elsewhere).
pub fn mux_sweep_json(mux: &MuxSweep) -> Value {
    match mux {
        MuxSweep::Ran { receipt } => json!({
            "state": "ran",
            "closed": receipt.closed,
            "would_close": receipt.would_close,
            "tabs_close_named": receipt.close_named,
            "sessions_unreachable": receipt.sessions_unreachable,
            "notice": receipt.notice,
        }),
        MuxSweep::Unread {
            exit_code,
            stderr_first,
        } => json!({
            "state": "unread",
            "exit_code": exit_code,
            "stderr_first": stderr_first,
        }),
        MuxSweep::Skipped => json!({"state": "skipped"}),
    }
}

/// The one human-receipt line for the mux half: the closed (or would-close)
/// count plus every label, or the reason the half could not be read, or the
/// flag that skipped it.
pub fn mux_sweep_text_line(mux: &MuxSweep, dry_run: bool) -> String {
    match mux {
        MuxSweep::Ran { receipt } => {
            let verb = if dry_run { "would close" } else { "closed" };
            let count = if dry_run {
                receipt.would_close
            } else {
                receipt.closed
            };
            let mut line = format!("mux sweep (ran): {verb} {count} tab(s)");
            if !receipt.close_named.is_empty() {
                line.push_str(": ");
                line.push_str(&receipt.close_named.join("; "));
            }
            if let Some(notice) = &receipt.notice {
                line.push_str(&format!(" (notice: {notice})"));
            }
            format!("{line}\n")
        }
        MuxSweep::Unread {
            exit_code,
            stderr_first,
        } => match exit_code {
            Some(code) => format!("mux sweep (unread): exit {code}: {stderr_first}\n"),
            None => format!("mux sweep (unread): {stderr_first}\n"),
        },
        MuxSweep::Skipped => "mux sweep (skipped by --no-mux)\n".to_string(),
    }
}

/// (x-1b90 change 3) `i64` seconds as `16h31m`, `59m`, `59s`.
fn hold_age(held_s: i64) -> String {
    let s = held_s.max(0) as u64;
    if s >= 3600 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
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
            "needs_live_stop",
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
        // A rehearsal never confirms a removal - the `pruned` line must
        // carry the same "would" verb the `retired` line already does.
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
            kept_open_work: vec![(
                "b1".into(),
                "N3".into(),
                "in_review".into(),
                "sessions".into(),
            )],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("  kept b1 (open work: N3 in_review; read via sessions)"),
            "{text}"
        );
        let out = render_reap(&s, true, false);
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        assert_eq!(
            v["kept_open_work"],
            json!([{"id": "b1", "node": "N3", "status": "in_review", "reader": "sessions"}])
        );
    }

    /// x-2774 change 4: the no-provenance keep line carries a closing paren
    /// and a real newline. The old spelling emitted an unbalanced paren and
    /// a literal backslash-n; invisible only while the bucket measured
    /// empty.
    #[test]
    fn reap_no_provenance_line_is_well_formed() {
        let s = GcSummary {
            kept_no_provenance: vec!["d1".into()],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        let line = text
            .lines()
            .find(|l| l.contains("kept d1"))
            .expect("the keep line renders");
        assert!(
            line.starts_with("  kept d1 (no provenance:") && line.ends_with(')'),
            "{line}"
        );
        assert!(
            !text.contains("\\n"),
            "no literal backslash-n in stdout: {text}"
        );
    }

    /// x-2774 change 2: a terminal-state retirement names the session state
    /// and the reader in the basis; the all-done basis is byte-identical to
    /// its old string.
    #[test]
    fn reap_retired_bases_spell_their_answer() {
        let s = GcSummary {
            retired: vec![(
                "a1".into(),
                "session terminal: harness state done (via sessions); node N3 in_review".into(),
            )],
            ..Default::default()
        };
        let text = render_reap(&s, false, false);
        assert!(
            text.contains("session terminal: harness state done"),
            "{text}"
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
            kept_open_work: vec![(
                "b1".into(),
                "N3".into(),
                "in_review".into(),
                "sessions".into(),
            )],
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

    fn ran_receipt() -> MuxSweep {
        MuxSweep::Ran {
            receipt: PruneReceipt {
                closed: 2,
                would_close: 0,
                close_named: vec!["main / squad 1 / \u{201c}ghost\u{201d} (tab 4)".into()],
                sessions_unreachable: vec![],
                notice: None,
            },
        }
    }

    #[test]
    fn the_mux_half_renders_in_three_distinguishable_states() {
        let unread = MuxSweep::Unread {
            exit_code: Some(1),
            stderr_first: "socket refused".into(),
        };
        for (mux, dry) in [
            (ran_receipt(), false),
            (ran_receipt(), true),
            (unread, false),
            (MuxSweep::Skipped, false),
        ] {
            let text = render_reap_with_inventory(&summary(&[]), None, Some(&mux), false, dry);
            assert!(
                text.contains(mux.state()),
                "text names the state {:?}: {text}",
                mux.state()
            );
            let out = render_reap_with_inventory(&summary(&[]), None, Some(&mux), true, dry);
            let v: Value = serde_json::from_str(out.trim()).expect("valid json");
            let m = v.get("mux").expect("mux object present in every mode");
            assert_eq!(m["state"], json!(mux.state()), "state word: {m}");
        }
    }

    #[test]
    fn the_ran_state_names_the_tabs_it_closed_with_their_labels() {
        for (dry, count) in [(false, 2), (true, 0)] {
            let out =
                render_reap_with_inventory(&summary(&[]), None, Some(&ran_receipt()), true, dry);
            let v: Value = serde_json::from_str(out.trim()).expect("valid json");
            assert_eq!(v["mux"]["closed"], json!(2));
            assert_eq!(v["mux"]["would_close"], json!(0));
            assert_eq!(
                v["mux"]["tabs_close_named"],
                json!(["main / squad 1 / \u{201c}ghost\u{201d} (tab 4)"]),
                "the labels ride the JSON: the operator judges the pass"
            );
            let text =
                render_reap_with_inventory(&summary(&[]), None, Some(&ran_receipt()), false, dry);
            assert!(text.contains("ghost"), "the label rides the text: {text}");
        }
    }

    #[test]
    fn the_unread_state_never_carries_a_count() {
        // AC3-EDGE: an unparsable sweep is `unread`, never a measured zero -
        // would_close must be ABSENT, not 0 (the plan's reader asserts it).
        let out = render_reap_with_inventory(
            &summary(&[]),
            None,
            Some(&MuxSweep::Unread {
                exit_code: Some(1),
                stderr_first: "boom".into(),
            }),
            true,
            false,
        );
        let v: Value = serde_json::from_str(out.trim()).expect("valid json");
        let m = v["mux"].as_object().expect("mux object");
        assert_eq!(m["state"], json!("unread"));
        assert!(!m.contains_key("would_close"), "{m:?}");
        assert!(!m.contains_key("closed"), "{m:?}");
        assert_eq!(m["exit_code"], json!(1));
        let text = render_reap_with_inventory(
            &summary(&[]),
            None,
            Some(&MuxSweep::Unread {
                exit_code: None,
                stderr_first: "No such file or directory".into(),
            }),
            false,
            false,
        );
        assert!(
            text.contains("No such file or directory"),
            "the spawn failure rides the text line: {text}"
        );
    }

    #[test]
    fn parse_prune_receipt_fails_closed_on_garbage_and_reads_the_real_keys() {
        // (x-91eb) None over a zeroed report: garbage stdout and a receipt
        // missing the tab counts parse as None; the real verb's keys parse
        // into the receipt.
        assert_eq!(parse_prune_receipt("nothing to prune"), None);
        assert_eq!(parse_prune_receipt("{\"tabs_kept\": 5}"), None);
        let receipt = parse_prune_receipt(
            "{\"tabs_closed\": 1, \"tabs_would_close\": 0, \
             \"tabs_close_named\": [\"s / q / tab 2\"], \
             \"sessions_unreachable\": [\"dead-host\"], \
             \"notice\": \"server liveness incomplete\"}",
        )
        .expect("the real shape parses");
        assert_eq!(receipt.closed, 1);
        assert_eq!(receipt.would_close, 0);
        assert_eq!(receipt.close_named, vec!["s / q / tab 2".to_string()]);
        assert_eq!(receipt.sessions_unreachable, vec!["dead-host".to_string()]);
        assert_eq!(
            receipt.notice.as_deref(),
            Some("server liveness incomplete")
        );
    }

    #[test]
    fn state_file_reap_json_keeps_all_five_zero_count_families() {
        let summary = crate::gc_sweep::StateFilesReapSummary::default();
        let out = render_state_files_reap(&summary, true);
        let value: Value = serde_json::from_str(out.trim()).expect("valid json");

        for family in [
            "expired_claims",
            "plan_locks",
            "agent_locks",
            "pr_status_cache",
            "claim_tmp",
        ] {
            assert_eq!(value["families"][family]["scanned"], json!(0));
            assert_eq!(value["families"][family]["deleted"], json!(0));
            assert_eq!(value["families"][family]["would_delete"], json!(0));
            assert_eq!(value["families"][family]["bytes"], json!(0));
            assert!(value["families"][family].get("oldest_age_s").is_some());
            assert_eq!(value["families"][family]["kept"], json!([]));
        }
        assert_eq!(value["totals"]["scanned"], json!(0));
        assert_eq!(value["totals"]["deleted"], json!(0));
        assert_eq!(value["totals"]["would_delete"], json!(0));
        assert_eq!(value["applied"], json!(false));
        assert_eq!(value["dry_run"], json!(true));
        assert_eq!(value["skip_reason"], Value::Null);
    }

    #[test]
    fn state_file_reap_text_names_each_family_total_and_dry_run() {
        let mut summary = crate::gc_sweep::StateFilesReapSummary::default();
        summary.plan_locks.kept = vec![
            crate::gc_sweep::StateReapKept {
                path: "locks/plan.lock".into(),
                reason: "within retention window".into(),
            };
            10_000
        ];
        summary.totals.kept = 10_000;
        let out = render_state_files_reap(&summary, false);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 7, "five families, total, and dry-run marker");
        assert!(
            out.len() < 1_024,
            "text output grew with kept paths: {}B",
            out.len()
        );
        assert!(lines[0].starts_with("expired_claims:"));
        assert!(lines[1].starts_with("plan_locks:"));
        assert!(lines[1].contains("within retention window=10000"));
        assert!(lines[2].starts_with("agent_locks:"));
        assert!(lines[3].starts_with("pr_status_cache:"));
        assert!(lines[4].starts_with("claim_tmp:"));
        assert!(lines[5].starts_with("total:"));
        assert_eq!(lines[6], "(dry-run: no changes made)");
    }

    /// (x-1b90 change 3) AC3-HP: a 17h hold on done work names its age and
    /// ends with the decision. AC3-EDGE: a 2h hold carries no decision.
    /// AC3-ERR: an old hold whose node is not done carries no decision.
    #[test]
    fn an_unresolved_hold_names_its_age_and_asks_for_a_decision_when_old_and_done() {
        let hold = |held_s: i64, nodes_done: bool| UnresolvedHold {
            id: "bp-ebd2-verb-law".into(),
            held_s,
            nodes_done,
        };
        // AC3-HP
        let mut s = summary(&[]);
        s.kept_transcript_unresolved
            .push(hold(17 * 3600 + 31 * 60, true));
        let text = render_reap(&s, false, true);
        let line = text
            .lines()
            .find(|l| l.contains("bp-ebd2-verb-law"))
            .expect("the hold line renders");
        assert!(line.contains("transcript unresolved for 17h31m"), "{line}");
        assert!(
            line.ends_with("needs a decision: fno agents rm bp-ebd2-verb-law)"),
            "{line}"
        );

        // AC3-EDGE
        let mut s = summary(&[]);
        s.kept_transcript_unresolved.push(hold(2 * 3600, true));
        let line = render_reap(&s, false, true);
        assert!(line.contains("transcript unresolved for 2h0m"), "{line}");
        assert!(!line.contains("needs a decision"), "{line}");

        // AC3-ERR
        let mut s = summary(&[]);
        s.kept_transcript_unresolved.push(hold(9 * 3600, false));
        let line = render_reap(&s, false, true);
        assert!(line.contains("transcript unresolved for 9h"), "{line}");
        assert!(!line.contains("needs a decision"), "{line}");

        // The JSON rows carry the fields the Python projection reads.
        let mut s = summary(&[]);
        s.kept_transcript_unresolved.push(hold(7 * 3600, true));
        let v: Value = serde_json::from_str(render_reap(&s, true, true).trim()).unwrap();
        let row = &v["kept_transcript_unresolved"][0];
        assert_eq!(row["id"], "bp-ebd2-verb-law");
        assert_eq!(row["held_s"], 7 * 3600);
        assert_eq!(row["nodes_done"], true);
    }
}
