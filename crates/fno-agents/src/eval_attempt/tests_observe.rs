//! `observe` door tests: identity + usage readback from the attempt's own
//! transcript store. Tempdir fixtures only; the real store is never touched.

use super::observe;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn observe_payload(lane: Value, extra: Value) -> Value {
    let mut p = json!({
        "lane": lane,
        "attempted": true,
        "spawned": true,
        "workdir": "/tmp/attempt-wd",
        "started_epoch": 1_700_000_000.0_f64,
        "now_epoch": 1_700_000_100.0_f64,
    });
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            p[k.as_str()] = v.clone();
        }
    }
    p
}

fn plant_claude(root: &Path, dir_name: &str, name: &str, body: &str, mtime: u64) -> PathBuf {
    let dir = root.join(dir_name);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime))
        .unwrap();
    p
}

fn claude_line(cwd: &str, model: &str, inp: u64, out: u64, cr: u64, cw: u64) -> String {
    json!({
        "type": "assistant",
        "cwd": cwd,
        "message": {"role": "assistant", "model": model,
                     "usage": {"input_tokens": inp, "output_tokens": out,
                                "cache_read_input_tokens": cr,
                                "cache_creation_input_tokens": cw}},
    })
    .to_string()
}

const WD: &str = "/tmp/attempt-wd";

fn observe_claude_finds_transcript_by_workdir_and_sums_usage() {
    let tmp = tempfile::TempDir::new().unwrap();
    let uuid = "d8996f9b-8854-4f22-8c28-c7819c6d0316";
    let body = format!(
        "{}\n{}\n{}\n",
        json!({"type": "user", "cwd": WD, "message": {"role": "user"}}),
        claude_line(WD, "glm-5.3-flash", 100, 20, 30, 5),
        claude_line(WD, "glm-5.3-flash", 50, 10, 0, 0),
    );
    plant_claude(
        tmp.path(),
        "-repo-wt",
        &format!("{uuid}.jsonl"),
        &body,
        1_700_000_050,
    );
    let lane =
        json!({"name": "glm", "harness": "claude", "model": "glm-5.3-flash", "effort": "high"});
    let out = observe(&observe_payload(
        lane,
        json!({"projects_root": tmp.path().to_str().unwrap()}),
    ));
    assert_eq!(out["lane_status"], "ok", "out: {out}");
    assert_eq!(out["observed_model"], "glm-5.3-flash");
    assert_eq!(out["observed_session_id"], uuid);
    assert_eq!(out["substituted"], false);
    assert_eq!(out["usage"]["input"], 150);
    assert_eq!(out["usage"]["output"], 30);
    assert_eq!(out["usage"]["cache_read"], 30);
    assert_eq!(out["usage"]["cache_write"], 5);
    assert_eq!(out["usage_source"], "claude-transcript");
}

fn observe_claude_no_transcript_reads_unverified_with_null_usage() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lane =
        json!({"name": "glm", "harness": "claude", "model": "glm-5.3-flash", "effort": "high"});
    let out = observe(&observe_payload(
        lane,
        json!({"projects_root": tmp.path().to_str().unwrap()}),
    ));
    assert_eq!(out["lane_status"], "unverified");
    assert!(
        out["lane_reason"]
            .as_str()
            .unwrap()
            .contains("no claude transcript"),
        "out: {out}"
    );
    assert_eq!(out["usage"], Value::Null);
    // A harness with no reader reads the same way.
    let lane = json!({"name": "x", "harness": "pi", "model": "m", "effort": "high"});
    let out = observe(&observe_payload(lane, json!({})));
    assert_eq!(out["lane_status"], "unverified");
    assert!(
        out["lane_reason"]
            .as_str()
            .unwrap()
            .contains("no transcript reader for harness 'pi'"),
        "out: {out}"
    );
    assert_eq!(out["usage"], Value::Null);
}

fn observe_claude_model_mismatch_is_substituted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let uuid = "d8996f9b-8854-4f22-8c28-c7819c6d0316";
    let body = format!(
        "{}\n{}\n",
        json!({"type": "user", "cwd": WD, "message": {"role": "user"}}),
        claude_line(WD, "claude-sonnet-5", 10, 2, 0, 0),
    );
    plant_claude(
        tmp.path(),
        "-repo-wt",
        &format!("{uuid}.jsonl"),
        &body,
        1_700_000_050,
    );
    let lane =
        json!({"name": "glm", "harness": "claude", "model": "glm-5.3-flash", "effort": "high"});
    let out = observe(&observe_payload(
        lane,
        json!({"projects_root": tmp.path().to_str().unwrap()}),
    ));
    assert_eq!(out["substituted"], true);
    assert_eq!(out["lane_status"], "substituted");
    assert_eq!(out["observed_model"], "claude-sonnet-5");
}

fn observe_statuses_before_any_worker() {
    let lane = json!({"name": "x", "harness": "claude", "model": "m", "effort": "high"});
    let mut p = observe_payload(lane.clone(), json!({}));
    p["attempted"] = json!(false);
    assert_eq!(observe(&p)["lane_status"], "not-applicable");
    let mut q = observe_payload(lane, json!({}));
    q["spawned"] = json!(false);
    let out = observe(&q);
    assert_eq!(out["lane_status"], "unavailable");
    assert_eq!(out["usage"], Value::Null);
}

fn observe_opencode_reads_session_and_sums_tokens() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("opencode.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, time_updated INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES ('s1', NULL, '/tmp/attempt-wd', 1700000050000)",
        [],
    )
    .unwrap();
    let msg = r#"{"role":"assistant","providerID":"zai","modelID":"glm-5.2","tokens":{"input":10,"output":4,"reasoning":3,"cache":{"read":6,"write":1}}}"#;
    conn.execute(
        "INSERT INTO message VALUES ('m1', 's1', 1700000050000, ?1)",
        [msg],
    )
    .unwrap();
    let lane =
        json!({"name": "glm", "harness": "opencode", "model": "zai/glm-5.2", "effort": "high"});
    let out = observe(&observe_payload(
        lane,
        json!({"opencode_dbs": [db.to_str().unwrap()]}),
    ));
    // The lane names provider/model and the store splits them: still ok.
    assert_eq!(out["lane_status"], "ok", "out: {out}");
    assert_eq!(out["observed_model"], "zai/glm-5.2");
    assert_eq!(out["observed_session_id"], "s1");
    assert_eq!(out["usage"]["input"], 10);
    // Reasoning joins output.
    assert_eq!(out["usage"]["output"], 7);
    assert_eq!(out["usage"]["cache_read"], 6);
    assert_eq!(out["usage"]["cache_write"], 1);
    assert_eq!(out["usage_source"], "opencode-store");
}

#[test]
fn all_contracts_in_one_declaration() {
    observe_claude_finds_transcript_by_workdir_and_sums_usage();
    observe_claude_no_transcript_reads_unverified_with_null_usage();
    observe_claude_model_mismatch_is_substituted();
    observe_statuses_before_any_worker();
    observe_opencode_reads_session_and_sums_tokens();
}
