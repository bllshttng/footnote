//! The check-in's hold label: a machine-armed conversation hold is
//! its own state, never the `DND on` a lead reads as a hold it armed itself
//! and invents a reason for. One function so every render path labels the
//! same hold the same way.

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_conversation_hold_reads_as_its_own_state() {
        let hold = json!({"clock_live": true, "clock_until": "2030-01-01T00:00:00Z",
                          "delivery_policy": "bus-only", "conversation": true});
        assert_eq!(
            hold_attention(&hold).unwrap(),
            "mail held while the user talks to you (machine-armed; lifts about 2 min after your answer)"
        );
    }

    #[test]
    fn a_hold_without_the_conversation_mark_stays_dnd() {
        // The manual legs: a live sidecar with no source, and a bare
        // bus-only stamp.
        let manual_clock = json!({"clock_live": true, "clock_until": "2030-01-01T00:00:00Z", "delivery_policy": "bus-only"});
        assert_eq!(hold_attention(&manual_clock).unwrap(), "DND on");
        let stamped =
            json!({"clock_live": false, "clock_until": null, "delivery_policy": "bus-only"});
        assert_eq!(hold_attention(&stamped).unwrap(), "DND on");
        let conversation_false = json!({"clock_live": true, "clock_until": "2030-01-01T00:00:00Z", "delivery_policy": null, "conversation": false});
        assert_eq!(hold_attention(&conversation_false).unwrap(), "DND on");
    }

    #[test]
    fn no_hold_reads_no_label() {
        let quiet = json!({"clock_live": false, "clock_until": null, "delivery_policy": null});
        assert_eq!(hold_attention(&quiet), None);
        assert_eq!(hold_attention(&Value::Null), None);
    }
}
