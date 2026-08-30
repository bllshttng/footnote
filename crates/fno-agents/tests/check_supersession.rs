use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[path = "../codegen/check_supersession.rs"]
mod generator;

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    input: Vec<Value>,
    expected: Vec<Value>,
}

fn cases() -> Vec<Case> {
    serde_json::from_str(include_str!("fixtures/check_supersession.json"))
        .expect("shared check supersession corpus must be valid JSON")
}

#[test]
fn corpus_matches_named_expected_rows() {
    for case in cases() {
        let actual = fno_agents::check_supersession::latest_per_name(&Value::Array(case.input));
        assert_eq!(actual, Value::Array(case.expected), "case {}", case.name);
    }
}

#[test]
fn tracked_python_source_is_fresh_and_generator_ran() {
    let contract_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("codegen/check_supersession.toml");
    let contract = generator::load_contract(&contract_path).expect("contract must parse");
    assert!(generator::render_rust(&contract).contains("pub fn latest_per_name"));
    let generated = generator::render_python(&contract);
    let tracked_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../cli/src/fno/pr/_check_supersession_generated.py");
    let tracked = std::fs::read_to_string(&tracked_path)
        .expect("tracked Python output must exist; run cargo build to regenerate it");
    assert_eq!(
        tracked, generated,
        "generated Python is stale; run cargo build --manifest-path crates/fno-agents/Cargo.toml"
    );
    println!("generated check supersession source: fresh");
}
