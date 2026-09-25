//! The retask transaction, native: clear one finished worker's pane, restamp
//! its registry row through a verified session succession, rename the row to
//! the next dispatch name, and resubmit the node's command. Ported from the
//! Python transaction that `fno agents retask` used to own; the Python leaf
//! stays the front door and hands the whole transaction over as one JSON
//! payload.
//!
//! # Payload contract (op-in-payload on the `rename` client action)
//!
//! `fno-agents rename` with NO argv reads one JSON payload on stdin:
//!
//! ```json
//! {
//!   "op": "retask",
//!   "worker": "bp-x0e67-planner",
//!   "node": "x-bbbb",
//!   "target": {
//!     "harness": "codex", "provider": null, "model": "gpt-5.6-sol",
//!     "effort": "high", "substrate": null, "permission_mode": null,
//!     "route": null, "account": null, "verb": "target"
//!   },
//!   "target_command": "$fno:target --no-merge x-bbbb",
//!   "mux": {"session": "main", "pane_id": 12}
//! }
//! ```
//!
//! `mux` is absent for a thread row: the Python front opened the thread's
//! dedicated viewport (`resolve_thread_viewport`, the ONE implementation) and
//! passes the opened `{"session", "pane_id"}` in the same shape instead. The
//! Rust side re-reads the worker row from the registry itself
//! (`find_agent_entry`), so the row it acts on is read at run time, never
//! trusted from the payload. The `target` object is trusted: the payload's
//! only producer is the Python front door, the action is unadvertised, and a
//! hand-written payload with a wrong tier would switch the pane to that tier.
//!
//! Two optional keys carry store paths the Python front resolved through the
//! config layer: `"graph"` (graph.json, honoring `config.paths.graph_json`)
//! and `"registry"` (registry.json, honoring run_retask's registry_path).
//! Absent keys fall back to the ambient resolution.
//!
//! Exit codes: 0 with the receipt as one stdout JSON line for both
//! `retasked` and `refused`; 2 for a usage or payload error (stderr names the
//! problem). The receipt keys and refusal words are byte-for-byte the ones
//! the Python transaction emitted, because the king loop and
//! `fno backlog advance` read them.

use serde_json::{json, Value};

use crate::harness_capabilities::{HarnessContract, ModelSwitchStrategy};

/// The target tier and axes the retasked worker should end up serving.
/// Mirrors Python's `RetaskCoordinate`; built from the payload's `target`.
#[derive(Debug, Clone, PartialEq)]
pub struct RetaskTarget {
    pub harness: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub substrate: Option<String>,
    pub permission_mode: Option<String>,
    pub route: Option<String>,
    pub account: Option<String>,
    /// The node's next lifecycle verb; profiles.<verb> supplies the tier.
    pub verb: String,
}

impl RetaskTarget {
    pub fn from_payload(value: &Value) -> Result<Self, String> {
        let obj = value
            .as_object()
            .ok_or("payload target must be an object")?;
        let harness = obj
            .get("harness")
            .and_then(Value::as_str)
            .ok_or("payload target needs harness")?
            .to_string();
        let get = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Ok(Self {
            harness,
            provider: get("provider"),
            model: get("model"),
            effort: get("effort"),
            substrate: get("substrate"),
            permission_mode: get("permission_mode"),
            route: get("route"),
            account: get("account"),
            verb: get("verb").unwrap_or_else(|| "target".to_string()),
        })
    }
}

/// The worker-row facts the transaction reads. Built from the registry row
/// the transport resolved at run time; never carried in the payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RetaskRow {
    pub name: String,
    pub harness: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub substrate: Option<String>,
    pub status_live: bool,
    pub harness_session_id: Option<String>,
    pub launch_account: Option<String>,
    /// `(session, pane_id)` when the row carries a live mux ref.
    pub mux: Option<(String, u64)>,
    /// The stable thread identity (`fno_id`), the thread substrate's ref.
    pub thread_id: Option<String>,
}

/// The detect verdict. Order of the underlying checks is load-bearing: an
/// earlier refusal names the row defect instead of a tier that would also
/// have mismatched.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectOutcome {
    /// `refused` | `spawn_required` | `switch_pending` | `retask_ready`.
    pub outcome: &'static str,
    pub reason: Option<String>,
}

