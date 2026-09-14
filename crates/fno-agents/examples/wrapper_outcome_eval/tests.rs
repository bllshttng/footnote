use super::*;
use sha2::{Digest, Sha256};
use std::fs;
use tempfile::TempDir;

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn artifact(root: &Path, path: &str, text: &str) -> Value {
    let dest = root.join(path);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&dest, text).unwrap();
    json!({"path": path, "sha256": hash(text.as_bytes())})
}

fn corpus() -> (TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    let runtime =
        json!({"harness":"codex","model":"fixed-model","effort":"high","budget_seconds":600});
    let mut runs = vec![];
    let mut tasks = vec![];
    for id in ["empty-result", "selected-result"] {
        let input = format!("Return the recorded result for {id}.");
        let expected = json!({"task":id,"count":if id=="empty-result" {0} else {2}});
        tasks.push(json!({"id":id,"input_sha256":hash(input.as_bytes()),"expected":expected}));
        for arm in ["baseline", "candidate"] {
            for repeat in 1..=3 {
                let prefix = format!("{arm}/{id}/{repeat}");
                let input_ref = artifact(dir.path(), &format!("{prefix}/input.txt"), &input);
                let result_ref = artifact(
                    dir.path(),
                    &format!("{prefix}/result.json"),
                    &expected.to_string(),
                );
                let scratch_dir = format!("{prefix}/scratch");
                fs::create_dir_all(dir.path().join(&scratch_dir)).unwrap();
                let scratch = if arm == "baseline" {
                    let text = "import json\nprint(json.loads(raw))\n";
                    let record = artifact(dir.path(), &format!("{scratch_dir}/wrap.py"), text);
                    vec![json!({"path":"wrap.py","sha256":record["sha256"]})]
                } else {
                    vec![]
                };
                runs.push(json!({
                    "task_id":id,"repeat":repeat,"arm":arm,
                    "attempt_id":prefix,"started_at":"2026-09-12T01:00:00Z",
                    "revision":if arm=="baseline" {"a".repeat(40)} else {"b".repeat(40)},
                    "runtime":runtime,"termination":"completed","exit_code":0,
                    "input":input_ref,"result":result_ref,"capture_complete":true,
                    "scratch_dir":scratch_dir,"scratch":scratch
                }));
            }
        }
    }
    let manifest = json!({
        "schema_version":1,"kind":"calibration","cohort_id":"wrapper-calibration",
        "declared_at":"2026-09-12T00:00:00Z","intervention":"Return one JSON document on stdout",
        "baseline_revision":"a".repeat(40),"candidate_revision":"b".repeat(40),
        "runtime":runtime,"repetitions":3,"tasks":tasks,"runs":runs
    });
    (dir, manifest)
}

#[test]
fn fewer_wrappers_with_correct_outputs_has_exact_denominators() {
    let (dir, m) = corpus();
    let r = evaluate(dir.path(), &m).unwrap();
    assert_eq!(r["comparison"], "improved");
    assert_eq!(r["baseline"]["attempts"], 6);
    assert_eq!(r["baseline"]["wrappers"], 6);
    assert_eq!(r["candidate"]["completed"], 6);
    assert_eq!(r["candidate"]["wrappers_per_attempt"], 0.0);
    assert_eq!(r["tasks"].as_array().unwrap().len(), 2);
}

#[test]
fn calibration_never_claims_live_benefit() {
    let (dir, m) = corpus();
    let r = evaluate(dir.path(), &m).unwrap();
    assert_eq!(r["verdict"], "calibration_only");
    assert_eq!(r["live_benefit_proven"], false);
}

#[test]
fn lower_completion_is_regression_even_with_no_wrappers() {
    let (dir, mut m) = corpus();
    let run = &mut m["runs"][3];
    run["result"] = artifact(
        dir.path(),
        run["result"]["path"].as_str().unwrap(),
        "{\"wrong\":true}",
    );
    let r = evaluate(dir.path(), &m).unwrap();
    assert_eq!(r["comparison"], "regressed");
    assert_eq!(r["candidate"]["attempts"], 6);
    assert_eq!(r["candidate"]["completed"], 5);
}

#[test]
fn absent_and_duplicate_attempts_are_insufficient() {
    let (dir, m) = corpus();
    let mut absent = m.clone();
    absent["runs"].as_array_mut().unwrap().pop();
    assert!(evaluate(dir.path(), &absent)
        .unwrap_err()
        .contains("missing attempt"));
    let mut dup = m.clone();
    let run = dup["runs"][0].clone();
    dup["runs"].as_array_mut().unwrap().push(run);
    assert!(evaluate(dir.path(), &dup)
        .unwrap_err()
        .contains("duplicate"));
}

#[test]
fn runtime_revision_input_and_time_mismatches_are_rejected() {
    let (dir, m) = corpus();
    for (pointer, value, expected) in [
        ("/runs/0/runtime/model", json!("different-model"), "runtime"),
        ("/runs/0/revision", json!("c".repeat(40)), "revision"),
        ("/runs/0/input/sha256", json!("0".repeat(64)), "hash"),
        ("/declared_at", json!("2026-09-13T00:00:00Z"), "declaration"),
    ] {
        let mut bad = m.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            evaluate(dir.path(), &bad).unwrap_err().contains(expected),
            "{pointer}"
        );
    }
}

