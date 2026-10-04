//! The utc_hours lane field: a lane's own UTC clock gate. Out-of-window
//! lanes skip with a receipt before any capacity read; in-window lanes ride
//! an unprobed capacity read. Helpers come from route_slot's own `tests`
//! module.

use super::tests::{chain_of, payload};
use super::*;
use serde_json::json;

#[test]
fn utc_hours_window_rows() {
    // Out of window: the lane skips with a receipt and the walk falls
    // through to the next lane.
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
            .any(|l| l == "slot skip agents.profiles.target.lanes[0] outside utc_hours(9-10) now=11z"),
        "chain: {chain:?}"
    );

    // In window: the real lane shape picks - provider names the HARNESS
    // (zcode takes no model), and zcode has no capacity entry, so the
    // window carries it; utc_hours never rides the candidate.
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

    // 15-01 spans 15:00-01:00 UTC across midnight: hour 0 inside, hour 2
    // outside (the single lane's walk-out holds as capacity-held).
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
            .any(|l| l == "slot skip agents.profiles.target.lanes[0] outside utc_hours(15-01) now=2z"),
        "chain: {chain:?}"
    );

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
    assert_eq!(out["candidate"]["evidence"]["capacity"], "unknown-permitted");
    let chain = chain_of(&out);
    assert!(
        chain
            .iter()
            .any(|l| l.contains("capacity=unknown-permitted")),
        "chain: {chain:?}"
    );

    // A malformed window refuses as a config fault before the walk.
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
