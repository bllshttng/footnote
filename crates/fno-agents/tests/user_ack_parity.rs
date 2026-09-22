//! parity-stage: characterization
//! parity-oracle: fno.inbox.operator_turns.ack_turn (the deleted Python body,
//! goldens captured from the canonical checkout before the shim swap)
//!
//! Characterization tests for the Rust ack write (`compaction ack`) behind
//! `fno inbox user ack`. The contract is the Python ledger row, field for
//! field: turn_id, outcome, law/capture/node ref, why. The timestamp differs
//! by clock, so it is checked for presence and string shape, never byte
//! equality. `answer` is the fourth kind the Rust write adds; its ledger row
//! matches the same shape, and it additionally lands one user_ask_answered
//! row (covered by unit tests in operator_turns.rs).

use common::{assert_golden, golden_dir, Golden};
use fno_agents::operator_turns::ack_turn;
use std::fs;
use std::path::{Path, PathBuf};

mod common;

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "fno-agents-user-ack-parity-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// The contract: one normalized ledger row. `ts` is checked for presence and
/// dropped; the remaining fields freeze in a fixed key order, so a Python row
/// and a Rust row freeze to the same bytes when they match.
fn normalized_row(dir: &Path, session: &str) -> String {
    let raw = fs::read_to_string(dir.join(format!("{session}.jsonl"))).unwrap();
    let v: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    assert!(
        v.get("ts").and_then(|t| t.as_str()).is_some(),
        "row carries ts"
    );
    let field = |name: &str| -> String {
        v.get(name)
            .map(|x| serde_json::to_string(x).unwrap())
            .unwrap_or_else(|| "null".to_string())
    };
    format!(
        "{{\"turn_id\": {}, \"outcome\": {}, \"ref\": {}, \"why\": {}}}",
        field("turn_id"),
        field("outcome"),
        field("ref"),
        field("why"),
    )
}

fn golden_of(dir: &Path, session: &str) -> Golden {
    Golden {
        exit: Some(0),
        streams: vec![normalized_row(dir, "s")],
    }
}

#[test]
fn law_capture_node_nothing_match_the_python_row() {
    let dir = tmp_dir("kinds");
    for (label, outcome, turn) in [
        ("law", "law:d-parity0001", "u-law"),
        ("capture", "capture:fu-parity", "u-capture"),
        ("node", "node:x-aaaa", "u-node"),
        ("nothing", "nothing", "u-nothing"),
    ] {
        let case_dir = dir.join(label);
        fs::create_dir_all(&case_dir).unwrap();
        let home = fno_agents::paths::AgentsHome::at(&case_dir.join("home"));
        let row = ack_turn(&home, &case_dir, "s", turn, outcome, "because").unwrap();
        assert_eq!(row["turn_id"], turn);
        assert_golden(
            "user_ack",
            &format!("{label}_ack"),
            &golden_of(&case_dir, "s"),
            None,
        );
    }
}

#[test]
fn answer_ack_matches_and_names_the_answer() {
    let dir = tmp_dir("answer");
    let home = fno_agents::paths::AgentsHome::at(&dir.join("home"));
    let row = ack_turn(
        &home,
        &dir,
        "s",
        "u-answer",
        "answer:use the narrow reading",
        "",
    )
    .unwrap();
    assert_eq!(row["outcome"], "answer:use the narrow reading");
    assert_eq!(row["ref"], "use the narrow reading");
    assert_golden("user_ack", "answer_ack", &golden_of(&dir, "s"), None);
}
