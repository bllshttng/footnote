//! One effective-readiness snapshot for target and crown admission.
//!
//! The four legs are deliberately separate. A healthy machine does not prove
//! that the lifecycle invokes the loop, and a lifecycle hook does not prove
//! that the provider can carry the native goal. Callers take one snapshot and
//! refuse on an unreadable leg instead of re-reading each concern independently.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

/// Direct client verb used by the Python admission seams. All four legs are
/// assembled before the result is rendered, so a caller pays one native read
/// and gets one refusal that names the failed leg.
pub fn run(args: &[String]) -> i32 {
    let harness = flag(args, "--harness").unwrap_or_else(|| "codex".to_string());
    let command = flag(args, "--command").unwrap_or_default();
    let scope = flag(args, "--scope").unwrap_or_default();
    let owner = flag(args, "--continuation-owner")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if scope.trim().is_empty() {
                format!("target:{harness}")
            } else {
                format!("king:{scope}")
            }
        });

    let machine = env_leg("FNO_LOOP_MACHINE", ReadinessLeg::ready);
    let stop = env_leg("FNO_LOOP_STOP", ReadinessLeg::ready);
    let lifecycle = lifecycle_leg(&harness, &command);
    let provider_goal = provider_goal_leg(&harness);
    let snapshot = measure(machine, lifecycle, stop, provider_goal, owner);
    println!("{}", snapshot.to_json());
    i32::from(!snapshot.ready())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn env_leg(name: &str, default: fn() -> ReadinessLeg) -> ReadinessLeg {
    match std::env::var(name).ok().as_deref() {
        Some("ready") | None => default(),
        Some("blocked") => ReadinessLeg::blocked(format!("{name}=blocked")),
        Some("unreadable") => ReadinessLeg::unreadable(format!("{name}=unreadable")),
        Some(value) => ReadinessLeg::unreadable(format!("{name} has unknown state {value:?}")),
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
            "king:x-e64a",
        );
        assert!(snapshot.ready());
        assert_eq!(snapshot.continuation_owner, "king:x-e64a");
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
            "king:x-e64a",
        );
        let refusal = admit(snapshot).unwrap_err();
        assert!(refusal.contains("machine"));
        assert!(refusal.contains("probe failed"));
    }
}
