//! One effective-readiness snapshot for target and crown admission.
//!
//! The four legs are deliberately separate. A healthy machine does not prove
//! that the lifecycle invokes the loop, and a lifecycle hook does not prove
//! that the provider can carry the native goal. Callers take one snapshot and
//! refuse on an unreadable leg instead of re-reading each concern independently.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegState {
    Ready,
    Blocked,
    Unreadable,
}

impl LegState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessLeg {
    pub state: LegState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ReadinessLeg {
    pub fn ready() -> Self {
        Self {
            state: LegState::Ready,
            reason: None,
        }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        Self {
            state: LegState::Blocked,
            reason: Some(reason.into()),
        }
    }

    pub fn unreadable(reason: impl Into<String>) -> Self {
        Self {
            state: LegState::Unreadable,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveLoopReadiness {
    pub machine: ReadinessLeg,
    pub lifecycle: ReadinessLeg,
    pub stop: ReadinessLeg,
    pub provider_goal: ReadinessLeg,
    pub continuation_owner: String,
}

pub type LoopReadiness = EffectiveLoopReadiness;

impl EffectiveLoopReadiness {
    pub fn ready(&self) -> bool {
        !self.continuation_owner.trim().is_empty()
            && self.machine.state.is_ready()
            && self.lifecycle.state.is_ready()
            && self.stop.state.is_ready()
            && self.provider_goal.state.is_ready()
    }

    pub fn first_refusal(&self) -> Option<String> {
        if self.continuation_owner.trim().is_empty() {
            return Some("continuation owner is unreadable".to_string());
        }
        [
            ("machine", &self.machine),
            ("lifecycle", &self.lifecycle),
            ("Stop", &self.stop),
            ("provider-goal", &self.provider_goal),
        ]
        .into_iter()
        .find(|(_, leg)| !leg.state.is_ready())
        .map(|(name, leg)| {
            format!(
                "{name} readiness is {:?}: {}",
                leg.state,
                leg.reason.as_deref().unwrap_or("no reason was readable")
            )
        })
    }

    /// JSON consumed by the Python admission seam. The four legs remain
    /// objects so an unreadable reason cannot be confused with a negative
    /// boolean.
    pub fn to_json(&self) -> Value {
        json!({
            "ready": self.ready(),
            "legs": {
                "machine": leg_json(&self.machine),
                "lifecycle": leg_json(&self.lifecycle),
                "stop": leg_json(&self.stop),
                "provider_goal": leg_json(&self.provider_goal),
            },
            "continuation_owner": self.continuation_owner,
            "refusal": self.first_refusal(),
        })
    }
}

pub fn measure(
    machine: ReadinessLeg,
    lifecycle: ReadinessLeg,
    stop: ReadinessLeg,
    provider_goal: ReadinessLeg,
    continuation_owner: impl Into<String>,
) -> EffectiveLoopReadiness {
    EffectiveLoopReadiness {
        machine,
        lifecycle,
        stop,
        provider_goal,
        continuation_owner: continuation_owner.into(),
    }
}

pub fn admit(readiness: EffectiveLoopReadiness) -> Result<EffectiveLoopReadiness, String> {
    match readiness.first_refusal() {
        Some(reason) => Err(reason),
        None => Ok(readiness),
    }
}

fn leg_json(leg: &ReadinessLeg) -> Value {
    json!({
        "state": leg.state,
        "reason": leg.reason,
    })
}

const READINESS_EVENT_TYPES: &[&str] = &["context_snapshot", "stop_decision"];
const CODEX_PLUGIN_ID: &str = "fno@footnote";
const CODEX_PLUGIN_REPAIR: &str = "fno config plugin install codex";
const CODEX_PLUGIN_LIST_TIMEOUT: Duration = Duration::from_secs(5);

fn codex_machine_leg_from_plugin_list(raw: &str) -> ReadinessLeg {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => return ReadinessLeg::unreadable(format!("codex plugin list JSON: {error}")),
    };
    let Some(installed) = value.get("installed").and_then(Value::as_array) else {
        return ReadinessLeg::unreadable("codex plugin list has no installed array");
    };
    let plugin = installed
        .iter()
        .find(|row| row.get("pluginId").and_then(Value::as_str) == Some(CODEX_PLUGIN_ID));
    let Some(plugin) = plugin else {
        return ReadinessLeg::blocked(format!(
            "plugin-missing: {CODEX_PLUGIN_ID} is not enabled; run `{CODEX_PLUGIN_REPAIR}`"
        ));
    };
    let (Some(is_installed), Some(enabled)) = (
        plugin.get("installed").and_then(Value::as_bool),
        plugin.get("enabled").and_then(Value::as_bool),
    ) else {
        return ReadinessLeg::unreadable(format!(
            "codex plugin list has unreadable installed/enabled state for {CODEX_PLUGIN_ID}"
        ));
    };
    if is_installed && enabled {
        ReadinessLeg::ready()
    } else {
        ReadinessLeg::blocked(format!(
            "plugin-missing: {CODEX_PLUGIN_ID} is not enabled; run `{CODEX_PLUGIN_REPAIR}`"
        ))
    }
}

fn codex_machine_leg() -> ReadinessLeg {
    let Some(binary) = crate::codex_daemon_readiness::codex_cli_path() else {
        return ReadinessLeg::unreadable("codex CLI is not available for plugin readiness");
    };
    let mut child = match Command::new(binary)
        .args(["plugin", "list", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ReadinessLeg::unreadable(format!("codex plugin list failed: {error}"))
        }
    };
    let deadline = Instant::now() + CODEX_PLUGIN_LIST_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return ReadinessLeg::unreadable("codex plugin list timed out after 5s");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return ReadinessLeg::unreadable(format!("codex plugin list wait failed: {error}"));
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            return ReadinessLeg::unreadable(format!(
                "codex plugin list output unreadable: {error}"
            ))
        }
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return ReadinessLeg::unreadable(format!(
            "codex plugin list exited {}: {}",
            output.status,
            detail.trim()
        ));
    }
    codex_machine_leg_from_plugin_list(&String::from_utf8_lossy(&output.stdout))
}

fn machine_leg(harness: &str) -> ReadinessLeg {
    if harness == "codex" {
        codex_machine_leg()
    } else {
        ReadinessLeg::ready()
    }
}

fn stop_leg_from_events(events: &str, harness: &str, session: &str) -> ReadinessLeg {
    if session.trim().is_empty() {
        return ReadinessLeg::unreadable("exact harness session id is unavailable");
    }
    let mut latest_snapshot: Option<(usize, Value)> = None;
    let mut rows = Vec::new();
    for line in events.lines().filter(|line| !line.trim().is_empty()) {
        let event: Value = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(error) => {
                return ReadinessLeg::unreadable(format!("readiness event JSON: {error}"))
            }
        };
        if !event.is_object() {
            return ReadinessLeg::unreadable("readiness event row is not an object");
        }
        let index = rows.len();
        if event.get("type").and_then(Value::as_str) == Some("context_snapshot")
            && event.pointer("/data/harness").and_then(Value::as_str) == Some(harness)
            && event.pointer("/data/session_id").and_then(Value::as_str) == Some(session)
        {
            latest_snapshot = Some((index, event.clone()));
        }
        rows.push(event);
    }
    let Some((snapshot_index, snapshot)) = latest_snapshot else {
        return ReadinessLeg::blocked(format!(
            "session-hook-unobserved: no context snapshot for exact session {session}"
        ));
    };
    let data = snapshot.get("data").unwrap_or(&Value::Null);
    let entry_state = data
        .get("entry_state")
        .and_then(Value::as_str)
        .unwrap_or("");
    let complete = data.get("measurement_complete").and_then(Value::as_bool) == Some(true);
    if !complete || !matches!(entry_state, "startup" | "resume" | "clear" | "post_compact") {
        return ReadinessLeg::blocked(format!(
            "session-hook-unobserved: newest context snapshot for {session} is incomplete or has no lifecycle marker"
        ));
    }
    let correlated_stop = rows.iter().skip(snapshot_index + 1).any(|event| {
        event.get("type").and_then(Value::as_str) == Some("stop_decision")
            && event.pointer("/data/session_id").and_then(Value::as_str) == Some(session)
            && event
                .pointer("/data/correlation_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with(&format!("stop:{session}:")))
    });
    if correlated_stop {
        ReadinessLeg::ready()
    } else {
        ReadinessLeg::blocked(format!(
            "session-refresh-unverified: no correlated Stop fire after the newest context snapshot for {session}"
        ))
    }
}

fn stop_leg(harness: &str, session: &str, cwd: &Path) -> ReadinessLeg {
    if session.trim().is_empty() {
        return ReadinessLeg::unreadable("exact harness session id is unavailable");
    }
    let path = crate::paths::events_path(cwd);
    let query = crate::event_store::EventQuery::of_types(READINESS_EVENT_TYPES);
    let events = match crate::event_store::journal_text_checked(&path, &query) {
        Ok(events) => events,
        Err(error) => {
            return ReadinessLeg::unreadable(format!("session hook journal unreadable: {error}"))
        }
    };
    stop_leg_from_events(&events, harness, session)
}

/// Direct client verb used by the Python admission seams. All four legs are
/// assembled before the result is rendered, so a caller pays one native read
/// and gets one refusal that names the failed leg.
pub fn run(args: &[String]) -> i32 {
    let harness = flag(args, "--harness")
        .or_else(|| std::env::var("FNO_HARNESS").ok())
        .unwrap_or_else(|| "claude".to_string());
    let scope = flag(args, "--scope").unwrap_or_default();
    let command = flag(args, "--command").unwrap_or_else(|| {
        if scope.trim().is_empty() {
            String::new()
        } else {
            format!("/fno:reign {}", scope.trim())
        }
    });
    // The spawn door runs before the worker's session exists, and the
    // caller's env names the spawner, never the worker. So it skips the
    // session-bound legs; the worker's own Stop hook checks them after launch.
    let pre_launch = args.iter().any(|arg| arg == "--pre-launch");
    let session = if pre_launch {
        String::new()
    } else {
        flag(args, "--session")
            .filter(|session| !session.trim().is_empty())
            .or_else(|| std::env::var("FNO_HARNESS_SESSION_ID").ok())
            .or_else(|| match harness.as_str() {
                "codex" => std::env::var("CODEX_THREAD_ID").ok(),
                "claude" => std::env::var("CLAUDE_SESSION_ID").ok(),
                "gemini" => std::env::var("GEMINI_SESSION_ID").ok(),
                _ => None,
            })
            .unwrap_or_default()
    };
    let owner = flag(args, "--continuation-owner")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if pre_launch {
                "target:pre-launch".to_string()
            } else {
                continuation_owner(&scope, &session)
            }
        });

    let ensure_goal = args.iter().any(|arg| arg == "--ensure-goal");
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let machine = machine_leg(&harness);
    let stop = if pre_launch {
        ReadinessLeg {
            state: LegState::Ready,
            reason: Some("pre-launch: the worker's Stop hook checks this leg".to_string()),
        }
    } else {
        stop_leg(&harness, &session, &cwd)
    };
    let lifecycle = lifecycle_leg(&harness, &command);
    let provider_goal = provider_goal_leg(&harness);
    let mut snapshot = measure(machine, lifecycle, stop, provider_goal, owner);
    let mut goal_receipt = None;
    if ensure_goal && harness == "codex" && snapshot.ready() {
        if scope.trim().is_empty() || session.trim().is_empty() {
            snapshot.provider_goal = ReadinessLeg::unreadable(
                "Codex reign goal ensure requires exact session and scope",
            );
        } else {
            match crate::reign_goal::ensure(&session, &scope, &cwd) {
                Ok(receipt) => goal_receipt = Some(receipt),
                Err(error) => snapshot.provider_goal = ReadinessLeg::unreadable(error),
            }
        }
    }
    if let Some(refusal) = snapshot.first_refusal() {
        eprintln!("{refusal}");
    }
    let mut output = snapshot.to_json();
    if let (Some(object), Some(receipt)) = (output.as_object_mut(), goal_receipt) {
        object.insert("goal_receipt".to_string(), receipt);
    }
    println!("{output}");
    i32::from(!snapshot.ready())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn continuation_owner(scope: &str, session: &str) -> String {
    if !scope.trim().is_empty() {
        format!("king:{}", scope.trim())
    } else if !session.trim().is_empty() {
        format!("target:{}", session.trim())
    } else {
        String::new()
    }
}