/// A bounded pane transport death. The receipt is built by the caller from
/// the seam's partial-state record, so a mid-transaction death still reports
/// the true pane state.
#[derive(Debug, Clone)]
pub struct TransportFailure {
    pub reason: String,
    pub detail: Option<String>,
}

/// The live seams `execute_retask` drives. The transport module implements
/// them over `fno mux`; tests inject fakes the way the Python tests injected
/// lambdas.
pub trait RetaskSeams {
    fn read_frame(&mut self) -> Result<String, TransportFailure>;
    fn settle(&mut self) -> Result<(), TransportFailure>;
    /// Send one raw text; `submit` presses enter. `Ok(false)` is an
    /// unconfirmed send (nonzero exit without a named transport failure).
    fn send(&mut self, text: &str, submit: bool) -> Result<bool, TransportFailure>;
    /// The post-clear session transition receipt: the successor session id as
    /// a bare string, or the structured verdict object.
    fn restamp(&mut self) -> Result<Value, TransportFailure>;
    /// Rename the registry row. `None` is a refused rename.
    fn rename(&mut self, new_name: &str) -> Option<String>;
    /// Persist a verified tier onto the renamed row.
    fn project_tier(&mut self, model: &str, effort: &str) -> Result<(), String>;
    /// The screen verdict over one frame, or `None` when no verdict source
    /// exists (which refuses as unobserved, never guesses).
    fn ready_frame(&mut self, frame: &str) -> Option<Value>;
    /// The source/PR authorization, or `None` when the caller supplies none.
    fn source_preflight(&mut self) -> Option<Value>;
}

/// The shared refusal receipt; overrides restate the true partial state.
pub(crate) fn refused_receipt(reason: &str) -> Value {
    json!({
        "status": "refused",
        "cleared": false,
        "session_restamped": false,
        "switch": "not_started",
        "switch_verified": false,
        "target_submit_confirmed": false,
        "reason": reason,
    })
}

