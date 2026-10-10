//! A run that ends at a non-delivery terminal routes its next step.
//! At finalize, the terminal becomes a help event on the help router:
//! Budget, NoProgress, Aborted and the unreviewed Done pair take a routed
//! step instead of leaving a postmortem nobody reads.
//!
//! Interrupted routes nothing: a user or lead cancel is respected, and the
//! self-written sentinel already routes stuck at the loop layer (the loop
//! consumes the sentinel and emits the stuck row itself), so finalize stays
//! out of that path.
//!
//! The unreviewed-Done arms come from the node's 2026-10-06 progress note:
//! reviewer unavailable (bot quota, no peer lane) goes to the lead as
//! gate-unsatisfiable; review only slow rides a Wait timer route (the timer
//! wake re-checks), and after two waits the lead is asked directly: merge
//! unreviewed, or assign a reviewer. "No run ends at green and sits."
//!
//! The ladder axis is the NODE, not the run: prior terminals come from the
//! ledger's per-session rows (graph_node_id + termination_reason), which
//! finalize writes before this step runs. A second non-delivery terminal on
//! one node also asks the lead a question carrying both postmortems (the
//! pair rule). The claim moves with the route (the claim-handoff
//! discipline): an off-session route releases the node claim
//! (holder-verified, so a run that held no claim is a silent no-op), and a
//! timer route keeps it, since the same session resumes into its claim.
//!
//! Every leg is best-effort: a failed write or wake logs one stderr note
//! and never changes finalize's verdict.

use crate::distress::HelpClass;
use serde_json::Value;
use std::path::Path;

/// Ledger reasons that prove the node can deliver: the budget ladder's
/// progress signal (a node that shipped before gets a lead question on its
/// second budget trip; a node that never shipped is stuck).
const DELIVERED_REASONS: &[&str] = &["DonePRGreen", "DoneAdvisory", "DoneDelivery"];

/// What finalize knows at the terminal; the decision joins these to the
/// ledger and the journals.
pub(crate) struct TerminalRouteFacts<'a> {
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub node: Option<&'a str>,
    /// The TerminationReason wire name ("Budget", "DoneUnreviewed", ...).
    pub reason: &'a str,
    /// This run's postmortem path, when the eval artifact was written.
    pub postmortem: Option<&'a str>,
    /// This run's wall-clock age in minutes at the terminal, from the
    /// manifest's created_at, so the budget evidence quotes the run's
    /// elapsed and not the seconds since the termination event fired.
    pub elapsed_min: Option<u64>,
    pub claim_key: Option<&'a str>,
    pub claim_holder: Option<&'a str>,
    pub project_events: &'a Path,
    pub global_events: &'a Path,
}

/// Decide, emit, route, and move the claim. Returns the class routed, for
/// the finalize log line.
pub(crate) fn route_terminal(f: &TerminalRouteFacts<'_>) -> Option<HelpClass> {
    let ledger = read_ledger(f.cwd);
    let (class, rung, evidence) = decide(f, &ledger)?;
    let turn_key = format!("terminal:{}", f.reason);
    if terminal_row_exists(f, &turn_key) {
        return None; // idempotent finalize retry: row and route already exist
    }
    let prior_pair_row = f.node.and_then(|n| newest_prior_terminal_row(f, n));
    append_terminal_row(
        f,
        &class,
        &format!("terminal {}", f.reason),
        rung,
        &turn_key,
        &evidence,
    );
    crate::help_router::route_emitted_distress(
        f.cwd,
        f.session_id,
        f.node,
        class,
        &format!("terminal {}", f.reason),
        Some(&evidence),
        rung,
        &turn_key,
    );
    move_claim_with_route(f, class, rung);
    pair_question(f, prior_pair_row.as_ref(), &evidence, class);
    Some(class)
}

