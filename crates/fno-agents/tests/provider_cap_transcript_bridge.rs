//! The provider-cap sweep resolves a live thread worker's transcript the way
//! `fno agents peek` does.
//!
//! A thread row carries only its 8-hex short id. The transcript is planted
//! where peek's resolver finds it - the store, named by the full uuid the
//! short id prefixes - with NO sessions-dir record, the shape that used to
//! read `transcript-not-found` while the lane walled. The binary's
//! `fno agents transcript-paths` child is stood in by a script that calls the
//! checkout's real `fno.provenance.resolver.resolve_transcript`, so the
//! resolution through the store is the production one, deploy-independent.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

const CLIENT: &str = env!("CARGO_BIN_EXE_fno-agents");
const CLI_SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli/src");

const SHORT: &str = "d8996f9b";
const UUID: &str = "d8996f9b-8854-4f22-8c28-c7819c6d0316";

const OK_LINE: &str = r#"{"type":"assistant","timestamp":"2026-09-22T07:00:00.000Z","message":{"role":"assistant","model":"glm-5.3-flash","content":[{"type":"text","text":"Running the tests now."}]}}"#;
const FOUR29_LINE: &str = r#"{"type":"assistant","timestamp":"2026-09-22T08:15:00.000Z","isApiErrorMessage":true,"message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"API Error: Request rejected (429) · [1308][Usage limit reached for 5 hour. Your limit will reset at 2026-09-22 09:41:13]"}]}}"#;

/// The stand-in `fno` binary: pins the child contract (argv head, python
/// runtime) and answers through the checkout's real resolver.
fn write_fake_fno(dir: &std::path::Path) -> PathBuf {
    let script = format!(
        r#"#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
argv = sys.argv[1:]
assert argv[:2] == ["agents", "transcript-paths"], argv
assert os.environ.get("FNO_AGENTS_RUNTIME") == "python", "the child must run the python runtime"
sys.path.insert(0, {cli_src:?})
from fno.provenance.resolver import resolve_transcript


def val(flag):
    return argv[argv.index(flag) + 1]


answer = {{}}
for sid in (v for v in val("--ids").split(",") if v):
    rt = resolve_transcript("claude", sid, "/", projects_root=Path(val("--projects-root")))
    answer[sid] = rt.transcript_path if rt.resolved and rt.transcript_path else None
print(json.dumps(answer))
"#,
        cli_src = CLI_SRC
    );
    let path = dir.join("fake-fno");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

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

    let fno = write_fake_fno(root);
    let output = Command::new(CLIENT)
        .args(["provider-cap", "status", "--json"])
        .env("FNO_AGENTS_HOME", &home)
        .env("FNO_BIN", &fno)
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
