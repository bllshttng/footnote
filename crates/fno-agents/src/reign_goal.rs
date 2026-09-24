//! Typed provider goal transitions for a crowned Codex reign.

use crate::codex_thread::{CodexThread, GoalStatus, GoalUsage, NativeGoal};
use crate::king_termination::KingManifest;
use serde_json::{json, Value};
use std::path::Path;

const STOP_GOAL_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone)]
enum GoalAction {
    Ensure {
        scope: String,
        owner: String,
    },
    Pause {
        scope: String,
        owner: String,
    },
    Resume {
        scope: String,
        owner: String,
    },
    Provider {
        method: String,
        text: String,
        scope: String,
    },
}

/// Initialize or reuse the exact reign goal before the crown manifest is written.
pub(crate) fn ensure(session_id: &str, scope: &str, cwd: &Path) -> Result<Value, String> {
    let scope = scope.trim();
    let owner = format!("king:{scope}");
    run_action(
        session_id,
        cwd,
        GoalAction::Ensure {
            scope: scope.to_string(),
            owner,
        },
    )
}

/// Pause a matching active goal only after the king loop proved a quiet park.
pub(crate) fn pause_codex_reign_goal(manifest: &KingManifest, cwd: &Path) -> Result<Value, String> {
    let (session_id, scope, owner) = manifest_identity(manifest)?;
    run_action(&session_id, cwd, GoalAction::Pause { scope, owner })
}

/// Resume only the paused goal that belongs to this exact crown and session.
pub(crate) fn resume(manifest: &KingManifest, cwd: &Path) -> Result<Value, String> {
    let (session_id, scope, owner) = manifest_identity(manifest)?;
    run_action(&session_id, cwd, GoalAction::Resume { scope, owner })
}

pub(crate) fn pause_reign_goal_receipt(
    thread_id: &str,
    scope: &str,
    continuation_owner: &str,
    objective: &str,
    usage: &GoalUsage,
) -> Result<Value, String> {
    if thread_id.trim().is_empty()
        || scope.trim().is_empty()
        || continuation_owner.trim().is_empty()
        || objective.trim().is_empty()
    {
        return Err("provider goal receipt is missing identity".to_string());
    }
    Ok(json!({
        "provider": "codex",
        "thread_id": thread_id,
        "scope": scope,
        "objective": objective,
        "status": "paused",
        "continuation_owner": continuation_owner,
        "usage": usage.receipt_value(),
    }))
}

fn manifest_identity(manifest: &KingManifest) -> Result<(String, String, String), String> {
    if manifest.harness.as_deref() != Some("codex") {
        return Err("provider goal belongs to a non-Codex reign".to_string());
    }
    let session_id = manifest
        .harness_session_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "Codex provider goal has no session id".to_string())?;
    let scope = manifest.scope.trim();
    if scope.is_empty() {
        return Err("Codex provider goal has no scope".to_string());
    }
    Ok((
        session_id.to_string(),
        scope.to_string(),
        format!("king:{scope}"),
    ))
}

fn run_action(session_id: &str, cwd: &Path, action: GoalAction) -> Result<Value, String> {
    if session_id.trim().is_empty() {
        return Err("Codex provider action is missing an exact session id".to_string());
    }
    let session_id = session_id.to_string();
    let cwd = cwd.to_path_buf();
    run_on_provider_thread(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("Codex provider goal runtime unavailable: {error}"))?;
        runtime.block_on(async move {
            let mut thread = CodexThread::resume_for_control(cwd, &session_id)
                .await
                .map_err(|error| format!("Codex provider goal unreadable: {error}"))?;
            apply_action(&mut thread, &session_id, action).await
        })
    })?
}

fn run_on_provider_thread<T: Send + 'static>(
    action: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    std::thread::Builder::new()
        .name("codex-provider-goal".to_string())
        .spawn(action)
        .map_err(|error| format!("Codex provider goal thread unavailable: {error}"))?
        .join()
        .map_err(|_| "Codex provider goal thread panicked".to_string())
}

