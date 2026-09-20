//! The conversion contract, read through the PACKAGED capability table
//! rather than a fixture.
//!
//! The unit tests beside the classifier build their own contract rows, so
//! they prove the classifier and nothing about what ships. These tests read
//! the table the binary carries, which is the one an operator's conversion
//! actually consults.

use fno_agents::convert::{classify, ConvertRefusal, KeeperSighting, PaneRead};
use fno_agents::harness_capabilities::{HarnessContract, CONVERSION_STRATEGIES};
use fno_agents::state::{MuxRef, RegistryEntry};
use fno_agents::AgentStatus;

/// Every harness the table names, so a new row cannot land without a
/// conversion answer.
fn harnesses() -> Vec<String> {
    let contract = HarnessContract::packaged().expect("the packaged table parses");
    contract.harness.keys().cloned().collect()
}

fn pane_row(harness: &str) -> RegistryEntry {
    let mut entry = RegistryEntry {
        name: "convert-proof".to_string(),
        cwd: "/repo".to_string(),
        status: AgentStatus::Live,
        created_at: "2026-09-20T00:00:00Z".to_string(),
        ..Default::default()
    };
    entry.harness = Some(harness.to_string());
    entry.harness_session_id = Some("01a0cafe-0000-4c1e-8a1c-2d3e4f5a6b7c".to_string());
    entry.substrate = Some("pane".to_string());
    entry.short_id = "cvproof1".to_string();
    entry.mux = Some(MuxRef {
        session: "fno".to_string(),
        pane_id: 41,
    });
    entry.pid = Some(4141);
    entry
}

#[test]
fn every_packaged_harness_declares_a_conversion_strategy_from_the_known_set() {
    let contract = HarnessContract::packaged().expect("the packaged table parses");
    for harness in harnesses() {
        let resolved = contract
            .conversion(&harness)
            .unwrap_or_else(|error| panic!("{harness}: {error}"));
        assert!(
            CONVERSION_STRATEGIES.contains(&resolved.strategy.as_str()),
            "{harness} declares strategy {:?}, which is not one this runtime can run",
            resolved.strategy
        );
        // The coherence rule the loader enforces, asserted against what
        // SHIPS: an unsupported row owes the operator a reason, and a
        // supported one must not carry a refusal it will never print.
        if resolved.strategy == "unsupported" {
            assert!(
                !resolved.refusal.is_empty(),
                "{harness} refuses conversion and names no reason"
            );
            assert!(
                !resolved.preserves_id,
                "{harness} cannot convert, so it cannot promise to preserve an id"
            );
        } else {
            assert!(
                resolved.refusal.is_empty(),
                "{harness} converts and still carries a refusal: {}",
                resolved.refusal
            );
            assert!(
                resolved.preserves_id,
                "{harness} converts without preserving the session id; a conversion that \
                 changes the address is a fork"
            );
        }
    }
}

#[test]
fn an_unsupported_harness_refuses_by_name_and_touches_nothing() {
    let contract = HarnessContract::packaged().expect("the packaged table parses");
    let resolved = contract
        .conversion("opencode")
        .expect("opencode is declared");
    assert_eq!(resolved.strategy, "unsupported");

    let entry = pane_row("opencode");
    let panes = [PaneRead {
        session: "fno".to_string(),
        pane_id: 41,
        child_pid: Some(4141),
    }];
    let Err(refusal) = classify(&entry, &resolved, &panes, &[]) else {
        panic!("an unsupported harness must refuse");
    };
    assert!(matches!(refusal, ConvertRefusal::Refused(_)));
    // The refusal carries the table's own sentence, so the operator reads
    // the measured reason rather than a generic "unsupported".
    assert!(
        refusal.message().contains("opencode"),
        "{}",
        refusal.message()
    );
}

#[test]
fn a_keeper_lane_harness_refuses_when_no_keeper_holds_its_pane() {
    let contract = HarnessContract::packaged().expect("the packaged table parses");
    // Find a harness the shipped table routes down the keeper lane, so this
    // test follows the table rather than pinning a name.
    let harness = harnesses()
        .into_iter()
        .find(|name| {
            contract
                .conversion(name)
                .map(|resolved| resolved.strategy == "keeper-rebind")
                .unwrap_or(false)
        })
        .expect("the shipped table routes at least one harness down the keeper lane");
    let resolved = contract.conversion(&harness).expect("declared above");

    let entry = pane_row(&harness);
    let panes = [PaneRead {
        session: "fno".to_string(),
        pane_id: 41,
        child_pid: Some(4141),
    }];
    // No keeper sighting: the strategy moves a keeper, so there is nothing
    // to move. It must refuse rather than stop a process it cannot resume.
    let Err(refusal) = classify(&entry, &resolved, &panes, &[]) else {
        panic!("{harness} must refuse a keeper-rebind with no keeper");
    };
    assert!(matches!(refusal, ConvertRefusal::Refused(_)));

    // The control: with the keeper in view, the same inputs classify.
    let keepers = [KeeperSighting {
        socket: "/tmp/mux/panes/41.sock".to_string(),
        keeper_pid: 4140,
        child_pid: 4141,
        cwd: "/repo".to_string(),
        // The keeper reads the session id off the child argv, never from a
        // field of its own, so the sighting carries the argv itself.
        argv: vec![
            harness.clone(),
            "--resume".to_string(),
            entry
                .harness_session_id
                .clone()
                .expect("the row carries a session id"),
        ],
        stale: None,
    }];
    let plan = classify(&entry, &resolved, &panes, &keepers)
        .expect("a keeper in view is what the strategy needs");
    assert_eq!(plan.strategy, "keeper-rebind");
    assert_eq!(plan.harness, harness);
}

#[test]
fn a_row_already_on_the_thread_lane_is_idempotent_not_an_error() {
    let contract = HarnessContract::packaged().expect("the packaged table parses");
    let harness = harnesses()
        .into_iter()
        .find(|name| {
            contract
                .conversion(name)
                .map(|resolved| resolved.strategy != "unsupported")
                .unwrap_or(false)
        })
        .expect("at least one harness converts");
    let resolved = contract.conversion(&harness).expect("declared above");

    let mut entry = pane_row(&harness);
    entry.substrate = Some("thread".to_string());
    entry.mux = None;
    let Err(refusal) = classify(&entry, &resolved, &[], &[]) else {
        panic!("a thread row has nothing to convert");
    };
    assert!(
        matches!(refusal, ConvertRefusal::AlreadyAThread { .. }),
        "asking for a state the world is already in is a receipt, not a failure: {}",
        refusal.message()
    );
}
