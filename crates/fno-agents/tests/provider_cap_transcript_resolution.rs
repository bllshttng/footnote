//! The provider-cap sweep resolves a live thread worker's transcript the way
//! `fno agents peek` does - natively, in the binary.
//!
//! A thread row carries only its 8-hex short id. The transcript is planted
//! where peek's resolver finds it - the claude projects store, named by the
//! full uuid the short id prefixes - with NO sessions-dir record, the shape
//! that used to read `transcript-not-found` while the lane walled.

use serde_json::Value;
use std::process::Command;

const CLIENT: &str = env!("CARGO_BIN_EXE_fno-agents");

const SHORT: &str = "d8996f9b";
const UUID: &str = "d8996f9b-8854-4f22-8c28-c7819c6d0316";

const OK_LINE: &str = r#"{"type":"assistant","timestamp":"2026-09-22T07:00:00.000Z","message":{"role":"assistant","model":"glm-5.3-flash","content":[{"type":"text","text":"Running the tests now."}]}}"#;
const FOUR29_LINE: &str = r#"{"type":"assistant","timestamp":"2026-09-22T08:15:00.000Z","isApiErrorMessage":true,"message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"API Error: Request rejected (429) · [1308][Usage limit reached for 5 hour. Your limit will reset at 2026-09-22 09:41:13]"}]}}"#;

#[test]
fn thread_row_resolves_its_transcript_where_peek_finds_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();

    let projects = root.join("projects");
    let repo_dir = projects.join("-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join(format!("{UUID}.jsonl")),
        format!("{OK_LINE}\n{FOUR29_LINE}\n"),
    )
    .unwrap();

    let home = root.join("agents");
    let registry = format!(
        r#"{{"schema_version":25,"agents":[{{"name":"w-d899","short_id":"{SHORT}","harness":"claude","provider":"zai","launch_account":"default","state":"working"}}]}}"#
    );
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("registry.json"), registry).unwrap();

    let output = Command::new(CLIENT)
        .args(["provider-cap", "status", "--json"])
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_AGENTS_HOME", &home)
        .env("FNO_CLAUDE_PROJECTS_DIR", &projects)
        .env("FNO_AGENTS_RUNTIME", "rust")
        .output()
        .expect("fno-agents status runs");
    assert!(
        output.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("status JSON");

    let lanes = status["lanes"].as_array().expect("lanes");
    let member = &lanes
        .iter()
        .flat_map(|l| l["members"].as_array().unwrap())
        .find(|m| m["name"] == "w-d899")
        .expect("the thread row's member");
    assert_eq!(
        member["capped"], true,
        "the planted 429 must read capped, member={member}"
    );
    assert_eq!(
        member["cap_unknown"],
        Value::Null,
        "a resolved transcript is measured, member={member}"
    );
    assert!(
        member["transcript"]
            .as_str()
            .is_some_and(|t| t.ends_with(&format!("{UUID}.jsonl"))),
        "member={member}"
    );
}