pub(crate) fn read_codex_goal_for_stop(session_id: &str) -> Result<Option<NativeGoal>, String> {
    if session_id.trim().is_empty() {
        return Err("Codex Stop goal read has no exact session id".to_string());
    }
    let session_id = session_id.to_string();
    run_on_provider_thread(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("Codex Stop goal runtime unavailable: {error}"))?;
        runtime
            .block_on(CodexThread::read_goal_for_stop(
                &session_id,
                STOP_GOAL_READ_TIMEOUT,
            ))
            .map_err(|error| error.to_string())
    })?
}

async fn apply_action(
    thread: &mut CodexThread,
    session_id: &str,
    action: GoalAction,
) -> Result<Value, String> {
    match action {
        GoalAction::Ensure { scope, owner } => {
            let goal = thread
                .ensure_reign_goal_typed(&scope, &owner)
                .await
                .map_err(|error| format!("Codex provider goal ensure refused: {error}"))?;
            let expected = crate::codex_thread::reign_objective(&scope);
            verify_goal(&goal, &expected, GoalStatus::Active, "ensure")?;
            goal_receipt(session_id, &scope, &owner, &goal)
        }
        GoalAction::Pause { scope, owner } => {
            let expected = crate::codex_thread::reign_objective(&scope);
            let current = thread
                .goal_get_typed()
                .await
                .map_err(|error| format!("Codex provider goal unreadable: {error}"))?;
            let Some(current) = current else {
                return Ok(json!({
                    "provider": "codex",
                    "thread_id": session_id,
                    "scope": scope,
                    "status": "absent",
                    "continuation_owner": owner,
                }));
            };
            if current.objective != expected {
                return Err(format!(
                    "Codex provider goal pause refused: objective does not match reign: {:?}",
                    current.objective
                ));
            }
            if current.status == GoalStatus::Paused {
                return pause_reign_goal_receipt(
                    session_id,
                    &scope,
                    &owner,
                    &current.objective,
                    &current.usage,
                );
            }
            verify_goal(&current, &expected, GoalStatus::Active, "pause")?;
            let paused = thread
                .goal_set_typed(&current.objective, GoalStatus::Paused)
                .await
                .map_err(|error| format!("Codex provider goal pause refused: {error}"))?;
            verify_goal(&paused, &expected, GoalStatus::Paused, "pause")?;
            verify_usage_preserved(&current, &paused, "pause")?;
            pause_reign_goal_receipt(session_id, &scope, &owner, &paused.objective, &paused.usage)
        }
        GoalAction::Resume { scope, owner } => {
            let expected = crate::codex_thread::reign_objective(&scope);
            let current = thread
                .goal_get_typed()
                .await
                .map_err(|error| format!("Codex provider goal unreadable: {error}"))?
                .ok_or_else(|| "Codex provider goal unreadable: no goal".to_string())?;
            verify_goal(&current, &expected, GoalStatus::Paused, "resume")?;
            let active = thread
                .goal_set_typed(&current.objective, GoalStatus::Active)
                .await
                .map_err(|error| format!("Codex provider goal resume refused: {error}"))?;
            verify_goal(&active, &expected, GoalStatus::Active, "resume")?;
            verify_usage_preserved(&current, &active, "resume")?;
            goal_receipt(session_id, &scope, &owner, &active)
        }
        GoalAction::Provider {
            method,
            text,
            scope,
        } => provider_action(thread, session_id, &method, &text, &scope).await,
    }
}

