use crate::loopcheck::TerminationReason;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct LoopCheckOutput {
    decision: String,
    termination_reason: Option<TerminationReason>,
    message: String,
    fires: u64,
    fingerprint: Option<String>,
    /// On a block decision only: the re-drive the gate names. A sent
    /// continuation carries what the gate said, not a literal the transport
    /// picked, so a gate-directed re-drive is distinguishable from user text.
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation: Option<String>,
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
        continuation: if decision == "block" {
            Some("/target --resume".to_string())
        } else {
            None
        },
    };
    serde_json::to_string(&out).unwrap_or_else(|_| r#"{"decision":"allow","termination_reason":null,"message":"serialization error","fires":0,"fingerprint":null}"#.to_string())
}

pub(crate) fn paused_output(driver: &str, message: &str) -> String {
    if driver == "king" {
        return serde_json::json!({
            "driver": "king",
            "decision": "allow",
            "termination_reason": null,
            "reason": message,
            "message": message,
            "actionable": 0,
            "fires": 0,
        })
        .to_string();
    }
    allow_output("allow", None, message, 0, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopcheck::TerminationReason;

    #[test]
    fn allow_output_serializes_correctly() {
        let json = allow_output(
            "allow",
            Some(TerminationReason::DonePRGreen),
            "done",
            3,
            Some("fp".into()),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["decision"], "allow");
        // Verify variant names serialize byte-identically to the spec strings.
        assert_eq!(v["termination_reason"], "DonePRGreen");
        assert_eq!(v["fires"], 3);
        assert_eq!(v["fingerprint"], "fp");
        // An allow carries no continuation: only a block names its re-drive.
        assert!(v.get("continuation").is_none());
    }

    #[test]
    fn allow_output_null_termination_reason() {
        let json = allow_output("block", None, "continue", 1, None);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["termination_reason"].is_null());
        assert!(v["fingerprint"].is_null());
        // A block names the continuation the gate directs.
        assert_eq!(v["continuation"], "/target --resume");
    }
}