/// The route table for terminals. Rungs are node-scoped (prior terminals),
/// not run-scoped: a run that ends has no more in-session turns, so every
/// terminal class enters at or past the escalation rung. Budget's first
/// trip rides the shipped Budget rung-0 timer (the resume leg); the
/// unreviewed-Done ladder rides Wait's timer (re-check) then Question
/// (merge unreviewed, or assign a reviewer). DoneAwaitingMerge stays out:
/// its merge-guard owns the follow-up. Interrupted is absent on purpose
/// (respected; see the module doc).
fn decide(f: &TerminalRouteFacts<'_>, ledger: &[Value]) -> Option<(HelpClass, u64, String)> {
    let mine = || -> String { terminal_evidence(f) };
    let priors = ledger_priors(f, ledger);
    let count = |reasons: &[&str]| -> u64 {
        priors
            .iter()
            .filter(|(_, reason)| reasons.contains(&reason.as_str()))
            .count() as u64
    };

    match f.reason {
        "Budget" => {
            let prior_budget = count(&["Budget"]);
            if prior_budget == 0 {
                Some((HelpClass::Budget, 0, mine()))
            } else if count(DELIVERED_REASONS) > 0 {
                // The node shipped before: the second trip asks the lead to
                // re-scope or raise the cap, with the spend.
                Some((HelpClass::Budget, 1, mine()))
            } else {
                // No delivered step between fires: a resume burns the cap
                // again for nothing, so the run routes stuck instead.
                Some((HelpClass::Stuck, 2, mine()))
            }
        }
        "NoProgress" => Some((HelpClass::Stuck, 2, mine())),
        "Aborted" => Some((HelpClass::Unclassified, 0, mine())),
        "DoneUnreviewed" => {
            let prior_review = count(&["DoneUnreviewed", "DoneAwaitingReview"]);
            if prior_review >= 2 {
                // Two waits already fired; a third wait just burns quota, so
                // the lead decides: merge unreviewed, or assign a reviewer.
                Some((HelpClass::Question, 0, mine()))
            } else {
                // Review only slow: a timer re-check (5m, then 10m). "No run
                // ends at green and sits."
                Some((HelpClass::Wait, prior_review, mine()))
            }
        }
        "DoneAwaitingReview" => Some((HelpClass::GateUnsatisfiable, 0, mine())),
        _ => None,
    }
}

/// The evidence line: axis facts from this session's budget termination
/// event, the run's spend and elapsed, and the postmortem path, so the
/// recipient acts without opening the journals.
fn terminal_evidence(f: &TerminalRouteFacts<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if f.reason == "Budget" {
        let term = newest_termination_row(f);
        if let Some((axis, cap, value)) = term.as_ref().and_then(budget_axis_facts) {
            let mut fact = format!("axis={axis} value={value}");
            if let Some(c) = cap {
                fact.push_str(&format!(" cap={c}"));
            }
            parts.push(fact);
        }
        let spend = crate::loopcheck::session_cost_from_ledger(
            &crate::paths::ledger_path(f.cwd),
            f.session_id,
        );
        parts.push(format!("spend=${:.2}", spend));
    }
    if let Some(mins) = f.elapsed_min {
        parts.push(format!("elapsed={mins}min"));
    }
    if let Some(pm) = f.postmortem.filter(|p| !p.trim().is_empty()) {
        parts.push(format!("postmortem={pm}"));
    }
    if parts.is_empty() {
        return format!("terminal {} on node {:?}", f.reason, f.node);
    }
    parts.join("; ")
}

/// The budget cap axis facts off this session's termination event (the
/// loopcheck emit carries axis, cap and value since the terminal route).
fn budget_axis_facts(row: &Value) -> Option<(String, Option<String>, String)> {
    let data = row.get("data")?;
    let axis = data.get("axis")?.as_str()?.to_string();
    let cap = data.get("cap").map(|c| match c {
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    });
    let value = data.get("value")?.to_string();
    Some((axis, cap, value))
}

/// The newest termination event for this session, from either journal.
fn newest_termination_row(f: &TerminalRouteFacts<'_>) -> Option<Value> {
    let mut best: Option<(i64, Value)> = None;
    for path in [f.global_events, f.project_events] {
        let lines = crate::event_store::journal_text(path, &["termination"]);
        for line in lines.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.pointer("/data/session_id").and_then(Value::as_str) != Some(f.session_id) {
                continue;
            }
            let ts = row_ts(&row);
            if best.as_ref().is_none_or(|(t, _)| ts >= *t) {
                best = Some((ts, row));
            }
        }
    }
    best.map(|(_, row)| row)
}

