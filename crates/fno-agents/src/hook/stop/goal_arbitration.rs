//! Goal truth, continuation ownership and correlated Stop event emission.

use super::{events_path, first_raw_field, Fire};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GoalTruth {
    pub(super) objective: String,
    pub(super) status: String,
    pub(super) continuation_owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GoalArbitration {
    None,
    Delegated,
    Refusal(String),
}

#[derive(Debug, Serialize)]
pub(super) struct StopDecisionEvent {
    pub(super) session_id: String,
    pub(super) raw_identity_candidates: Vec<String>,
    pub(super) turn_id: String,
    pub(super) manifest: String,
    pub(super) scope: String,
    pub(super) node_id: String,
    pub(super) driver: String,
    pub(super) continuation_owner: String,
    pub(super) decision: String,
    pub(super) class: String,
    pub(super) correlation_id: String,
    pub(super) harness_output_contract: String,
}

pub(super) fn goal_payload(value: &Value) -> Option<&Value> {
    value
        .get("goal")
        .or_else(|| value.get("native_goal"))
        .or_else(|| value.get("provider_goal"))
        .or_else(|| {
            (value.get("goal_status").is_some()
                || value.get("goal_objective").is_some()
                || value.get("continuation_owner").is_some())
            .then_some(value)
        })
}

fn parse_goal_value(value: &Value) -> Result<Option<GoalTruth>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or("goal truth is unreadable: expected an object")?;
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let objective = text(&["objective", "goal"]);
    let mut status = text(&["status"]);
    if status.is_empty() {
        status = "active".into();
    }
    if !matches!(status.as_str(), "active" | "paused" | "completed" | "done") {
        return Err(format!("goal truth has unknown status {status:?}"));
    }
    if object.get("verified").and_then(Value::as_bool) == Some(false) {
        return Err("goal truth is not verified".into());
    }
    if objective.is_empty() {
        return Err("goal truth is missing objective".into());
    }
    Ok(Some(GoalTruth {
        objective,
        status: if status == "done" {
            "completed".into()
        } else {
            status
        },
        continuation_owner: text(&["continuationOwner", "continuation_owner", "owner"]),
    }))
}

fn manifest_goal(content: &str) -> Result<Option<GoalTruth>, String> {
    let objective = first_raw_field(
        content,
        &["goal_objective", "native_goal_objective", "goal"],
    );
    let status = first_raw_field(content, &["goal_status", "native_goal_status"]);
    let owner = first_raw_field(content, &["goal_continuation_owner", "goal_owner"]);
    let verified = first_raw_field(content, &["goal_verified", "native_goal_verified"]);
    if objective.is_none() && status.is_none() && owner.is_none() && verified.is_none() {
        return Ok(None);
    }
    if verified.as_deref() == Some("false") {
        return Err("manifest goal truth is not verified".into());
    }
    Ok(Some(GoalTruth {
        objective: objective
            .filter(|v| !v.is_empty())
            .ok_or("manifest goal truth is missing objective")?,
        status: status.unwrap_or_else(|| "active".into()),
        continuation_owner: owner.unwrap_or_default(),
    }))
}

fn merge_goal_truth(payload: Option<&Value>, manifest: &str) -> Result<Option<GoalTruth>, String> {
    let from_payload = payload.map(parse_goal_value).transpose()?.flatten();
    let from_manifest = manifest_goal(manifest)?;
    match (from_payload, from_manifest) {
        (Some(left), Some(right)) if left != right => Err(format!(
            "conflicting goal truth: payload {:?}, manifest {:?}",
            left, right
        )),
        (Some(goal), _) | (_, Some(goal)) => Ok(Some(goal)),
        (None, None) => Ok(None),
    }
}

pub(super) fn arbitrate_continuation(driver: &str, fire: &Fire, manifest: &str) -> GoalArbitration {
    let goal = match merge_goal_truth(fire.goal_payload.as_ref(), manifest) {
        Ok(goal) => goal,
        Err(reason) => return GoalArbitration::Refusal(reason),
    };
    arbitrate_goal_truth(driver, manifest, goal)
}

pub(super) fn arbitrate_codex_continuation(
    driver: &str,
    fire: &Fire,
    manifest: &str,
) -> GoalArbitration {
    if driver != "king" {
        return GoalArbitration::None;
    }
    let live = crate::reign_goal::read_codex_goal_for_stop(&fire.session_id);
    arbitrate_codex_continuation_from_reading(driver, fire, manifest, live)
}

