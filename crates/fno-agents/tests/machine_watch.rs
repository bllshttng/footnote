//! Branch agreement between the two suites (x-d6ad AC11): the Rust arm reads
//! the same three-payload fixture the Python decider pins, and takes the
//! branch each verdict names. The reason sentence prints verbatim; nothing
//! here computes a busy fraction.

use fno_agents::machine_watch::{tick_machine_watch, MachineWatchState};
use fno_agents::spawn_gate::FootprintCausePayload;
use serde_json::Value;
use std::time::Instant;

fn fixture_cases() -> Vec<(String, FootprintCausePayload, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../cli/tests/agents/fixtures/machine_pressure.json"
    );
    let text = std::fs::read_to_string(path).expect("the shared fixture must exist");
    let fixture: Value = serde_json::from_str(&text).expect("fixture parses");
    let cases = fixture["cases"].as_array().expect("cases array");
    cases
        .iter()
        .map(|case| {
            let name = case["name"].as_str().unwrap_or_default().to_string();
            let payload: FootprintCausePayload =
                serde_json::from_value(case["payload"].clone()).expect("payload parses");
            let reason = case["payload"]["machine"]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            (name, payload, reason)
        })
        .collect()
}

#[test]
fn rust_takes_the_branch_each_fixture_verdict_names() {
    for (name, payload, reason) in fixture_cases() {
        let mut state = MachineWatchState::default();
        let mut notified = 0usize;
        let outcome = tick_machine_watch(
            &mut state,
            Ok(&payload),
            |_: &str, _: &str| {
                notified += 1;
                true
            },
            Instant::now(),
        );
        let expected = match name.as_str() {
            "hot_box" => "debouncing",
            "busy_but_calm_box" => "calm",
            "unreadable_sensor" => "machine_unreadable",
            other => panic!("unknown fixture case {other}"),
        };
        assert_eq!(
            outcome.skip_reason.as_deref(),
            Some(expected),
            "case {name}"
        );
        assert_eq!(outcome.acted, 0, "acted: case {name}");
        assert_eq!(notified, 0, "no notice on the first tick, case {name}");
        if expected == "machine_unreadable" {
            assert_eq!(outcome.detail, reason, "the reason prints verbatim");
        }
    }
}

#[test]
fn the_hot_fixture_case_escalates_with_the_reason_verbatim() {
    let mut escalated = false;
    for (name, payload, reason) in fixture_cases() {
        if name != "hot_box" {
            continue;
        }
        escalated = true;
        let mut state = MachineWatchState::default();
        let mut bodies: Vec<String> = Vec::new();
        tick_machine_watch(&mut state, Ok(&payload), |_, _| true, Instant::now());
        let second = tick_machine_watch(
            &mut state,
            Ok(&payload),
            |_, body| {
                bodies.push(body.to_string());
                true
            },
            Instant::now(),
        );
        assert_eq!(second.acted, 1);
        assert_eq!(bodies.len(), 1);
        assert!(
            bodies[0].contains(&reason),
            "reason verbatim: {}",
            bodies[0]
        );
    }
    assert!(escalated, "the fixture must carry a hot case");
}