/// Prior ledger rows for the node as (session, reason) pairs, excluding
/// this run: the node-scoped history the ladders count.
fn ledger_priors(f: &TerminalRouteFacts<'_>, ledger: &[Value]) -> Vec<(String, String)> {
    ledger
        .iter()
        .filter_map(|row| {
            let node = row.get("graph_node_id").and_then(Value::as_str)?;
            if node != f.node? {
                return None;
            }
            let session = row
                .get("fno_id")
                .and_then(Value::as_str)
                .or_else(|| row.get("session_id").and_then(Value::as_str))?;
            if session == f.session_id {
                return None;
            }
            let reason = row.get("termination_reason").and_then(Value::as_str)?;
            Some((session.to_string(), reason.to_string()))
        })
        .collect()
}

/// The ledger as JSON rows; unreadable is an empty history.
fn read_ledger(cwd: &Path) -> Vec<Value> {
    let Ok(content) = std::fs::read_to_string(crate::paths::ledger_path(cwd)) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Value>>(&content).unwrap_or_default()
}

/// Idempotency: a finalize retry after a partial failure must not re-route.
/// A prior row with this run + turn key means the route already fired.
fn terminal_row_exists(f: &TerminalRouteFacts<'_>, turn_key: &str) -> bool {
    for path in [f.project_events, f.global_events] {
        let lines = crate::event_store::journal_text(path, &["blocked"]);
        for line in lines.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.get("run").and_then(Value::as_str) == Some(f.session_id)
                && row.pointer("/data/turn").and_then(Value::as_str) == Some(turn_key)
            {
                return true;
            }
        }
    }
    false
}

/// The `blocked` row's data object: the distress payload with
/// `kind = "terminal"` so readers tell a terminal from a tag, plus the
/// postmortem path the pair rule reads from a prior run's row.
fn terminal_data(
    f: &TerminalRouteFacts<'_>,
    class: &HelpClass,
    reason_text: &str,
    rung: u64,
    turn_key: &str,
    evidence: &str,
) -> Value {
    serde_json::json!({
        "reason": cap500(reason_text),
        "kind": "terminal",
        "class": class.as_str(),
        "postmortem": f.postmortem,
        "turn": turn_key,
        "rung": rung,
        "evidence": cap500(evidence),
    })
}

/// The `blocked` row the terminal becomes: the distress envelope with
/// `data.kind = "terminal"`, so readers tell a terminal from a tag.
/// Mirrored to both journals like every blocked row.
fn append_terminal_row(
    f: &TerminalRouteFacts<'_>,
    class: &HelpClass,
    reason_text: &str,
    rung: u64,
    turn_key: &str,
    evidence: &str,
) {
    let data = terminal_data(f, class, reason_text, rung, turn_key, evidence);
    let mut env = serde_json::json!({
        "ts": crate::loopcheck::now_rfc3339_utc(),
        "v": 1,
        "type": "blocked",
        "source": "finalize",
        "run": f.session_id,
        "data": data,
    });
    if let Some(n) = f.node {
        env["node"] = serde_json::json!(n);
    }
    for path in [f.project_events, f.global_events] {
        if let Err(error) =
            crate::claims::append_event_line(path, &env, std::time::Duration::from_secs(2))
        {
            eprintln!(
                "terminal-route: blocked write to {} failed (non-fatal): {error}",
                path.display()
            );
        }
    }
}

