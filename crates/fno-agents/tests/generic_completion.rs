use fno_agents::loopcheck::TerminationReason;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_fno-agents");

struct GenericEnv {
    _tmp: TempDir,
    cwd: PathBuf,
    state: PathBuf,
    transcript: PathBuf,
    events: PathBuf,
    global_events: PathBuf,
    evaluator: PathBuf,
}

fn setup(response: &str) -> GenericEnv {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("project");
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    let state = cwd.join(".fno/target-state.md");
    fs::write(
        &state,
        "---\nsession_id: generic-session\ncreated_at: 2026-08-02T12:00:00Z\nattended: true\nplan_path: plan.md\n---\ngraph_node_id: x-delivery\n",
    )
    .unwrap();
    fs::write(
        cwd.join("plan.md"),
        "---\nnode: x-delivery\ncompletion: delivery\n---\n",
    )
    .unwrap();
    let transcript = cwd.join("transcript.jsonl");
    fs::write(
        &transcript,
        "{\"message\":{\"role\":\"assistant\",\"content\":\"<promise>\"}}\n",
    )
    .unwrap();
    let events = cwd.join(".fno/events.jsonl");
    fs::write(&events, "").unwrap();
    let global_events = tmp.path().join("global-events.jsonl");
    let evaluator = tmp.path().join("fno-evaluator");
    fs::write(
        &evaluator,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            response.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    fs::set_permissions(&evaluator, fs::Permissions::from_mode(0o755)).unwrap();
    GenericEnv {
        _tmp: tmp,
        cwd,
        state,
        transcript,
        events,
        global_events,
        evaluator,
    }
}

fn run(env: &GenericEnv) -> Value {
    let output = Command::new(BIN)
        .arg("loop-check")
        .arg("--state")
        .arg(&env.state)
        .arg("--transcript")
        .arg(&env.transcript)
        .arg("--cwd")
        .arg(&env.cwd)
        .arg("--events")
        .arg(&env.events)
        .arg("--global-events")
        .arg(&env.global_events)
        .arg("--settings")
        .arg(env.cwd.join("missing-settings.toml"))
        .arg("--ledger")
        .arg(env.cwd.join("missing-ledger.json"))
        .arg("--gh-bin")
        .arg(env.cwd.join("missing-gh"))
        .arg("--git-bin")
        .arg(env.cwd.join("missing-git"))
        .env("FNO_LOOPCHECK_FNO_BIN", &env.evaluator)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn passed_response() -> String {
    json!({
        "version": "delivery-evaluate-response.v1",
        "status": "evaluated",
        "fact_revision": "sha256:abc",
        "verdict": {
            "evaluator_version": "delivery-evaluator.v1",
            "session_id": null,
            "work_order_node_id": "x-delivery",
            "attempt_id": "attempt-1",
            "aggregate": "passed",
            "fact_revision": "sha256:abc",
            "requirements": [{
                "deliverable_id": "output",
                "evidence_id": "artifact-ready",
                "subject_kind": "artifact",
                "subject_id": "artifact-1",
                "result": "passed",
                "producers": ["adapter:test"],
                "source_revisions": ["artifact-sha"],
                "diagnostics": []
            }],
            "diagnostics": []
        },
        "diagnostics": []
    })
    .to_string()
}

fn event_types(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event.get("type")?.as_str().map(str::to_owned))
        .collect()
}

#[test]
fn generic_completion_preserves_legacy_terminal_serialization() {
    assert_eq!(
        serde_json::to_string(&TerminationReason::DonePRGreen).unwrap(),
        "\"DonePRGreen\""
    );
    assert_eq!(
        serde_json::to_string(&TerminationReason::DoneAdvisory).unwrap(),
        "\"DoneAdvisory\""
    );
}

#[test]
fn generic_completion_ac_d7_hp_passed_canonical_verdict_terminates_without_gh() {
    let env = setup(&passed_response());

    let output = run(&env);

    assert_eq!(output["decision"], "allow");
    assert_eq!(output["termination_reason"], "DoneDelivery");
    assert!(event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
    let events = fs::read_to_string(&env.events).unwrap();
    assert!(events.lines().any(|line| {
        let event: Value = serde_json::from_str(line).unwrap();
        event["type"] == "loop_check"
            && event["data"]["decision"] == "allow"
            && event["data"]["fact_revision"] == "sha256:abc"
    }));
}

#[test]
fn generic_completion_ac_d6_err_malformed_evaluator_json_never_terminates() {
    let env = setup("{not-json");

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
    assert!(!event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
}

#[test]
fn generic_completion_ac_d8_inv_passed_observation_cannot_unlock() {
    let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
    response["verdict"]["requirements"][0]["subject_kind"] = json!("observation");
    let env = setup(&response.to_string());

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
}

#[test]
fn generic_completion_ac_d10_err_verdict_must_be_durably_appended() {
    let env = setup(&passed_response());
    fs::remove_file(&env.events).unwrap();
    fs::create_dir(&env.events).unwrap();

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
}