#[test]
fn incomplete_or_altered_capture_is_rejected() {
    let (dir, mut m) = corpus();
    m["runs"][0]["capture_complete"] = json!(false);
    assert!(evaluate(dir.path(), &m).unwrap_err().contains("capture"));
    m["runs"][0]["capture_complete"] = json!(true);
    fs::write(
        dir.path().join("baseline/empty-result/1/scratch/wrap.py"),
        "changed",
    )
    .unwrap();
    assert!(evaluate(dir.path(), &m).unwrap_err().contains("hash"));
}

#[test]
fn unlisted_scratch_is_not_counted_as_zero() {
    let (dir, m) = corpus();
    fs::write(
        dir.path()
            .join("candidate/empty-result/1/scratch/unlisted.py"),
        "json.loads(raw)",
    )
    .unwrap();
    assert!(evaluate(dir.path(), &m).unwrap_err().contains("inventory"));
}

#[test]
fn escaping_and_reused_evidence_is_rejected() {
    let (dir, m) = corpus();
    let mut escape = m.clone();
    escape["runs"][0]["input"]["path"] = json!("../outside.txt");
    assert!(evaluate(dir.path(), &escape).unwrap_err().contains("path"));
    let mut reused = m.clone();
    reused["runs"][1]["scratch_dir"] = reused["runs"][0]["scratch_dir"].clone();
    assert!(evaluate(dir.path(), &reused)
        .unwrap_err()
        .contains("reused"));
}

#[test]
fn failure_and_abandonment_remain_in_denominator() {
    let (dir, mut m) = corpus();
    m["runs"][3]["termination"] = json!("abandoned");
    m["runs"][3]["exit_code"] = json!(1);
    m["runs"][3]["result"] = Value::Null;
    let r = evaluate(dir.path(), &m).unwrap();
    assert_eq!(r["candidate"]["attempts"], 6);
    assert_eq!(r["candidate"]["completed"], 5);
    assert_eq!(r["comparison"], "regressed");
}

#[test]
fn all_failed_and_no_baseline_signal_never_improve() {
    let (dir, mut m) = corpus();
    for run in m["runs"].as_array_mut().unwrap() {
        run["termination"] = json!("task_failed");
        run["exit_code"] = json!(1);
        run["result"] = Value::Null;
    }
    assert_eq!(
        evaluate(dir.path(), &m).unwrap()["comparison"],
        "insufficient"
    );
    let (dir, mut m) = corpus();
    for run in m["runs"].as_array_mut().unwrap() {
        for file in run["scratch"].as_array().unwrap() {
            fs::remove_file(
                dir.path()
                    .join(run["scratch_dir"].as_str().unwrap())
                    .join(file["path"].as_str().unwrap()),
            )
            .unwrap();
        }
        run["scratch"] = json!([]);
    }
    assert_eq!(
        evaluate(dir.path(), &m).unwrap()["comparison"],
        "insufficient"
    );
}

#[test]
fn unchanged_wrappers_are_not_improvement() {
    let (dir, mut m) = corpus();
    for run in m["runs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter(|r| r["arm"] == "candidate")
    {
        let text = "json.loads(raw)\n";
        let path = format!("{}/wrap.py", run["scratch_dir"].as_str().unwrap());
        let a = artifact(dir.path(), &path, text);
        run["scratch"] = json!([{"path":"wrap.py","sha256":a["sha256"]}]);
    }
    assert_eq!(evaluate(dir.path(), &m).unwrap()["comparison"], "unchanged");
}

#[cfg(unix)]
#[test]
fn symlinked_evidence_is_rejected() {
    let (dir, m) = corpus();
    let path = dir.path().join("baseline/empty-result/1/input.txt");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("../../2/input.txt", &path).unwrap();
    assert!(evaluate(dir.path(), &m).unwrap_err().contains("symlink"));
}

#[test]
fn report_pins_evaluator_and_classifier_bytes() {
    let (dir, m) = corpus();
    let r = evaluate(dir.path(), &m).unwrap();
    assert_eq!(r["classifier_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(r["evaluator_sha256"].as_str().map(str::len), Some(64));
}

#[test]
fn malformed_or_missing_result_is_insufficient() {
    let (dir, mut m) = corpus();
    m["runs"][3]["result"] = artifact(dir.path(), "candidate/bad.json", "{bad");
    assert!(evaluate(dir.path(), &m)
        .unwrap_err()
        .contains("not valid JSON"));
    m["runs"][3]["result"] = Value::Null;
    assert!(evaluate(dir.path(), &m)
        .unwrap_err()
        .contains("missing result"));
}

#[test]
fn one_task_regression_cannot_hide_inside_an_aggregate_gain() {
    let (dir, mut m) = corpus();
    let run = &mut m["runs"][3];
    let folder = run["scratch_dir"].as_str().unwrap().to_string();
    let text = "json.loads(raw)\n";
    let mut captures = vec![];
    for n in 0..4 {
        let name = format!("wrap{n}.py");
        artifact(dir.path(), &format!("{folder}/{name}"), text);
        captures.push(json!({"path":name,"sha256":hash(text.as_bytes())}));
    }
    run["scratch"] = json!(captures);
    let r = evaluate(dir.path(), &m).unwrap();
    assert_eq!(r["baseline"]["wrappers"], 6);
    assert_eq!(r["candidate"]["wrappers"], 4);
    assert_eq!(r["comparison"], "regressed");
}