fn override_receipt(mut receipt: Value, overrides: Value) -> Value {
    if let (Some(base), Some(extra)) = (receipt.as_object_mut(), overrides.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    receipt
}

/// Normalize the structured transition receipt at the consumer seam: a bare
/// successor string is a minimal succession, a dict passes through, anything
/// else is no verdict.
pub(crate) fn transition_receipt(value: &Value, predecessor: &str) -> Option<Value> {
    if let Some(successor) = value.as_str() {
        if successor == predecessor {
            return None;
        }
        return Some(json!({
            "classification": "succession",
            "predecessor_session_id": predecessor,
            "current_session_id": successor,
            "registry_rows": 1,
            "lineage_recorded": true,
        }));
    }
    if value.is_object() {
        return Some(value.clone());
    }
    None
}

/// The (model, effort) pair one status frame reports, or `None` when the
/// harness's status pattern does not settle both named groups.
pub(crate) fn status_tier(strategy: &ModelSwitchStrategy, frame: &str) -> Option<(String, String)> {
    let pattern = regex::Regex::new(&strategy.status_pattern).ok()?;
    let captures = pattern.captures(frame)?;
    let model = captures.name("model")?.as_str().to_string();
    let effort = captures.name("effort")?.as_str().to_string();
    Some((model, effort))
}

/// Menu rows one frame paints: `(row number, has cursor, label)`.
fn menu_rows(frame: &str) -> Vec<(i64, bool, String)> {
    let row_re = regex::Regex::new(r"^\s*(›\s*)?(\d+)\.\s+(.*)$").expect("static menu regex");
    let mut rows = Vec::new();
    for line in frame.lines() {
        if let Some(captures) = row_re.captures(line) {
            let row: i64 = captures[2].parse().unwrap_or(0);
            let cursor = captures.get(1).is_some();
            rows.push((row, cursor, captures[3].to_string()));
        }
    }
    rows
}

/// How many arrow presses move the menu cursor onto `target`: exact
/// (case-insensitive) match first, then the shortest containing label, so an
/// alias like "sol" lands on the full name over its `-mini` sibling.
pub(crate) fn menu_delta(frame: &str, target: &str) -> Option<i64> {
    let rows = menu_rows(frame);
    let current = rows
        .iter()
        .find(|(_, cursor, _)| *cursor)
        .map(|(row, _, _)| *row);
    let key = target.trim().to_lowercase();
    let containing: Vec<(i64, String)> = rows
        .iter()
        .filter(|(_, _, label)| !key.is_empty() && label.to_lowercase().contains(&key))
        .map(|(row, _, label)| (*row, label.clone()))
        .collect();
    let exact: Vec<i64> = containing
        .iter()
        .filter(|(_, label)| label.trim().to_lowercase() == key)
        .map(|(row, _)| *row)
        .collect();
    let desired = if exact.len() == 1 {
        Some(exact[0])
    } else if !containing.is_empty() {
        containing
            .iter()
            .min_by_key(|(_, label)| label.len())
            .map(|(row, _)| *row)
    } else {
        None
    };
    let current = current?;
    let desired = desired?;
    Some(desired - current)
}

/// Walk the harness's pick-menu onto `target` and confirm with enter.
pub(crate) fn walk_menu<S: RetaskSeams>(
    seams: &mut S,
    frame: &str,
    target: &str,
) -> Result<bool, TransportFailure> {
    let Some(delta) = menu_delta(frame, target) else {
        return Ok(false);
    };
    let arrow = if delta > 0 { "\x1b[B" } else { "\x1b[A" };
    for _ in 0..delta.abs() {
        if !seams.send(arrow, false)? {
            return Ok(false);
        }
    }
    seams.settle()?;
    let verified = seams.read_frame()?;
    if menu_delta(&verified, target) != Some(0) {
        return Ok(false);
    }
    seams.send("", true)
}

/// The strategy bundle one harness's transaction needs, looked up once.
struct TierStrategy {
    strategy: ModelSwitchStrategy,
    ready_marker: String,
}

fn tier_strategy(harness: &str) -> Result<TierStrategy, String> {
    let contract = HarnessContract::packaged().map_err(|e| e.to_string())?;
    let caps = contract.capabilities(harness).map_err(|e| e.to_string())?;
    Ok(TierStrategy {
        strategy: caps.model_switch_strategy.clone(),
        ready_marker: caps.ready_marker.clone(),
    })
}

/// The pre-mutation detect: is this worker retaskable onto `target` at all?
/// Every check names its reason; nothing here mutates a pane.
pub fn detect_retask(
    row: &RetaskRow,
    target: &RetaskTarget,
    live_permission_mode: Option<&str>,
) -> DetectOutcome {
    fn refused(reason: &str) -> DetectOutcome {
        DetectOutcome {
            outcome: "refused",
            reason: Some(reason.to_string()),
        }
    }
    fn spawn_required(reason: &str) -> DetectOutcome {
        DetectOutcome {
            outcome: "spawn_required",
            reason: Some(reason.to_string()),
        }
    }
    if !row.status_live {
        return refused("worker_not_live");
    }
    let substrate = row.substrate.clone().unwrap_or_default();
    if substrate != "pane" && substrate != "thread" {
        return refused("worker_substrate_unknown");
    }
    match substrate.as_str() {
        "pane" => {
            let usable = row
                .mux
                .as_ref()
                .is_some_and(|(session, pane_id)| !session.is_empty() && *pane_id != 0);
            if !usable {
                return refused("worker_has_no_mux_ref");
            }
        }
        "thread" => {
            let usable = row.mux.is_none()
                && row
                    .thread_id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty());
            if !usable {
                return refused("worker_has_no_thread_ref");
            }
        }
        _ => unreachable!("substrate validated above"),
    }
    if row.harness_session_id.as_deref().is_none_or(str::is_empty) {
        return refused("worker_has_no_session_id");
    }

    // Compare against the live worker, never mere presence: config defaults
    // always resolve a permission_mode, and an unobservable mode fails closed.
    if let Some(target_mode) = &target.permission_mode {
        let Some(live_mode) = live_permission_mode else {
            return spawn_required("permission_mode_unobserved");
        };
        if live_mode != target_mode {
            return spawn_required("permission_mode");
        }
    }
    if target.account.is_some() && target.account != row.launch_account {
        return spawn_required("account");
    }
    let target_provider = target.provider.clone().or_else(|| row.provider.clone());
    let target_substrate = target.substrate.clone().or(Some(substrate.clone()));
    let current_axes = (row.harness.clone(), row.provider.clone(), Some(substrate));
    let target_axes = (target.harness.clone(), target_provider, target_substrate);
    for (axis, (current, wanted)) in [
        ("harness", (Some(current_axes.0), Some(target_axes.0))),
        ("provider", (current_axes.1, target_axes.1)),
        ("substrate", (current_axes.2, target_axes.2)),
    ] {
        if current != wanted {
            return spawn_required(axis);
        }
    }

    let desired_model = target
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .or(row.model.clone());
    let desired_effort = target
        .effort
        .clone()
        .filter(|e| !e.is_empty())
        .or(row.effort.clone());
    if (row.model.clone(), row.effort.clone()) != (desired_model, desired_effort) {
        return DetectOutcome {
            outcome: "switch_pending",
            reason: None,
        };
    }
    DetectOutcome {
        outcome: "retask_ready",
        reason: None,
    }
}

