//! The claude Code hook payload: the shape hooks.json producers send.

use super::{effort_field, str_field, HookEvent};
use serde_json::Value;

pub(crate) fn normalize(event: &str, payload: &Value) -> HookEvent {
    let tool = str_field(payload, "tool_name");
    // The question text a background session is blocked on: the picker's
    // tool_input carries the questions, so the blocked report can name WHAT
    // is being asked instead of the static fallback.
    let message = match payload.get("message").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => question_text(payload, &tool),
    };
    HookEvent {
        harness: "claude".into(),
        event: event.into(),
        session_id: str_field(payload, "session_id"),
        tool,
        message,
        model: str_field(payload, "to_model"),
        effort: effort_field(payload),
        posture: None,
    }
}

/// The first question's text out of an AskUserQuestion `tool_input`; empty
/// when the payload carried no parseable question.
fn question_text(payload: &Value, tool: &str) -> String {
    if tool != "AskUserQuestion" {
        return String::new();
    }
    payload
        .get("tool_input")
        .and_then(|ti| ti.get("questions"))
        .and_then(Value::as_array)
        .and_then(|qs| qs.first())
        .and_then(|q| q.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_message_and_postmodelswitch_axes_ride() {
        let n = normalize(
            "Notification",
            &json!({"session_id": "s1", "message": "waiting on permission to run rm"}),
        );
        assert_eq!(n.session_id, "s1");
        assert_eq!(n.message, "waiting on permission to run rm");

        // A bare-string effort rides (the shell producer's string-effort case).
        let m = normalize(
            "PostModelSwitch",
            &json!({"session_id": "s1", "to_model": "glm-5.3[1m]", "effort": "high"}),
        );
        assert_eq!(m.model, "glm-5.3[1m]");
        assert_eq!(m.effort, "high");

        // The {"level": ...} object spelling too.
        let o = normalize(
            "PostModelSwitch",
            &json!({"session_id": "s1", "to_model": "glm-5.3[1m]", "effort": {"level": "xhigh"}}),
        );
        assert_eq!(o.effort, "xhigh");

        // No effort object: the field stays empty, the daemon-side row effort
        // is left to the axis that actually owns it.
        let none = normalize(
            "UserPromptSubmit",
            &json!({"session_id": "s1", "prompt": "go"}),
        );
        assert_eq!(none.effort, "");
        assert_eq!(none.model, "");
    }

    #[test]
    fn picker_question_text_is_the_message() {
        let q = normalize(
            "PreToolUse",
            &json!({
                "session_id": "s1", "tool_name": "AskUserQuestion",
                "tool_input": {"questions": [
                    {"question": "Pick a budget door: hard cap or soft warn?", "header": "Budget"}
                ]}
            }),
        );
        assert_eq!(q.tool, "AskUserQuestion");
        assert_eq!(q.message, "Pick a budget door: hard cap or soft warn?");

        // No tool_input: the message stays empty, the job applies its static
        // fallback.
        let bare = normalize(
            "PreToolUse",
            &json!({"session_id": "s1", "tool_name": "AskUserQuestion"}),
        );
        assert_eq!(bare.message, "");

        // A non-picker tool never mines tool_input for a message.
        let bash = normalize(
            "PreToolUse",
            &json!({"session_id": "s1", "tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        assert_eq!(bash.message, "");
        assert_eq!(bash.tool, "Bash");
    }
}
