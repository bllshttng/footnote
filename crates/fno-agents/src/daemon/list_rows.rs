//! The `agent.list` entry point and its attention ordering, extracted from
//! the parent under the file-budget gate: the list projection's change
//! (the `substrate` key) moved with the code it touched.

use super::*;

pub(super) fn handle_list(ctx: &Ctx, req: &Request) -> Response {
    handle_list_with_truth(ctx, req, crate::truth_probe::family1_truth_probe_many)
}

/// One row's list-lane attention key: evidence tier, then longest-silent
/// first, then name so consecutive lists never shuffle equal rows. Only
/// fields that carry their evidence with them (`basis`,
/// `last_activity_age_s`) - never `status`, never a bare verdict. A row with
/// no probe answer (all three null) lands in the neutral tier with age 0:
/// absence of a reading is not urgency.
/// `to_bits` is order-preserving for non-negative f64 (and an age is a
/// duration, always non-negative), which is what lets a float age ride an
/// `Ord` tuple key.
pub(super) fn attention_sort_key(row: &Value) -> (u8, std::cmp::Reverse<u64>, String) {
    let basis = row.get("basis").and_then(|v| v.as_str());
    let age = row
        .get("last_activity_age_s")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let tier = if matches!(basis, Some("process-gone") | Some("pane-gone"))
        || row.get("reachability").and_then(|v| v.as_str()) == Some("unreachable")
    {
        5
    } else if basis == Some("transcript") && age >= STALE_ATTENTION_S {
        0
    } else if basis == Some("silent") {
        1
    } else if basis == Some("no-evidence") {
        2
    } else {
        4
    };
    (
        tier,
        std::cmp::Reverse(age.to_bits()),
        row.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

/// The STATUS word `list` renders: SERVED ACTIVITY, never a `live` token
/// (x-c672, AC7). Nothing decides on this word anymore - retirement reads the
/// reverse join, the lanes read their own probes - so the column answers the
/// operator's actual question, what is this session doing: `writing` (the
/// transcript moved inside `STALE_ATTENTION_S`), `quiet` (older), `parked`
/// (the tail closed a promise). A positively falsified row reads `orphaned`,
/// and a probe that did not answer reads `unknown`. A confirmed-live pid does
/// NOT lift an unanswered age to `quiet`: the word is activity, and a process
/// being up says nothing about when it last wrote - the same row must render
/// the same word through the Python list lane, which has no pid census.
pub(super) fn rendered_status_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
) -> &'static str {
    if probe.and_then(|p| p.reachability.as_deref()) == Some("unreachable") {
        return "orphaned";
    }
    match probe.map(|p| p.state.as_str()) {
        Some("done") => "parked",
        Some(_) => match probe.and_then(|p| p.last_activity_age_s) {
            Some(age) if age < STALE_ATTENTION_S => "writing",
            Some(_) => "quiet",
            None => "unknown",
        },
        None => "unknown",
    }
}

/// True for a Claude model id or tier alias. Mirrors
/// `fno.agents.model_routing.is_anthropic_model` (cli/src/fno/agents/model_routing.py) --
/// duplicated rather than shelled out to because the daemon already pays one
/// probe per row and a second process spawn per row would multiply that cost
/// for a four-branch string check.
fn is_anthropic_model(model: &str) -> bool {
    let name = model.trim().to_ascii_lowercase();
    name.starts_with("claude-") || matches!(name.as_str(), "opus" | "sonnet" | "haiku" | "fable")
}

/// Structural refusal predicate (Locked Decision 3). Mirrors
/// `fno.agents.reachability._is_refused` -- never reads the transcript's
/// prose, so a reworded refusal message cannot break it. Fails OPEN: a
/// recorded `route_settings_path` records the INTENDED route, so a
/// foreign-routed worker answering as a foreign model is healthy, not refused.
fn is_refused(observed_model: &Value, harness: &str, route_settings_path: Option<&str>) -> bool {
    if harness != "claude" {
        return false;
    }
    if route_settings_path.is_some() {
        return false;
    }
    if observed_model.get("kind").and_then(Value::as_str) != Some("observed") {
        return false;
    }
    match observed_model.get("model").and_then(Value::as_str) {
        Some(model) => !is_anthropic_model(model),
        None => false,
    }
}

/// Map a truth probe onto the progress axis `list` renders, mirroring Python's
/// `classify_progress` (`fno/agents/reachability.py`). Reads the SAME probe
/// `rendered_status_from_truth` reads, plus `harness` and
/// `route_settings_path` off the registry entry -- no second probe is paid.
///
/// Precedence matches the Python classifier exactly: a falsified/unresolved
/// row first (`reachability` absent or `unreachable` -- AC12-FR, the
/// compatibility-fallback case included, since an unmeasured row has no
/// progress state to report either), then the refusal predicate, then the
/// truth-state arms plus the measured transcript age. A written `working`
/// state is not progress evidence when its transcript stopped advancing.
pub(crate) fn progress_from_truth(
    probe: Option<&crate::truth_probe::TruthProbe>,
    harness: &str,
    route_settings_path: Option<&str>,
) -> (&'static str, &'static str) {
    match probe.and_then(|p| p.reachability.as_deref()) {
        Some("unreachable") | None => return ("unknown", "no-evidence"),
        _ => {}
    }
    let observed_model = probe.map(|p| &p.observed_model);
    if observed_model.is_some_and(|om| is_refused(om, harness, route_settings_path)) {
        return ("refused", "model-refused");
    }
    match probe.map(|p| p.state.as_str()) {
        Some("working" | "watching") => match probe.and_then(|p| p.last_activity_age_s) {
            None => ("unknown", "no-evidence"),
            Some(age) if age >= STALE_ATTENTION_S => ("unknown", "silent"),
            Some(_) => ("advancing", "transcript-turn"),
        },
        Some("your-move") => ("awaiting-operator", "operator-turn"),
        Some("done") => ("parked", "promise"),
        Some("stalled") => ("unknown", "silent"),
        _ => ("unknown", "no-evidence"),
    }
}

pub(crate) fn registry_truth_handle(entry: &RegistryEntry) -> String {
    if let Some(session_id) = entry.harness_session_id.as_deref() {
        return session_id.to_string();
    }
    if !entry.short_id.is_empty() {
        entry.short_id.clone()
    } else {
        entry.name.clone()
    }
}