async fn provider_action(
    thread: &mut CodexThread,
    session_id: &str,
    method: &str,
    command: &str,
    scope: &str,
) -> Result<Value, String> {
    match method {
        "thread/compact/start" => {
            let receipt = thread
                .compact()
                .await
                .map_err(|error| format!("Codex compaction refused: {error}"))?;
            Ok(json!({
                "verified": true,
                "action": "compact",
                "provider": "codex",
                "thread_id": session_id,
                "status": "completed",
                "receipt": receipt,
            }))
        }
        "thread/goal/get" => {
            let goal = thread
                .goal_get_typed()
                .await
                .map_err(|error| format!("Codex provider goal unreadable: {error}"))?
                .ok_or_else(|| "Codex provider goal unreadable: no goal".to_string())?;
            provider_goal_get_receipt(session_id, &scope, &goal)
        }
        "thread/goal/set" => {
            let (objective, owner) = goal_set_contract(command, scope, session_id)?;
            if command.trim() == "/goal resume" {
                let current = thread
                    .goal_get_typed()
                    .await
                    .map_err(|error| format!("Codex provider goal unreadable: {error}"))?
                    .ok_or_else(|| "Codex goal resume refused: no paused reign goal".to_string())?;
                verify_goal(&current, &objective, GoalStatus::Paused, "resume")?;
                let active = thread
                    .goal_set_typed(&current.objective, GoalStatus::Active)
                    .await
                    .map_err(|error| format!("Codex provider goal resume refused: {error}"))?;
                verify_goal(&active, &objective, GoalStatus::Active, "resume")?;
                verify_usage_preserved(&current, &active, "resume")?;
                return Ok(json!({
                    "verified": true,
                    "action": "goal_set",
                    "provider": "codex",
                    "thread_id": session_id,
                    "status": "active",
                    "previous_status": "paused",
                    "objective": active.objective,
                    "continuation_owner": owner,
                    "usage": active.usage.receipt_value(),
                }));
            }
            let current = thread
                .goal_get_typed()
                .await
                .map_err(|error| format!("Codex provider goal unreadable: {error}"))?;
            let goal = match current {
                Some(current) if current.objective != objective => {
                    return Err(format!(
                        "refusing to replace Codex objective {:?} with {:?}",
                        current.objective, objective
                    ));
                }
                Some(current) if current.status == GoalStatus::Active => {
                    verify_goal(&current, &objective, GoalStatus::Active, "goal-set")?;
                    current
                }
                Some(current) => {
                    verify_goal(&current, &objective, GoalStatus::Paused, "goal-set")?;
                    let active = thread
                        .goal_set_typed(&objective, GoalStatus::Active)
                        .await
                        .map_err(|error| format!("Codex provider goal set refused: {error}"))?;
                    verify_usage_preserved(&current, &active, "goal-set")?;
                    active
                }
                None => thread
                    .goal_set_typed(&objective, GoalStatus::Active)
                    .await
                    .map_err(|error| format!("Codex provider goal set refused: {error}"))?,
            };
            verify_goal(&goal, &objective, GoalStatus::Active, "goal-set")?;
            Ok(json!({
                "verified": true,
                "action": "goal_set",
                "provider": "codex",
                "thread_id": session_id,
                "status": "active",
                "objective": goal.objective,
                "continuation_owner": owner,
                "usage": goal.usage.receipt_value(),
            }))
        }
        other => Err(format!("unsupported Codex provider action {other:?}")),
    }
}

fn goal_set_contract(
    command: &str,
    scope: &str,
    session_id: &str,
) -> Result<(String, String), String> {
    if command.trim() == "/goal resume" {
        let scope = scope.trim();
        if scope.is_empty() {
            return Err("Codex goal resume requires the exact crown scope".into());
        }
        return Ok((
            crate::codex_thread::reign_objective(scope),
            format!("king:{scope}"),
        ));
    }
    let objective = command
        .trim()
        .strip_prefix("/goal")
        .map(str::trim)
        .filter(|objective| !objective.is_empty())
        .ok_or_else(|| "Codex goal command needs a non-empty active objective".to_string())?;
    if matches!(
        objective.split_whitespace().next(),
        Some("clear" | "complete" | "completed")
    ) {
        return Err("Codex goal clear/complete is not exposed by the provider lane".into());
    }
    let owner = continuation_owner_for_goal(objective, scope, session_id)?;
    Ok((objective.to_string(), owner))
}

