//! The route preview reads the capacity the spawn gate reads.
//! AC1-AC3: the states leg arms the walk's one refresh and its lane rows read
//! the map the walk judged. AC6-AC7: an all-unknown walk-out refuses as
//! unknown, not exhausted. Helpers come from route_slot's own `tests` module.

use super::tests::{
    chain_of, fresh_codex_row, now_epoch, payload, slot_env_payload, stale_codex_row, state_json,
    write_refresh_stub, CapacityEnv,
};
use super::*;
use serde_json::json;

/// AC1-HP: the readout's walk refreshes a stale lane itself, and both the
/// lane row and `would_take` read the map the walk judged: `codex-luna` ok,
/// the pick the gate would make, without a manual refresh first.
#[test]
fn states_walk_refreshes_a_stale_lane_and_rows_read_the_judged_map() {
    let env = CapacityEnv::new(&state_json(Some(&stale_codex_row())), None);
    let marker = env.dir.path().join("marker");
    let stub = write_refresh_stub(env.dir.path(), &fresh_codex_row(), &marker);
    std::env::set_var("FNO_BIN", &stub);
    let out = resolve_slot_payload(&slot_env_payload(json!({
        "mode": "states",
    })));
    let row = out["lane_states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "codex-luna")
        .expect("the codex lane row");
    assert_eq!(row["state"], "ok", "lane_states: {:?}", out["lane_states"]);
    assert_eq!(
        out["would_take"],
        "agents.profiles.target.lanes[0] codex-luna"
    );
    assert_eq!(out["routing"], "armed");
    assert!(marker.exists(), "the readout never ran the refresh stub");
}

/// AC2-ERR: a refresh that fails (non-zero exit) fails OPEN: the readout
/// judges the stale map, the codex lane stays unknown, and the fresh claude
/// fallback answers. Non-zero exit returns immediately, well inside the
/// transport bound.
#[test]
fn states_walk_survives_a_failing_refresh_and_judges_the_stale_map() {
    let env = CapacityEnv::new(&state_json(Some(&stale_codex_row())), None);
    let stub = crate::write_exec_stub(env.dir.path(), "fail-stub.sh", "#!/bin/sh\nexit 1\n");
    std::env::set_var("FNO_BIN", &stub);
    let out = resolve_slot_payload(&slot_env_payload(json!({
        "mode": "states",
    })));
    let row = out["lane_states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "codex-luna")
        .expect("the codex lane row");
    assert_eq!(
        row["state"], "unknown",
        "lane_states: {:?}",
        out["lane_states"]
    );
    assert_eq!(
        out["would_take"],
        "agents.profiles.target.lanes[1] sonnet-x"
    );
    assert_eq!(out["routing"], "armed");
}

/// AC3-EDGE: an explicit capacity is the tests' seam: the readout walks with
/// the map it is given and never probes, same as `no_refresh_flag_means_no_probe`.
#[test]
fn states_with_explicit_capacity_never_probes() {
    let env = CapacityEnv::new(&state_json(Some(&stale_codex_row())), None);
    let marker = env.dir.path().join("marker");
    let stub = write_refresh_stub(env.dir.path(), &fresh_codex_row(), &marker);
    std::env::set_var("FNO_BIN", &stub);
    let out = resolve_slot_payload(&slot_env_payload(json!({
        "mode": "states",
        "capacity": {
            "codex": {"state": "unknown", "window": "stale",
                      "accounts": {"codex": "unknown"},
                      "sources": {"codex": "stale"},
                      "observed_at": {"codex": now_epoch() - 900.0},
                      "evidence": {}, "resets": {}},
            "claude": {"state": "ok", "window": "window",
                       "accounts": {"makers": "ok"},
                       "sources": {"makers": "window"},
                       "observed_at": {"makers": now_epoch()},
                       "evidence": {"makers": "proven"}, "resets": {}},
        },
    })));
    assert!(
        !marker.exists(),
        "the readout probed with an explicit capacity"
    );
    assert_eq!(
        out["would_take"],
        "agents.profiles.target.lanes[1] sonnet-x"
    );
}

/// AC6-HP: every lane skipped ONLY on unknown capacity refuses as UNKNOWN:
/// the terminal is `slot=unknown refuse`, the reason kind is
/// `capacity-unknown`, and the composed text names the cause and the fix.
#[test]
fn all_lanes_unknown_refuses_as_unknown_not_exhausted() {
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": ["flash-x"],
        "profile": {"on_exhausted": "refuse", "on_unknown": "skip"},
        "capacity": {"claude": {"state": "unknown", "window": "stale",
                                "accounts": {"zai-main": "unknown"},
                                "sources": {"zai-main": "stale"},
                                "observed_at": {"zai-main": now_epoch() - 15.0 * 3600.0},
                                "evidence": {}, "resets": {}}},
    })));
    assert_eq!(
        chain_of(&out).last().map(String::as_str),
        Some("slot=unknown refuse"),
        "chain: {:?}",
        chain_of(&out)
    );
    assert_eq!(out["verdict"], "capacity-held");
    assert_eq!(out["reason_kind"], "capacity-unknown");
    assert_eq!(out["refusal_terminal"]["class"], "unknown-refuse");
    let text = out["refusal_terminal"]["text"].as_str().unwrap();
    assert!(
        text.contains(
            "every configured lane reads capacity unknown (on_unknown=skip); none is exhausted"
        ),
        "terminal: {text}"
    );
    assert!(
        text.contains("oldest evidence 15h old (source=stale)"),
        "terminal: {text}"
    );
    assert!(
        text.contains("fno config accounts usage --refresh"),
        "terminal: {text}"
    );
}

/// AC7-EDGE: one exhausted lane and one unknown lane keep the exhausted
/// terminal: the mix is a real capacity stand-down, not the unknown cause.
#[test]
fn a_mixed_exhausted_and_unknown_walkout_keeps_the_exhausted_terminal() {
    let out = resolve_slot_payload(&payload(json!({
        "lanes_raw": ["flash-x", "sonnet-x"],
        "profile": {"on_exhausted": "refuse", "on_unknown": "skip"},
        "capacity": {"claude": {"state": "unknown", "window": "stale",
                                "accounts": {"zai-main": "exhausted"},
                                "sources": {"zai-main": "window"},
                                "observed_at": {"zai-main": now_epoch() - 10.0},
                                "evidence": {}, "resets": {}}},
    })));
    assert_eq!(
        chain_of(&out).last().map(String::as_str),
        Some("slot=exhausted refuse"),
        "chain: {:?}",
        chain_of(&out)
    );
    assert_eq!(out["reason_kind"], "capacity-exhausted");
    assert_eq!(out["refusal_terminal"]["class"], "exhausted-refuse");
}
