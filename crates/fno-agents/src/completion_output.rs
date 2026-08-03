use crate::loopcheck::TerminationReason;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct LoopCheckOutput {
    decision: String,
    termination_reason: Option<TerminationReason>,
    message: String,
    fires: u64,
    fingerprint: Option<String>,
}

pub(crate) fn allow_output(
    decision: &str,
    termination_reason: Option<TerminationReason>,
    message: &str,
    fires: u64,
    fingerprint: Option<String>,
) -> String {
    let out = LoopCheckOutput {
        decision: decision.to_string(),
        termination_reason,
        message: message.to_string(),
        fires,
        fingerprint,
    };
    serde_json::to_string(&out).unwrap_or_else(|_| r#"{"decision":"allow","termination_reason":null,"message":"serialization error","fires":0,"fingerprint":null}"#.to_string())
}