fn lifecycle_leg(harness: &str, command: &str) -> ReadinessLeg {
    if command.trim().is_empty() {
        return ReadinessLeg::ready();
    }
    let contract = match crate::harness_capabilities::HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => return ReadinessLeg::unreadable(error.to_string()),
    };
    let caps = match contract.capabilities(harness) {
        Ok(caps) => caps,
        Err(error) => return ReadinessLeg::unreadable(error.to_string()),
    };
    match caps.loop_participation.as_str() {
        "native" => ReadinessLeg::ready(),
        "extension" if !caps.loop_extension.is_empty() => {
            let path = std::env::current_dir()
                .ok()
                .map(|root| root.join(&caps.loop_extension));
            if path.as_ref().is_some_and(|path| path.is_file()) {
                ReadinessLeg::ready()
            } else {
                ReadinessLeg::unreadable(format!(
                    "loop extension is absent or stale: {}",
                    caps.loop_extension
                ))
            }
        }
        participation => ReadinessLeg::blocked(format!(
            "harness {harness:?} declares loop_participation={participation:?}"
        )),
    }
}

fn provider_goal_leg(harness: &str) -> ReadinessLeg {
    if harness != "codex" {
        return ReadinessLeg::ready();
    }
    let contract = match crate::harness_capabilities::HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => return ReadinessLeg::unreadable(error.to_string()),
    };
    let Ok(caps) = contract.capabilities(harness) else {
        return ReadinessLeg::unreadable(format!("unknown provider {harness:?}"));
    };
    if caps.provider_action("goal_get").is_some() && caps.provider_action("goal_set").is_some() {
        ReadinessLeg::ready()
    } else {
        ReadinessLeg::unreadable(
            "Codex native goal_get and goal_set actions are not both declared".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_snapshot_keeps_the_four_admission_legs_distinct() {
        let snapshot = measure(
            ReadinessLeg::ready(),
            ReadinessLeg::ready(),
            ReadinessLeg::ready(),
            ReadinessLeg::ready(),
            "king:x-0000",
        );
        assert!(snapshot.ready());
        assert_eq!(snapshot.continuation_owner, "king:x-0000");
        assert_eq!(
            snapshot.to_json()["legs"]["provider_goal"]["state"],
            "ready"
        );
    }

    #[test]
    fn an_unreadable_leg_refuses_without_becoming_false_health() {
        let snapshot = measure(
            ReadinessLeg::unreadable("probe failed"),
            ReadinessLeg::ready(),
            ReadinessLeg::ready(),
            ReadinessLeg::ready(),
            "king:x-0000",
        );
        let refusal = admit(snapshot).unwrap_err();
        assert!(refusal.contains("machine"));
        assert!(refusal.contains("probe failed"));
    }

    #[test]
    fn target_continuation_owner_uses_the_exact_session_id() {
        assert_eq!(
            continuation_owner("", "thread-full-id"),
            "target:thread-full-id"
        );
        assert_eq!(
            continuation_owner("scope-a", "thread-full-id"),
            "king:scope-a"
        );
        assert!(continuation_owner("", "").is_empty());
    }

    #[test]
    fn codex_machine_leg_requires_the_enabled_footnote_plugin() {
        let enabled = codex_machine_leg_from_plugin_list(
            r#"{"installed":[{"pluginId":"fno@footnote","installed":true,"enabled":true}]}"#,
        );
        assert!(enabled.state.is_ready());

        let missing = codex_machine_leg_from_plugin_list(r#"{"installed":[]}"#);
        assert_eq!(missing.state, LegState::Blocked);
        let reason = missing.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("plugin-missing"));
        assert!(reason.contains("fno config plugin install codex"));
    }

    #[test]
    fn stop_leg_requires_a_session_snapshot_and_a_later_correlated_stop() {
        let absent = stop_leg_from_events("", "codex", "thread-a");
        assert!(absent
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("session-hook-unobserved"));

        let snapshot = serde_json::json!({
            "ts": "2026-09-23T10:00:00Z",
            "type": "context_snapshot",
            "source": "hook",
            "data": {
                "session_id": "thread-a",
                "harness": "codex",
                "entry_state": "resume",
                "measurement_complete": true
            }
        });
        let events = format!("{}\n", snapshot);
        let unrefreshed = stop_leg_from_events(&events, "codex", "thread-a");
        assert_eq!(unrefreshed.state, LegState::Blocked);
        assert!(unrefreshed
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("session-refresh-unverified"));

        let stop = serde_json::json!({
            "ts": "2026-09-23T10:01:00Z",
            "type": "stop_decision",
            "source": "hook",
            "data": {
                "session_id": "thread-a",
                "correlation_id": "stop:thread-a:turn-1"
            }
        });
        let events = format!("{snapshot}\n{stop}\n");
        let refreshed = stop_leg_from_events(&events, "codex", "thread-a");
        assert!(refreshed.state.is_ready());
    }
}