fn continuation_owner_for_goal(
    objective: &str,
    scope: &str,
    session_id: &str,
) -> Result<String, String> {
    // Codex persists thread id, objective, and status only; Footnote derives
    // its owner from the selected crown scope or exact target session.
    if !scope.trim().is_empty() {
        let expected = crate::codex_thread::reign_objective(scope);
        if objective != expected.as_str() {
            return Err(format!(
                "Codex crowned goal objective does not match scope {:?}",
                scope.trim()
            ));
        }
        return Ok(format!("king:{}", scope.trim()));
    }
    if objective.starts_with("$fno:reign ") {
        return Err("Codex reign goal owner requires the exact manifest scope".into());
    }
    if session_id.trim().is_empty() {
        return Err("Codex provider goal has no exact session id".into());
    }
    Ok(format!("target:{session_id}"))
}

pub(crate) fn run_provider_command(args: &[String]) -> i32 {
    let session_id = flag(args, "--session").unwrap_or_default();
    let cwd = flag(args, "--cwd").unwrap_or_else(|| ".".to_string());
    let method = flag(args, "--method").unwrap_or_default();
    let text = flag(args, "--text").unwrap_or_default();
    let scope = flag(args, "--scope").unwrap_or_default();
    if session_id.trim().is_empty() || method.trim().is_empty() {
        eprintln!("fno-agents loop command: --session and --method are required");
        return 2;
    }
    match run_action(
        &session_id,
        Path::new(&cwd),
        GoalAction::Provider {
            method,
            text,
            scope,
        },
    ) {
        Ok(receipt) => {
            println!("{receipt}");
            0
        }
        Err(error) => {
            eprintln!("fno-agents loop command: {error}");
            1
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn verify_goal(
    goal: &NativeGoal,
    expected_objective: &str,
    expected_status: GoalStatus,
    action: &str,
) -> Result<(), String> {
    if goal.objective != expected_objective {
        return Err(format!(
            "Codex provider goal {action} refused: objective does not match reign: {:?}",
            goal.objective
        ));
    }
    if goal.status != expected_status {
        return Err(format!(
            "Codex provider goal {action} refused: expected {expected_status:?}, got {:?}",
            goal.status
        ));
    }
    Ok(())
}

fn goal_receipt(
    session_id: &str,
    scope: &str,
    owner: &str,
    goal: &NativeGoal,
) -> Result<Value, String> {
    let expected_owner = continuation_owner_for_goal(&goal.objective, scope, session_id)?;
    if session_id.trim().is_empty()
        || scope.trim().is_empty()
        || owner.trim().is_empty()
        || goal.thread_id != session_id
        || goal.objective.trim().is_empty()
        || expected_owner.as_str() != owner
    {
        return Err("provider goal receipt is missing identity".to_string());
    }
    let status = match goal.status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usageLimited",
        GoalStatus::BudgetLimited => "budgetLimited",
        GoalStatus::Completed => "complete",
    };
    Ok(json!({
        "provider": "codex",
        "thread_id": session_id,
        "scope": scope,
        "objective": goal.objective,
        "status": status,
        "continuation_owner": owner,
        "usage": goal.usage.receipt_value(),
    }))
}

fn provider_goal_get_receipt(
    session_id: &str,
    scope: &str,
    goal: &NativeGoal,
) -> Result<Value, String> {
    let owner = continuation_owner_for_goal(&goal.objective, scope, session_id)?;
    let mut receipt = goal_receipt(session_id, scope, &owner, goal)?;
    receipt["verified"] = json!(true);
    receipt["action"] = json!("goal_get");
    Ok(receipt)
}

fn verify_usage_preserved(
    before: &NativeGoal,
    after: &NativeGoal,
    action: &str,
) -> Result<(), String> {
    if after.usage.preserves(&before.usage) {
        Ok(())
    } else {
        Err(format!(
            "Codex provider goal {action} refused: token budget or usage regressed"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_runtime_runs_on_a_fresh_thread_inside_an_ambient_runtime() {
        let ambient = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("ambient runtime builds");
        let result = ambient.block_on(async {
            run_on_provider_thread(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("provider runtime builds")
                    .block_on(async { 17 })
            })
        });
        assert_eq!(result, Ok(17));
    }

    #[test]
    fn provider_goal_receipt_keeps_exact_scope_and_owner() {
        let goal = NativeGoal {
            thread_id: "thread-1".to_string(),
            objective: "$fno:reign x-aaaa".to_string(),
            status: GoalStatus::Paused,
            usage: GoalUsage {
                token_budget: Some(20_000),
                tokens_used: 120,
                time_used_seconds: 5,
            },
        };
        let receipt = provider_goal_get_receipt("thread-1", "x-aaaa", &goal).unwrap();
        assert_eq!(receipt["verified"], true);
        assert_eq!(receipt["action"], "goal_get");
        assert_eq!(receipt["provider"], "codex");
        assert_eq!(receipt["thread_id"], "thread-1");
        assert_eq!(receipt["scope"], "x-aaaa");
        assert_eq!(receipt["objective"], "$fno:reign x-aaaa");
        assert_eq!(receipt["status"], "paused");
        assert_eq!(receipt["continuation_owner"], "king:x-aaaa");
        assert_eq!(
            receipt["usage"],
            json!({
                "token_budget": 20_000,
                "tokens_used": 120,
                "time_used_seconds": 5,
            })
        );
        assert_eq!(receipt["usage"]["token_budget"], 20_000);
        assert_eq!(receipt["usage"]["tokens_used"], 120);
        assert_eq!(receipt["usage"]["time_used_seconds"], 5);
    }

    #[test]
    fn goal_receipt_refuses_a_different_continuation_owner() {
        let goal = NativeGoal {
            thread_id: "thread-1".to_string(),
            objective: "$fno:reign x-aaaa".to_string(),
            status: GoalStatus::Active,
            usage: GoalUsage::default(),
        };
        assert!(goal_receipt("thread-1", "x-aaaa", "king:x-bbbb", &goal).is_err());
    }

    #[test]
    fn goal_set_contract_does_not_offer_clear_and_keeps_reign_scope_pinned() {
        assert!(goal_set_contract("/goal clear", "x-aaaa", "thread-1").is_err());
        assert!(goal_set_contract("/goal $fno:reign x-bbbb", "x-aaaa", "thread-1").is_err());
        assert!(goal_set_contract("/goal $fno:reign x-aaaa", "", "thread-1").is_err());
        assert_eq!(
            goal_set_contract("/goal resume", "scope-a", "thread-1").unwrap(),
            ("$fno:reign scope-a".to_string(), "king:scope-a".to_string())
        );
        assert!(goal_set_contract("/goal resume", "", "thread-1").is_err());
        assert_eq!(
            goal_set_contract("/goal keep working", "", "thread-1").unwrap(),
            ("keep working".to_string(), "target:thread-1".to_string())
        );
    }

    #[test]
    fn resume_requires_the_exact_paused_reign_goal() {
        let goal = NativeGoal {
            thread_id: "thread-1".to_string(),
            objective: "$fno:reign x-aaaa".to_string(),
            status: GoalStatus::Paused,
            usage: GoalUsage::default(),
        };
        assert!(verify_goal(&goal, "$fno:reign x-aaaa", GoalStatus::Paused, "resume").is_ok());
        assert!(verify_goal(&goal, "$fno:reign x-bbbb", GoalStatus::Paused, "resume").is_err());
    }

    #[test]
    fn readable_codex_reign_goal_pauses_without_replacing_its_receipt() {
        let receipt = pause_reign_goal_receipt(
            "thread-1",
            "x-aaaa",
            "king:x-aaaa",
            "$fno:reign x-aaaa",
            &GoalUsage {
                token_budget: Some(20_000),
                tokens_used: 120,
                time_used_seconds: 5,
            },
        )
        .expect("the matching active goal can be parked");
        assert_eq!(receipt["provider"], "codex");
        assert_eq!(receipt["thread_id"], "thread-1");
        assert_eq!(receipt["objective"], "$fno:reign x-aaaa");
        assert_eq!(receipt["status"], "paused");
        assert_eq!(receipt["continuation_owner"], "king:x-aaaa");
        assert_eq!(receipt["scope"], "x-aaaa");
    }
}