/// Run the bounded retask transaction through injected pane seams.
///
/// `Ok(receipt)` is the full receipt (`retasked` or a refusal with every
/// partial-state override the Python transaction set). `Err` is a pane
/// transport death; the caller builds the refusal from the seam's own
/// partial-state record.
pub fn execute_retask<S: RetaskSeams>(
    row: &RetaskRow,
    target: &RetaskTarget,
    node: &str,
    target_command: &str,
    seams: &mut S,
    live_permission_mode: Option<&str>,
) -> Result<Value, TransportFailure> {
    let refusal = refused_receipt("refused");
    let tier = match tier_strategy(&row.harness) {
        Ok(tier) => tier,
        Err(message) => return Ok(override_receipt(refusal, json!({ "reason": message }))),
    };
    let strategy = &tier.strategy;
    if strategy.kind == "unsupported" {
        return Ok(override_receipt(
            refusal,
            json!({ "reason": "unsupported_switch_strategy" }),
        ));
    }
    if let Some(source) = seams.source_preflight() {
        if source.get("status").and_then(Value::as_str) != Some("ready") {
            return Ok(override_receipt(refusal, source));
        }
    }
    let planned = detect_retask(row, target, live_permission_mode);
    if planned.outcome == "spawn_required" || planned.outcome == "refused" {
        let reason = planned
            .reason
            .unwrap_or_else(|| planned.outcome.to_string());
        return Ok(override_receipt(refusal, json!({ "reason": reason })));
    }

    let initial_frame = seams.read_frame()?;
    if initial_frame.trim().is_empty() {
        return Ok(override_receipt(
            refusal,
            json!({ "reason": "pane_frame_unreadable" }),
        ));
    }
    let Some(verdict) = seams.ready_frame(&initial_frame) else {
        return Ok(override_receipt(
            refusal,
            json!({ "reason": "pane_state_unobserved" }),
        ));
    };
    let verdict_ok = verdict.get("matched") == Some(&json!(true))
        && verdict
            .get("rule_id")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
        && verdict
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
    if !verdict_ok {
        return Ok(override_receipt(
            refusal,
            json!({ "reason": "pane_state_unobserved" }),
        ));
    }
    if verdict["state"] != json!("idle") || verdict["rule_id"] != json!(tier.ready_marker) {
        return Ok(override_receipt(
            refusal,
            json!({ "reason": "pane_not_idle" }),
        ));
    }
    if !seams.send("/clear", true)? {
        return Ok(override_receipt(
            refusal,
            json!({ "reason": "clear_not_confirmed" }),
        ));
    }

    let predecessor = row.harness_session_id.clone().unwrap_or_default();
    let raw_transition = seams.restamp()?;
    let Some(transition) = transition_receipt(&raw_transition, &predecessor) else {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "reason": "session_transition_unconfirmed" }),
        ));
    };
    if transition["classification"] != json!("succession") {
        let reason = transition
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("session_transition_not_succession");
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "reason": reason }),
        ));
    }
    if transition["predecessor_session_id"] != json!(predecessor) {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "reason": "clear_predecessor_mismatch" }),
        ));
    }
    let new_session = transition["current_session_id"].as_str().unwrap_or("");
    if new_session.is_empty() || new_session == predecessor {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "reason": "session_transition_unconfirmed" }),
        ));
    }
    if transition["registry_rows"] != json!(1) {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "reason": "successor_row_count_invalid" }),
        ));
    }
    if transition["lineage_recorded"] != json!(true) {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "reason": "successor_lineage_unrecorded" }),
        ));
    }

    let rename_name = match crate::naming::verb_code_for(Some(&target.verb)) {
        Ok(verb_code) => {
            crate::naming::dispatch_agent_name(None, &verb_code, node, None, None, None, None)
        }
        Err(error) => {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "reason": error.to_string() }),
            ));
        }
    };
    let renamed = match rename_name {
        Ok(name) => seams.rename(&name),
        Err(error) => {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "reason": error.to_string() }),
            ));
        }
    };
    let Some(renamed) = renamed else {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "session_restamped": true, "reason": "registry_rename_refused" }),
        ));
    };

    let status_command = &strategy.status_command;
    if !seams.send(status_command, true)? {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "status_not_confirmed" }),
        ));
    }
    let settled = settled_read(seams)?;
    let Some((cleared_model, cleared_effort)) = status_tier(strategy, &settled) else {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "status_unreadable" }),
        ));
    };
    if let Err(_message) = seams.project_tier(&cleared_model, &cleared_effort) {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "registry_projection_failed" }),
        ));
    }

    let desired_model = if target.model.as_deref().is_some_and(|m| !m.is_empty()) {
        target.model.clone().unwrap()
    } else {
        cleared_model.clone()
    };
    let desired_effort = if target.effort.as_deref().is_some_and(|e| !e.is_empty()) {
        target.effort.clone().unwrap()
    } else {
        cleared_effort.clone()
    };
    let switch_needed = cleared_model != desired_model || cleared_effort != desired_effort;
    let switch = if !switch_needed {
        "skipped_same_tier"
    } else if strategy.kind == "direct" {
        let mut confirmed = true;
        for template in &strategy.tokens {
            let command = template
                .replace("{model}", &desired_model)
                .replace("{effort}", &desired_effort);
            if !seams.send(&command, true)? {
                confirmed = false;
                break;
            }
        }
        if !confirmed {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "switch_not_confirmed" }),
            ));
        }
        "switched"
    } else {
        let Some(open) = strategy.tokens.first() else {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "switch_not_confirmed" }),
            ));
        };
        if !seams.send(open, true)? {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "switch_not_confirmed" }),
            ));
        }
        let frame = settled_read(seams)?;
        if !walk_menu(seams, &frame, &desired_model)? {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "model_row_missing" }),
            ));
        }
        let Some(effort_label) = strategy.effort_labels.get(&desired_effort) else {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "effort_label_missing" }),
            ));
        };
        let frame = settled_read(seams)?;
        if !walk_menu(seams, &frame, effort_label)? {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "reason": "effort_row_missing" }),
            ));
        }
        "switched"
    };

    let mut switch_verified = switch == "skipped_same_tier";
    if switch == "switched" {
        if !seams.send(status_command, true)? {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "switch": switch, "reason": "post_switch_status_not_confirmed" }),
            ));
        }
        let settled = settled_read(seams)?;
        let verified = status_tier(strategy, &settled);
        if verified.as_ref() != Some(&(desired_model.clone(), desired_effort.clone())) {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "switch": switch, "reason": "post_switch_status_mismatch" }),
            ));
        }
        if let Err(_message) = seams.project_tier(&desired_model, &desired_effort) {
            return Ok(override_receipt(
                refusal,
                json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "switch": switch, "reason": "registry_projection_failed" }),
            ));
        }
        switch_verified = true;
    }

    if !seams.send(target_command, true)? {
        return Ok(override_receipt(
            refusal,
            json!({ "cleared": true, "session_restamped": true, "registry_name": renamed, "switch": switch, "switch_verified": switch_verified, "reason": "target_submit_not_confirmed" }),
        ));
    }
    Ok(json!({
        "status": "retasked",
        "cleared": true,
        "session_restamped": true,
        "switch": switch,
        "switch_verified": true,
        "target_submit_confirmed": true,
        "registry_name": renamed,
        "source_session_id": predecessor,
        "current_session_id": new_session,
        "transition": "succession",
        "registry_rows": 1,
        "lineage_recorded": true,
    }))
}

fn settled_read<S: RetaskSeams>(seams: &mut S) -> Result<String, TransportFailure> {
    seams.settle()?;
    seams.read_frame()
}

pub mod transport;

#[cfg(test)]
mod tests;