/// The newest prior terminal row on this node, for the pair rule's both
/// postmortems: this run's row carries its own path, so the pair question
/// reads the prior run's path from its row.
fn newest_prior_terminal_row(f: &TerminalRouteFacts<'_>, node: &str) -> Option<Value> {
    let mut best: Option<(i64, Value)> = None;
    for path in [f.global_events, f.project_events] {
        let lines = crate::event_store::journal_text(path, &["blocked"]);
        for line in lines.lines() {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if row.get("node").and_then(Value::as_str) != Some(node)
                || row.pointer("/data/kind").and_then(Value::as_str) != Some("terminal")
                || row.get("run").and_then(Value::as_str) == Some(f.session_id)
            {
                continue;
            }
            let ts = row_ts(&row);
            if best.as_ref().is_none_or(|(t, _)| ts >= *t) {
                best = Some((ts, row));
            }
        }
    }
    best.map(|(_, row)| row)
}

/// The pair rule: a second non-delivery terminal on one node asks the lead
/// a question carrying both postmortems, so the pattern (not just this
/// run's shape) gets a ruling. Rides the Question route (the ladder), and
/// skips when the terminal itself already routed as Question.
fn pair_question(
    f: &TerminalRouteFacts<'_>,
    prior: Option<&Value>,
    evidence: &str,
    class: HelpClass,
) {
    let Some(node) = f.node else {
        return;
    };
    let prior = match prior {
        Some(p) => p,
        None => return,
    };
    if class == HelpClass::Question {
        return;
    }
    let q_evidence = pair_evidence(node, prior, f.postmortem, evidence);
    let turn = format!("terminal-pair:{node}");
    if terminal_row_exists(f, &turn) {
        return;
    }
    append_terminal_row(
        f,
        &HelpClass::Question,
        "terminal pair",
        0,
        &turn,
        &q_evidence,
    );
    crate::help_router::route_emitted_distress(
        f.cwd,
        f.session_id,
        f.node,
        HelpClass::Question,
        &format!("terminal pair on node {node}"),
        Some(&q_evidence),
        0,
        &turn,
    );
}

/// The pair question's evidence line: both postmortems, the prior run's
/// read from its terminal row, and this run's own evidence tail.
fn pair_evidence(node: &str, prior: &Value, postmortem: Option<&str>, evidence: &str) -> String {
    let prior_run = prior
        .get("run")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let prior_pm = prior
        .pointer("/data/postmortem")
        .and_then(Value::as_str)
        .unwrap_or("unrecorded (row predates the terminal route)");
    let mut q_evidence = format!(
        "second non-delivery terminal on node {node} (prior run {prior_run}); postmortems: {} and {prior_pm}",
        postmortem.unwrap_or("unrecorded")
    );
    q_evidence.push_str(&format!("; this run: {evidence}"));
    q_evidence
}

/// The claim moves with the route (the claim-handoff discipline): an
/// off-session route frees the node for the next owner now,
/// holder-verified and idempotent, so the claim never waits for a reaper;
/// a timer route keeps it, since the same session resumes into its claim.
fn move_claim_with_route(f: &TerminalRouteFacts<'_>, class: HelpClass, rung: u64) {
    let step = crate::help_router::route(class, rung);
    if !matches!(step, crate::help_router::Route::OffSession { .. }) {
        return;
    }
    let key = f
        .claim_key
        .map(str::to_string)
        .or_else(|| f.node.map(|n| format!("node:{n}")));
    let Some(key) = key else {
        return;
    };
    let holder = f
        .claim_holder
        .filter(|h| !h.trim().is_empty())
        .unwrap_or(f.session_id);
    if let Err(error) = crate::claims::release(&key, &holder, None, None) {
        eprintln!("terminal-route: claim release failed (non-fatal): {error}");
    }
}

fn cap500(s: &str) -> String {
    s.chars().take(500).collect()
}