pub(super) fn arbitrate_codex_continuation_from_reading(
    driver: &str,
    fire: &Fire,
    manifest: &str,
    live: Result<Option<crate::codex_thread::NativeGoal>, String>,
) -> GoalArbitration {
    if driver != "king" {
        return GoalArbitration::None;
    }
    let Some(expected_session) = first_raw_field(manifest, &["harness_session_id"]) else {
        return GoalArbitration::Refusal(
            "Codex Stop manifest has no exact harness session id".into(),
        );
    };
    if expected_session != fire.session_id {
        return GoalArbitration::Refusal(format!(
            "Codex Stop session mismatch: expected {expected_session:?}, got {:?}",
            fire.session_id
        ));
    }
    let live = match live {
        Ok(Some(goal)) => goal,
        Ok(None) => return GoalArbitration::None,
        Err(reason) => {
            return GoalArbitration::Refusal(format!("Codex provider goal unreadable: {reason}"))
        }
    };
    if live.thread_id != fire.session_id {
        return GoalArbitration::Refusal(format!(
            "Codex provider goal thread mismatch: expected {:?}, got {:?}",
            fire.session_id, live.thread_id
        ));
    }
    let Some(owner) = expected_continuation_owner(driver, manifest) else {
        return GoalArbitration::Refusal(
            "active Codex goal owner is not defined by the manifest".into(),
        );
    };
    let Some(scope) = first_raw_field(manifest, &["scope", "crown_scope"]) else {
        return GoalArbitration::Refusal("active Codex goal has no crown scope".into());
    };
    let expected_owner = format!("king:{}", scope.trim());
    if owner != expected_owner {
        return GoalArbitration::Refusal(format!(
            "active Codex goal owner must derive from crown scope: expected {expected_owner:?}, got {owner:?}"
        ));
    }
    let expected_objective = crate::codex_thread::reign_objective(&scope);
    if live.objective != expected_objective {
        return GoalArbitration::Refusal(format!(
            "conflicting goal truth: expected objective {expected_objective:?}, got {:?}",
            live.objective
        ));
    }
    let status = match live.status {
        crate::codex_thread::GoalStatus::Active => "active",
        crate::codex_thread::GoalStatus::Paused => "paused",
        crate::codex_thread::GoalStatus::Blocked => "blocked",
        crate::codex_thread::GoalStatus::UsageLimited => "usageLimited",
        crate::codex_thread::GoalStatus::BudgetLimited => "budgetLimited",
        crate::codex_thread::GoalStatus::Completed => "completed",
    };
    arbitrate_goal_truth(
        driver,
        manifest,
        Some(GoalTruth {
            objective: live.objective,
            status: status.to_string(),
            continuation_owner: owner,
        }),
    )
}

fn arbitrate_goal_truth(driver: &str, manifest: &str, goal: Option<GoalTruth>) -> GoalArbitration {
    let Some(goal) = goal else {
        return GoalArbitration::None;
    };
    if goal.status != "active" {
        return GoalArbitration::None;
    }
    if goal.continuation_owner.is_empty() {
        return GoalArbitration::Refusal("active goal truth is missing continuation owner".into());
    }
    let expected_owner = expected_continuation_owner(driver, manifest);
    let Some(expected_owner) = expected_owner else {
        return GoalArbitration::Refusal(
            "active goal truth cannot be verified: manifest scope/node is missing".into(),
        );
    };
    if goal.continuation_owner != expected_owner {
        return GoalArbitration::Refusal(format!(
            "conflicting goal truth: expected continuation owner {expected_owner:?}, got {:?}",
            goal.continuation_owner
        ));
    }
    if driver == "king" {
        let Some(scope) = first_raw_field(manifest, &["scope", "crown_scope"]) else {
            return GoalArbitration::Refusal(
                "active goal truth cannot be verified: manifest scope is missing".into(),
            );
        };
        let expected = crate::codex_thread::reign_objective(&scope);
        if goal.objective != expected {
            return GoalArbitration::Refusal(format!(
                "conflicting goal truth: expected objective {expected:?}, got {:?}",
                goal.objective
            ));
        }
    }
    GoalArbitration::Delegated
}

fn expected_continuation_owner(driver: &str, manifest: &str) -> Option<String> {
    if let Some(owner) = first_raw_field(manifest, &["continuation_owner"]) {
        return Some(owner);
    }
    let scope = first_raw_field(manifest, &["scope", "crown_scope"]).unwrap_or_default();
    let node_id = first_raw_field(manifest, &["node_id", "fno_id"]).unwrap_or_default();
    match driver {
        "king" if !scope.is_empty() => Some(format!("king:{scope}")),
        "target" if !node_id.is_empty() => Some(format!("target:{node_id}")),
        _ => None,
    }
}

pub(super) fn emit_stop_decision(
    cwd: &Path,
    fire: &Fire,
    state: Option<&Path>,
    driver: &str,
    continuation_owner: &str,
    decision: &str,
    class: &str,
    manifest: &str,
) {
    let session_id = if fire.session_id.is_empty() {
        fire.hook_harness_id.clone()
    } else {
        fire.session_id.clone()
    };
    let turn = if fire.turn_id.is_empty() {
        "stop".to_string()
    } else {
        fire.turn_id.clone()
    };
    let correlation_id = format!("stop:{session_id}:{turn}");
    let event = StopDecisionEvent {
        session_id,
        raw_identity_candidates: fire.resolve_ids.clone(),
        turn_id: fire.turn_id.clone(),
        manifest: state
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        scope: first_raw_field(manifest, &["scope", "crown_scope"]).unwrap_or_default(),
        node_id: first_raw_field(manifest, &["node_id", "fno_id"]).unwrap_or_default(),
        driver: driver.to_string(),
        continuation_owner: continuation_owner.into(),
        decision: decision.into(),
        class: class.into(),
        correlation_id,
        harness_output_contract: harness_output_contract(fire, decision).into(),
    };
    let project_events = events_path(cwd);
    let global_events = std::env::var_os("GLOBAL_EVENTS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fno/events.jsonl")))
        .unwrap_or_else(|| project_events.clone());
    crate::loopcheck::emit_to_both(
        &project_events,
        &global_events,
        "stop_decision",
        serde_json::to_value(event).expect("Stop decision event serializes"),
    );
}

fn harness_output_contract(fire: &Fire, decision: &str) -> &'static str {
    if decision == "allow" {
        return "empty";
    }
    let claude = fire.harness.as_deref() == Some("claude")
        || std::env::var("CLAUDECODE").as_deref() == Ok("1")
        || std::env::var_os("CLAUDE_PLUGIN_ROOT").is_some_and(|v| !v.is_empty());
    let foreign = [
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "GEMINI_SESSION_ID",
        "OPENCODE_SESSION_ID",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()));
    if claude && !foreign {
        "json_block"
    } else {
        "exit_2_stderr"
    }
}
