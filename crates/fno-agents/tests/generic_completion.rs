use fno_agents::delivery_completion::pr_passes;
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
        "---\nnode: x-delivery\nstatus: ready\ncreated: 2026-08-02\ncompletion: delivery\ncompany_work:\n  work_order:\n    node_id: x-delivery\n    attempt_id: attempt-1\n  deliverables:\n    - id: output\n      kind: arbitrary-output\n      work_order_id: x-delivery\n      attempt_id: attempt-1\n      required_evidence_ids: [artifact-ready]\n  evidence:\n    - id: artifact-ready\n      work_order_id: x-delivery\n      attempt_id: attempt-1\n      subject_kind: artifact\n      subject_id: artifact-1\n      result: passed\n---\n",
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
            "#!/bin/sh\ncase \"$*\" in\n  'do delivery evaluate --json --plan-path '*) printf '%s\\n' '{}' ;;\n  *) exit 64 ;;\nesac\n",
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
        .env("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS", "0")
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
        "evidence_revision": "sha256:evidence-1",
        "verdict": {
            "evaluator_version": "delivery-evaluator.v1",
            "session_id": null,
            "work_order_node_id": "x-delivery",
            "attempt_id": "attempt-1",
            "aggregate": "passed",
            "fact_revision": "sha256:abc",
            "required_requirements": [{
                "deliverable_id": "output",
                "evidence_id": "artifact-ready"
            }],
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

fn canonical_setup() -> GenericEnv {
    let env = setup(&passed_response());
    fs::write(
        env.cwd.join("plan.md"),
        "---\nnode: x-delivery\nstatus: ready\ncreated: 2026-08-02\ncompletion: delivery\ncompany_work:\n  work_order:\n    node_id: x-delivery\n    attempt_id: attempt-1\n  deliverables:\n    - id: output\n      kind: arbitrary-output\n      work_order_id: x-delivery\n      attempt_id: attempt-1\n      required_evidence_ids: [artifact-ready, review-ready]\n  evidence:\n    - id: artifact-ready\n      work_order_id: x-delivery\n      attempt_id: attempt-1\n      subject_kind: artifact\n      subject_id: artifact-1\n      result: passed\n    - id: review-ready\n      work_order_id: x-delivery\n      attempt_id: attempt-1\n      subject_kind: review\n      subject_id: review-1\n      result: passed\n---\n",
    )
    .unwrap();
    let event = |id: &str, kind: &str, subject: &str, producer: &str| {
        json!({
            "ts": "2026-08-02T12:00:00Z",
            "type": "delivery_evidence_observed",
            "source": "target",
            "data": {
                "version": "delivery-evidence-fact.v1",
                "evidence": {
                    "id": id,
                    "work_order_id": "x-delivery",
                    "attempt_id": "attempt-1",
                    "subject_kind": kind,
                    "subject_id": subject,
                    "result": "passed"
                },
                "producer": producer,
                "observed_at": "2026-08-02T12:00:00Z",
                "source_revision": format!("{id}-source"),
                "fresh_until": "2099-08-02T12:00:00Z",
                "adapter_version": "test.v1",
                "fact_revision": "producer-snapshot"
            }
        })
        .to_string()
    };
    fs::write(
        &env.events,
        format!(
            "{}\n{}\n",
            event(
                "artifact-ready",
                "artifact",
                "artifact-1",
                "adapter:artifact"
            ),
            event("review-ready", "review", "review-1", "adapter:review"),
        ),
    )
    .unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    fs::write(
        &env.evaluator,
        format!(
            "#!/bin/sh\nexec uv run --project '{}' fno-py \"$@\"\n",
            repo.join("cli").display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&env.evaluator, fs::Permissions::from_mode(0o755)).unwrap();
    env
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
fn generic_completion_ac_d4_compat_counterfactuals_match_pr_authority() {
    assert!(pr_passes(true, true, true, true, true));
    for failing_read in 0..5 {
        let mut reads = [true; 5];
        reads[failing_read] = false;
        assert!(
            !pr_passes(reads[0], reads[1], reads[2], reads[3], reads[4]),
            "legacy PR authority bypassed current read {failing_read}"
        );
    }
}

#[test]
fn generic_completion_ac_d4_compat_pr_adapter_matches_live_rust_authority() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let script = r#"
import datetime as dt
import json
from fno.delivery import LegacyPRSnapshot, adapt_legacy_pr

now = dt.datetime.now(dt.timezone.utc)
cases = [
    {"name": "passed", "pr_open": True, "ci_ok": True, "ci_pending": False, "reviewed": True, "head_shipped": True, "probes_passed": True},
    {"name": "closed", "pr_open": False, "ci_ok": True, "ci_pending": False, "reviewed": True, "head_shipped": True, "probes_passed": True},
    {"name": "ci-red", "pr_open": True, "ci_ok": False, "ci_pending": False, "reviewed": True, "head_shipped": True, "probes_passed": True},
    {"name": "ci-pending", "pr_open": True, "ci_ok": False, "ci_pending": True, "reviewed": True, "head_shipped": True, "probes_passed": True},
    {"name": "unreviewed", "pr_open": True, "ci_ok": True, "ci_pending": False, "reviewed": False, "head_shipped": True, "probes_passed": True},
    {"name": "wrong-head", "pr_open": True, "ci_ok": True, "ci_pending": False, "reviewed": True, "head_shipped": False, "probes_passed": True},
    {"name": "probe-failed", "pr_open": True, "ci_ok": True, "ci_pending": False, "reviewed": True, "head_shipped": True, "probes_passed": False},
]
for case in cases:
    snapshot = LegacyPRSnapshot(
        work_order_node_id="x-live",
        attempt_id="attempt-live",
        current_head="head-live",
        observed_at=now,
        fresh_until=now + dt.timedelta(minutes=5),
        source_revision="pr-live:head-live",
        fact_revision="pr-live-snapshot",
        **{key: value for key, value in case.items() if key != "name"},
    )
    shadow = adapt_legacy_pr(snapshot, evaluated_at=now)
    print(json.dumps({
        "input": case,
        "legacy_passed": shadow.legacy_passed,
        "reads": {row.evidence_id: row.result.value for row in shadow.verdict.requirements},
    }))
"#;
    let output = Command::new("uv")
        .args(["run", "--project"])
        .arg(repo.join("cli"))
        .args(["python", "-c", script])
        .output()
        .expect("run current-head Python PR adapter");
    assert!(
        output.status.success(),
        "adapter stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 7);
    for row in rows {
        let input = &row["input"];
        let pr_open = input["pr_open"].as_bool().unwrap();
        let ci_ok = input["ci_ok"].as_bool().unwrap();
        let ci_pending = input["ci_pending"].as_bool().unwrap();
        let reviewed = input["reviewed"].as_bool().unwrap();
        let head_shipped = input["head_shipped"].as_bool().unwrap();
        let probes_passed = input["probes_passed"].as_bool().unwrap();
        let authority = pr_passes(
            pr_open,
            ci_ok && !ci_pending,
            reviewed,
            head_shipped,
            probes_passed,
        );
        assert_eq!(row["legacy_passed"], json!(authority), "{row}");
        assert_eq!(
            row["reads"]["legacy-pr-open"],
            json!(if pr_open { "passed" } else { "failed" }),
            "{row}"
        );
        assert_eq!(
            row["reads"]["legacy-pr-ci"],
            json!(if ci_pending {
                "blocked"
            } else if ci_ok {
                "passed"
            } else {
                "failed"
            }),
            "{row}"
        );
        assert_eq!(
            row["reads"]["legacy-pr-review"],
            json!(if reviewed { "passed" } else { "blocked" }),
            "{row}"
        );
        assert_eq!(
            row["reads"]["legacy-pr-head"],
            json!(if head_shipped { "passed" } else { "failed" }),
            "{row}"
        );
        assert_eq!(
            row["reads"]["legacy-pr-probes"],
            json!(if probes_passed { "passed" } else { "failed" }),
            "{row}"
        );
    }
}

#[test]
fn generic_completion_ac_d7_hp_passed_canonical_verdict_terminates_without_gh() {
    let env = setup(&passed_response());

    let output = run(&env);

    assert_eq!(output["decision"], "allow", "{output}");
    assert_eq!(output["termination_reason"], "DoneDelivery");
    let message = output["message"].as_str().unwrap();
    assert!(
        message.contains("fno do delivery evaluate --json"),
        "{message}"
    );
    assert!(message.contains("sha256:abc"), "{message}");
    assert!(event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
    let events = fs::read_to_string(&env.events).unwrap();
    assert!(events.lines().any(|line| {
        let event: Value = serde_json::from_str(line).unwrap();
        event["type"] == "loop_check"
            && event["data"]["decision"] == "allow"
            && event["data"]["fact_revision"] == "sha256:abc"
    }));
    assert!(!events.lines().any(|line| {
        let event: Value = serde_json::from_str(line).unwrap();
        event["type"] == "termination" && event["data"]["reason"] == "DoneDelivery"
    }));
}

#[test]
fn generic_completion_missing_manifest_session_never_terminates() {
    let env = setup(&passed_response());
    fs::write(
        &env.state,
        "---\ncreated_at: 2026-08-02T12:00:00Z\nattended: true\nplan_path: plan.md\n---\ngraph_node_id: x-delivery\n",
    )
    .unwrap();

    let mut output = Value::Null;
    for _ in 0..6 {
        output = run(&env);
    }

    assert_eq!(output["decision"], "allow", "{output}");
    assert_eq!(output["termination_reason"], "NoProgress", "{output}");
    assert!(!event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
    let events = fs::read_to_string(&env.events).unwrap();
    assert!(!events.contains("DoneDelivery"));
}

#[test]
fn generic_completion_rejects_a_verdict_for_a_different_manifest_node() {
    let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
    response["verdict"]["work_order_node_id"] = json!("x-other");
    let env = setup(&response.to_string());

    let output = run(&env);

    assert_eq!(output["decision"], "block", "{output}");
    assert!(output["termination_reason"].is_null(), "{output}");
    assert!(!event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
}

#[test]
fn generic_completion_nonpassing_reaches_the_no_progress_backstop() {
    let env = setup("{not-json");

    let mut output = Value::Null;
    for _ in 0..6 {
        output = run(&env);
    }

    assert_eq!(output["decision"], "allow", "{output}");
    assert_eq!(output["termination_reason"], "NoProgress", "{output}");
    assert!(!event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
}

#[test]
fn generic_completion_passed_without_promise_reaches_the_no_progress_backstop() {
    let env = setup(&passed_response());
    fs::write(
        &env.transcript,
        "{\"message\":{\"role\":\"assistant\",\"content\":\"still working\"}}\n",
    )
    .unwrap();

    let mut output = Value::Null;
    for _ in 0..6 {
        output = run(&env);
    }

    assert_eq!(output["decision"], "allow", "{output}");
    assert_eq!(output["termination_reason"], "NoProgress", "{output}");
    assert!(!event_types(&env.events).contains(&"delivery_verdict_evaluated".to_string()));
}

#[test]
fn generic_completion_evidence_progress_resets_the_no_progress_streak() {
    let env = setup(&passed_response());
    fs::write(
        &env.transcript,
        "{\"message\":{\"role\":\"assistant\",\"content\":\"still working\"}}\n",
    )
    .unwrap();
    for _ in 0..4 {
        assert_eq!(run(&env)["decision"], "block");
    }
    let mut progressed: Value = serde_json::from_str(&passed_response()).unwrap();
    progressed["evidence_revision"] = json!("sha256:evidence-2");
    fs::write(
        &env.evaluator,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            progressed.to_string().replace('\'', "'\\''")
        ),
    )
    .unwrap();

    let output = run(&env);

    assert_eq!(output["decision"], "block", "{output}");
    assert!(output["termination_reason"].is_null(), "{output}");
    let output = run(&env);
    assert_eq!(output["decision"], "block", "{output}");
    assert!(output["termination_reason"].is_null(), "{output}");
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

#[test]
fn generic_completion_ac_d5_inv_valid_incomplete_declaration_uses_legacy_path() {
    let env = setup(&passed_response());
    fs::write(
        env.cwd.join("plan.md"),
        "---\nnode: x-delivery\nstatus: ready\ncreated: 2026-08-02\ncompletion: delivery\ncompany_work:\n  work_order:\n    node_id: x-delivery\n    attempt_id: attempt-1\n  deliverables:\n    - id: output\n      kind: arbitrary-output\n      work_order_id: x-delivery\n      attempt_id: attempt-1\n      required_evidence_ids: []\n---\n",
    )
    .unwrap();

    let output = run(&env);

    assert_eq!(output["termination_reason"], "DoneAdvisory");
}

#[test]
fn generic_completion_malformed_explicit_plan_never_falls_through_to_advisory() {
    let env = setup(&passed_response());
    fs::write(
        env.cwd.join("plan.md"),
        "---\ncompletion: delivery\ncompany_work: [\n---\n",
    )
    .unwrap();

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
}

#[test]
fn generic_completion_structurally_invalid_explicit_plan_never_falls_through_to_advisory() {
    let invalid_declarations = [
        "company_work: bogus\n",
        "company_work:\n  work_order: bogus\n  deliverables: []\n",
        "company_work:\n  work_order:\n    node_id: x-delivery\n  deliverables: []\n",
        "company_work:\n  work_order:\n    node_id: x-delivery\n    attempt_id: attempt-1\n  deliverables: bogus\n",
        "company_work:\n  work_order:\n    node_id: x-delivery\n    attempt_id: attempt-1\n  deliverables:\n    - id: output\n      required_evidence_ids: bogus\n",
    ];
    for declaration in invalid_declarations {
        let env = setup(&passed_response());
        fs::write(
            env.cwd.join("plan.md"),
            format!("---\ncompletion: delivery\n{declaration}---\n"),
        )
        .unwrap();

        let output = run(&env);

        assert_eq!(output["decision"], "block", "{declaration}: {output}");
        assert!(
            output["termination_reason"].is_null(),
            "{declaration}: {output}"
        );
    }
}

#[test]
fn generic_completion_ac_d6_arch_real_cli_evaluates_full_plan_and_journal() {
    let env = canonical_setup();

    let output = run(&env);

    assert_eq!(output["decision"], "allow", "{output}");
    assert_eq!(output["termination_reason"], "DoneDelivery");
}

#[test]
fn generic_completion_ac_d6_arch_dropped_requirement_stays_nonterminal() {
    let env = canonical_setup();
    let first = fs::read_to_string(&env.events)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    fs::write(&env.events, format!("{first}\n")).unwrap();

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
}

#[test]
fn generic_completion_ac_d6_arch_incomplete_pass_response_is_rejected() {
    let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
    response["verdict"]["required_requirements"] = json!([
        {"deliverable_id": "output", "evidence_id": "artifact-ready"},
        {"deliverable_id": "output", "evidence_id": "review-ready"}
    ]);
    let env = setup(&response.to_string());

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
}

#[test]
fn generic_completion_rejects_blank_strict_boundary_values() {
    let mutations: &[(&[&str], Value)] = &[
        (&["fact_revision"], json!("   ")),
        (&["evidence_revision"], json!("   ")),
        (&["verdict", "work_order_node_id"], json!("   ")),
        (&["verdict", "attempt_id"], json!("   ")),
        (
            &["verdict", "requirements", "0", "producers", "0"],
            json!("   "),
        ),
        (
            &["verdict", "requirements", "0", "source_revisions", "0"],
            json!("   "),
        ),
    ];
    for (path, value) in mutations {
        let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
        let mut current = &mut response;
        for segment in &path[..path.len() - 1] {
            current = if let Ok(index) = segment.parse::<usize>() {
                &mut current[index]
            } else {
                &mut current[*segment]
            };
        }
        let last = path[path.len() - 1];
        if let Ok(index) = last.parse::<usize>() {
            current[index] = value.clone();
        } else {
            current[last] = value.clone();
        }
        let env = setup(&response.to_string());

        let output = run(&env);

        assert_eq!(output["decision"], "block", "path={path:?}: {output}");
        assert!(
            output["termination_reason"].is_null(),
            "path={path:?}: {output}"
        );
    }
}

#[test]
fn generic_completion_active_plan_rejects_inactive_evaluator_response() {
    let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
    response["status"] = json!("inactive");
    response["fact_revision"] = Value::Null;
    response["evidence_revision"] = Value::Null;
    response["verdict"] = Value::Null;
    let env = setup(&response.to_string());

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
}

#[test]
fn generic_completion_rejects_outer_diagnostics_on_a_passed_verdict() {
    let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
    response["diagnostics"] = json!(["outer-only"]);
    let env = setup(&response.to_string());

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    assert!(output["termination_reason"].is_null());
    assert!(
        output["message"].as_str().unwrap().contains("outer-only"),
        "{output}"
    );
}

#[test]
fn generic_completion_surfaces_requirement_rejection_context() {
    let mut response: Value = serde_json::from_str(&passed_response()).unwrap();
    response["verdict"]["aggregate"] = json!("unknown");
    response["verdict"]["requirements"][0]["result"] = json!("unknown");
    response["verdict"]["requirements"][0]["diagnostics"] =
        json!(["stale after 2026-08-02T12:00:01Z"]);
    let env = setup(&response.to_string());

    let output = run(&env);

    assert_eq!(output["decision"], "block");
    let message = output["message"].as_str().unwrap();
    assert!(message.contains("output/artifact-ready"), "{message}");
    assert!(message.contains("adapter:test"), "{message}");
    assert!(
        message.contains("stale after 2026-08-02T12:00:01Z"),
        "{message}"
    );
}