fn row_ts(row: &Value) -> i64 {
    row.get("ts")
        .and_then(Value::as_str)
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn led(json: &str) -> Vec<Value> {
        serde_json::from_str::<Vec<Value>>(json).unwrap()
    }

    fn facts<'a>(
        session: &'a str,
        node: Option<&'a str>,
        reason: &'a str,
    ) -> TerminalRouteFacts<'a> {
        TerminalRouteFacts {
            cwd: Path::new("/nonexistent-cwd-for-decide-only"),
            session_id: session,
            node,
            reason,
            postmortem: None,
            elapsed_min: None,
            claim_key: None,
            claim_holder: None,
            project_events: Path::new("/nonexistent-events"),
            global_events: Path::new("/nonexistent-events"),
        }
    }

    #[test]
    fn budget_first_trip_routes_budget_at_rung_zero() {
        let ledger = led(&format!(
            r#"[{{"fno_id":"a1","graph_node_id":"x-n","termination_reason":"Budget"}}]"#
        ));
        let f = facts("cur", Some("x-n"), "Budget");
        let (class, rung, _) = decide(&f, &ledger).unwrap();
        assert_eq!(class, HelpClass::Budget);
        assert_eq!(rung, 0);
    }

    #[test]
    fn budget_second_trip_without_delivery_routes_stuck() {
        let ledger = led(r#"[
            {"fno_id":"a1","graph_node_id":"x-n","termination_reason":"Budget"},
            {"fno_id":"a2","graph_node_id":"x-n","termination_reason":"Budget"},
            {"fno_id":"cur","graph_node_id":"x-n","termination_reason":"Budget"}
        ]"#);
        let f = facts("cur", Some("x-n"), "Budget");
        let (class, rung, _) = decide(&f, &ledger).unwrap();
        assert_eq!(class, HelpClass::Stuck);
        assert_eq!(rung, 2);
    }

    #[test]
    fn budget_second_trip_after_delivery_asks_the_lead() {
        let ledger = led(r#"[
            {"fno_id":"a1","graph_node_id":"x-n","termination_reason":"Budget"},
            {"fno_id":"a2","graph_node_id":"x-n","termination_reason":"DonePRGreen"},
            {"fno_id":"cur","graph_node_id":"x-n","termination_reason":"Budget"}
        ]"#);
        let f = facts("cur", Some("x-n"), "Budget");
        let (class, rung, _) = decide(&f, &ledger).unwrap();
        assert_eq!(class, HelpClass::Budget);
        assert_eq!(rung, 1);
    }

    #[test]
    fn other_nodes_rows_never_count_as_priors() {
        let ledger = led(r#"[
            {"fno_id":"a1","graph_node_id":"other","termination_reason":"Budget"},
            {"fno_id":"cur","graph_node_id":"x-n","termination_reason":"Budget"}
        ]"#);
        let f = facts("cur", Some("x-n"), "Budget");
        let (class, rung, _) = decide(&f, &ledger).unwrap();
        assert_eq!(class, HelpClass::Budget);
        assert_eq!(rung, 0);
    }

    #[test]
    fn noprogress_and_aborted_take_their_classes() {
        let empty: Vec<Value> = Vec::new();
        let f = facts("cur", Some("x-n"), "NoProgress");
        let (class, rung, _) = decide(&f, &empty).unwrap();
        assert_eq!(class, HelpClass::Stuck);
        assert_eq!(rung, 2);

        let f = facts("cur", Some("x-n"), "Aborted");
        let (class, rung, _) = decide(&f, &empty).unwrap();
        assert_eq!(class, HelpClass::Unclassified);
        assert_eq!(rung, 0);
    }

    #[test]
    fn unreviewed_ladder_waits_twice_then_asks() {
        let empty: Vec<Value> = Vec::new();
        let f = facts("cur", Some("x-n"), "DoneUnreviewed");
        let (class, rung, _) = decide(&f, &empty).unwrap();
        assert_eq!(class, HelpClass::Wait);
        assert_eq!(rung, 0);

        let ledger =
            led(r#"[{"fno_id":"a1","graph_node_id":"x-n","termination_reason":"DoneUnreviewed"}]"#);
        let f = facts("cur", Some("x-n"), "DoneUnreviewed");
        let (class, rung, _) = decide(&f, &ledger).unwrap();
        assert_eq!(class, HelpClass::Wait);
        assert_eq!(rung, 1);

        let ledger = led(r#"[
            {"fno_id":"a1","graph_node_id":"x-n","termination_reason":"DoneUnreviewed"},
            {"fno_id":"a2","graph_node_id":"x-n","termination_reason":"DoneUnreviewed"},
            {"fno_id":"cur","graph_node_id":"x-n","termination_reason":"DoneUnreviewed"}
        ]"#);
        let f = facts("cur", Some("x-n"), "DoneUnreviewed");
        let (class, rung, _) = decide(&f, &ledger).unwrap();
        assert_eq!(class, HelpClass::Question);
        assert_eq!(rung, 0);
    }

    #[test]
    fn awaiting_review_routes_gate_unsatisfiable() {
        let empty: Vec<Value> = Vec::new();
        let f = facts("cur", Some("x-n"), "DoneAwaitingReview");
        let (class, rung, _) = decide(&f, &empty).unwrap();
        assert_eq!(class, HelpClass::GateUnsatisfiable);
        assert_eq!(rung, 0);
    }

    #[test]
    fn interrupted_and_delivered_route_nothing() {
        let empty: Vec<Value> = Vec::new();
        for reason in ["Interrupted", "DonePRGreen", "HeldOnQuestion", "NoWork"] {
            let f = facts("cur", Some("x-n"), reason);
            assert!(decide(&f, &empty).is_none());
        }
    }

    #[test]
    fn terminal_row_data_carries_the_postmortem_key() {
        let mut f = facts("cur", Some("x-n"), "Budget");
        f.postmortem = Some("/tmp/pm-cur.md");
        let data = terminal_data(
            &f,
            &HelpClass::Budget,
            "terminal Budget",
            0,
            "terminal:Budget",
            "axis=cost value=1.5 cap=0.5",
        );
        assert_eq!(
            data.get("postmortem").and_then(Value::as_str),
            Some("/tmp/pm-cur.md"),
            "row data must carry the run's postmortem path: the pair rule reads it from a prior run's row"
        );
        let bare = facts("cur", Some("x-n"), "Budget");
        let data = terminal_data(
            &bare,
            &HelpClass::Budget,
            "terminal Budget",
            0,
            "terminal:Budget",
            "ev",
        );
        assert_eq!(
            data.get("postmortem").map(Value::is_null),
            Some(true),
            "absent postmortem reads as a null key, never an omitted key"
        );
    }

    #[test]
    fn pair_evidence_names_both_postmortems_from_the_prior_row() {
        let prior = serde_json::json!({
            "run": "a1",
            "data": {"kind": "terminal", "postmortem": "/tmp/pm-a1.md"}
        });
        let ev = pair_evidence(
            "x-n",
            &prior,
            Some("/tmp/pm-cur.md"),
            "axis=cost value=1.5 cap=0.5; spend=$1.50",
        );
        assert!(
            ev.contains("postmortems: /tmp/pm-cur.md and /tmp/pm-a1.md"),
            "pair evidence must carry both postmortem paths: {ev}"
        );
        let legacy = serde_json::json!({"run": "a0", "data": {"kind": "terminal"}});
        let ev = pair_evidence("x-n", &legacy, None, "terminal Budget on node");
        assert!(
            ev.contains("unrecorded (row predates the terminal route)"),
            "a prior row without a postmortem key reads as unrecorded: {ev}"
        );
    }

    #[test]
    fn budget_axis_facts_read_cap_and_value_off_the_termination_row() {
        let row = serde_json::json!({
            "ts": "2026-10-10T12:00:00Z",
            "data": {"session_id": "cur", "axis": "cost", "cap": 0.5, "value": 1.5}
        });
        let (axis, cap, value) = budget_axis_facts(&row).unwrap();
        assert_eq!(axis, "cost");
        assert_eq!(cap.as_deref(), Some("0.5"));
        assert_eq!(value, "1.5");
        let bare = serde_json::json!({
            "ts": "2026-10-10T12:00:00Z",
            "data": {"session_id": "cur", "axis": "cost", "value": 1.5}
        });
        let (axis, cap, _) = budget_axis_facts(&bare).unwrap();
        assert_eq!(axis, "cost");
        assert_eq!(cap, None, "malformed cap reads as absent, trip stays real");
    }
}
