//! The utc_hours lane field: a lane's own UTC clock gate. Out-of-window
//! lanes skip with a receipt before any capacity read; in-window lanes ride
//! an unprobed capacity read. Helpers come from route_slot's own `tests`
//! module.

use super::tests::{chain_of, payload};
use super::*;
use serde_json::json;

// --- utc_hours: the lane's own clock gate ------------------------------- //

#[test]
fn utc_hours_out_of_window_skips_with_receipt_and_falls_through() {
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": [
            {"provider": "flash-x", "utc_hours": "9-10"},
            "sonnet-x",
        ],
        "utc_now_hour": 11,
    })));
    assert_eq!(out["status"], "pick");
    assert_eq!(out["candidate"]["lane"], "sonnet-x");
    let chain = chain_of(&out);
    assert!(
        chain
            .iter()
            .any(|l| l
                == "slot skip agents.profiles.target.lanes[0] outside utc_hours(9-10) now=11z"),
        "chain: {chain:?}"
    );
}

#[test]
fn utc_hours_in_window_picks_the_lane() {
    // The real lane shape: provider names the HARNESS (zcode takes no
    // model), and zcode has no capacity entry - the window carries it.
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": [{"provider": "zcode", "utc_hours": "9-10"}],
        "utc_now_hour": 9,
    })));
    assert_eq!(out["status"], "pick");
    assert_eq!(out["candidate"]["lane"], "agents.profiles.target.lanes[0]");
    assert_eq!(out["candidate"]["harness"], "zcode");
    assert_eq!(out["candidate"]["model"], "");
    let fields = out["candidate"]["lane_fields"].as_object().unwrap();
    assert!(
        !fields.contains_key("utc_hours"),
        "utc_hours must not ride the candidate: {fields:?}"
    );
}

#[test]
fn utc_hours_wrap_window_in_and_out() {
    // 15-01 spans 15:00-01:00 UTC across midnight: hour 0 inside, hour 2
    // outside.
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": [{"provider": "flash-x", "utc_hours": "15-01"}],
        "utc_now_hour": 0,
    })));
    assert_eq!(out["status"], "pick");
    assert_eq!(out["candidate"]["lane"], "agents.profiles.target.lanes[0]");

    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": [{"provider": "flash-x", "utc_hours": "15-01"}],
        "utc_now_hour": 2,
    })));
    assert_eq!(out["status"], "none");
    assert_eq!(out["verdict"], "capacity-held");
    let chain = chain_of(&out);
    assert!(
        chain
            .iter()
            .any(|l| l
                == "slot skip agents.profiles.target.lanes[0] outside utc_hours(15-01) now=2z"),
        "chain: {chain:?}"
    );
}

#[test]
fn utc_hours_window_overrides_unknown_capacity() {
    // The window is the lane's availability model: inside it, an unprobed
    // capacity read does not veto the lane even under on_unknown=skip.
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": [{"provider": "sonnet-x", "utc_hours": "9-10"}],
        "utc_now_hour": 9,
        "capacity": {"claude": {"state": "unknown"}},
        "profile": {"on_exhausted": "refuse", "on_low": "prefer_healthy",
                    "on_unknown": "skip", "by_difficulty": {}},
    })));
    assert_eq!(out["status"], "pick");
    assert_eq!(out["candidate"]["lane"], "agents.profiles.target.lanes[0]");
    assert_eq!(
        out["candidate"]["evidence"]["capacity"],
        "unknown-permitted"
    );
    let chain = chain_of(&out);
    assert!(
        chain
            .iter()
            .any(|l| l.contains("capacity=unknown-permitted")),
        "chain: {chain:?}"
    );
}

#[test]
fn utc_hours_malformed_window_refuses_as_config_fault() {
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": [{"provider": "flash-x", "utc_hours": "25-01"}],
        "utc_now_hour": 9,
    })));
    assert_eq!(out["status"], "none");
    assert_eq!(out["verdict"], "policy-held");
    let chain = chain_of(&out);
    assert!(
        chain.iter().any(|l| l.contains(".utc_hours must be H-H")),
        "chain: {chain:?}"
    );
}
