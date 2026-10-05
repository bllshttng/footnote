//! The check-in's hold label: a machine-armed conversation hold is its own
//! state, never the `DND on` a lead reads as a hold it armed itself and
//! invents a reason for. One function so every render path labels the same
//! hold the same way. The label split is guarded at the integration points:
//! the conversation leg through mail_hold's arming test, the DND and
//! no-hold legs through lead_checkin's diff_rows tests.

use serde_json::Value;

/// The attention line a live hold earns, or None when no hold shows. A
/// conversation-sourced clock (the machine armed it when the user started
/// talking) reads as its own state; any other live clock or bus-only stamp
/// is the lead's own DND.
pub(crate) fn hold_attention(hold: &Value) -> Option<String> {
    if hold
        .get("conversation")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(
            "mail held while the user talks to you (machine-armed; lifts about 2 min after your answer)"
                .into(),
        );
    }
    let clock_live = hold
        .get("clock_live")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let bus_only = hold.get("delivery_policy").and_then(Value::as_str) == Some("bus-only");
    (clock_live || bus_only).then(|| "DND on".into())
}
