#![cfg(unix)]
//! grok's Stop envelope through the real `fno-agents hook stop` binary.
//!
//! Payloads replay the recorded grok 1.0.34 contract
//! (tests/fixtures/grok-stop-trials.txt). The live continuation run is
//! blocked machine-wide (no grok login), so these pin the envelope handling
//! only; the capability row stays "extension" until a live run re-measures.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const SID: &str = "0198abcd-1234-5678-9abc-def012345678";

fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    let _ = fs::set_permissions(&path, perms);
    path
}

fn no_pr_gh(dir: &Path) -> PathBuf {
    make_script(
        dir,
        "gh",
        r#"if [ "$1" = "--version" ]; then echo 'gh version 2.x'; exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo 'no pull requests found for branch' >&2
  exit 1
fi
exit 1"#,
    )
}

struct GrokFixture {
    _tmp: TempDir,
    repo: PathBuf,
    events: PathBuf,
    gh_dir: PathBuf,
    grok_home: PathBuf,
}

fn fixture(_tag: &str, manifest_body: &str) -> GrokFixture {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".fno")).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    fs::write(repo.join("f.txt"), "one\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "c1"
        ])
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    let spaces = tmp.path().join("spaces");
    let slug = fno_agents::paths::space_slug(&fs::canonicalize(&repo).unwrap());
    let space = spaces.join(slug);
    fs::create_dir_all(&space).unwrap();
    fs::write(space.join("target-state.md"), manifest_body).unwrap();
    let events = space.join("events.jsonl");
    let gh_dir = tmp.path().join("bin");
    fs::create_dir_all(&gh_dir).unwrap();
    no_pr_gh(&gh_dir);
    let grok_home = tmp.path().join("grok");
    fs::create_dir_all(grok_home.join("sessions").join("%2Frepo")).unwrap();
    GrokFixture {
        _tmp: tmp,
        repo,
        events,
        gh_dir,
        grok_home,
    }
}

fn fire(fx: &GrokFixture, payload: &str) -> (i32, String, String) {
    let bin = env!("CARGO_BIN_EXE_fno-agents");
    let spaces = fx._tmp.path().join("spaces");
    let home = fx._tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("hook")
        .arg("stop")
        .envs(fno_agents::test_run::self_owner_env())
        .env("FNO_SPACES_DIR", &spaces)
        .env("GROK_HOME", &fx.grok_home)
        .env("GROK_SESSION_ID", SID)
        .env("CLAUDE_PLUGIN_ROOT", fx.gh_dir.join("root"))
        .env("HOME", &home)
        .env(
            "PATH",
            format!(
                "{}:{}",
                fx.gh_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .current_dir(&fx.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn seed_store(fx: &GrokFixture, text: &str) {
    let file = fx
        .grok_home
        .join("sessions")
        .join("%2Frepo")
        .join(SID)
        .join("chat_history.jsonl");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let line = serde_json::json!({ "text": text });
    fs::write(&file, serde_json::to_string(&line).unwrap() + "\n").unwrap();
}

fn last_loop_check(events: &Path) -> Option<serde_json::Value> {
    let content = fs::read_to_string(events).ok()?;
    content
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("loop_check"))
}

fn grok_payload(repo: &Path, extra: serde_json::Value, lam: &str) -> String {
    let mut body = serde_json::json!({
        "hookEventName": "stop",
        "sessionId": SID,
        "cwd": repo.to_str().unwrap(),
        "workspaceRoot": repo.to_str().unwrap(),
        "permissionMode": "default",
        "promptId": "prompt-1",
        "stopHookActive": false,
        "lastAssistantMessage": lam,
        "timestamp": "2026-09-18T00:00:00Z"
    });
    for (k, v) in extra.as_object().into_iter().flatten() {
        body[k] = v.clone();
    }
    body.to_string()
}

const ADV_MANIFEST: &str = "---\nsession_id: sess-grok\nharness_session_id: 0198abcd-1234-5678-9abc-def012345678\nno_ship: true\n---\n";
const OWNED_MANIFEST: &str =
    "---\nsession_id: sess-grok-b\nharness_session_id: 0198abcd-1234-5678-9abc-def012345678\n---\n";

/// AC2-HP: the promise rides ONLY grok's lastAssistantMessage; the store
/// file lacks it, so an advisory allow proves the payload intent channel.
#[test]
fn grok_fire_with_a_promise_terminates_advisory_through_the_payload() {
    let fx = fixture("adv", ADV_MANIFEST);
    seed_store(&fx, "wrapping up the run");
    let payload = grok_payload(
        &fx.repo,
        serde_json::json!({}),
        "all done <promise>MISSION COMPLETE: shipped</promise>",
    );
    let (code, stdout, stderr) = fire(&fx, &payload);
    assert_eq!(code, 0, "{stdout} {stderr}");
    // The allow path prints nothing; the block path is the one that speaks.
    assert!(stdout.trim().is_empty(), "allow must be silent: {stdout}");
    let journal = fs::read_to_string(&fx.events).unwrap_or_default();
    let row = last_loop_check(&fx.events)
        .unwrap_or_else(|| panic!("a loop_check row: journal={journal:?} stderr={stderr}"));
    assert_eq!(
        row.pointer("/data/decision").and_then(|s| s.as_str()),
        Some("allow"),
        "{row}"
    );
    assert_eq!(
        row.pointer("/data/intent_source").and_then(|s| s.as_str()),
        Some("payload"),
        "{row}"
    );
}

/// AC2-HP: with no PR in the world, the gate blocks through grok's shape.
#[test]
fn grok_fire_without_a_pr_blocks_like_any_session() {
    let fx = fixture("nopr", OWNED_MANIFEST);
    seed_store(&fx, "still working, nothing to report");
    let payload = grok_payload(&fx.repo, serde_json::json!({}), "still working");
    let (code, stdout, stderr) = fire(&fx, &payload);
    assert_eq!(code, 0, "{stdout} {stderr}");
    let d: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
    assert_eq!(d["decision"], "block", "{d}");
    let journal = fs::read_to_string(&fx.events).unwrap_or_default();
    assert!(
        last_loop_check(&fx.events).is_some(),
        "decide ran: journal={journal:?} stderr={stderr}"
    );
}

/// AC2-ERR: a subagent's stop is not the session's turn gate.
#[test]
fn grok_subagent_stop_is_skipped() {
    let fx = fixture("sub", OWNED_MANIFEST);
    let payload = grok_payload(
        &fx.repo,
        serde_json::json!({ "subagentType": "worker" }),
        "text",
    );
    let (code, stdout, stderr) = fire(&fx, &payload);
    assert_eq!(code, 0, "{stdout} {stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    assert!(last_loop_check(&fx.events).is_none());
}

/// AC2-ERR: no promptId is the session-end, observe-only fire.
#[test]
fn grok_session_end_stop_is_skipped() {
    let fx = fixture("end", OWNED_MANIFEST);
    let payload = grok_payload(
        &fx.repo,
        serde_json::json!({ "promptId": serde_json::Value::Null }),
        "text",
    );
    // Remove the key structurally: a string replace would silently depend on
    // where serde happens to serialize it.
    let mut v: serde_json::Value = serde_json::from_str(&payload).unwrap();
    v.as_object_mut().unwrap().remove("promptId");
    let payload = v.to_string();
    let (code, stdout, stderr) = fire(&fx, &payload);
    assert_eq!(code, 0, "{stdout} {stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    assert!(last_loop_check(&fx.events).is_none());
}

/// AC3-ERR: an owned grok session with no store entry gets the bounded
/// unavailable block, and stderr names the store reading.
#[test]
fn grok_missing_store_blocks_unavailable_and_names_the_reading() {
    let fx = fixture("miss", OWNED_MANIFEST);
    let payload = grok_payload(&fx.repo, serde_json::json!({}), "text");
    let (code, stdout, stderr) = fire(&fx, &payload);
    assert!(stderr.contains("grok session store read for"), "{stderr}");
    assert!(stderr.contains("none"), "{stderr}");
    let d: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
    assert_eq!(d["decision"], "block", "{d}");
    let reason = d["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("checker unavailable"), "{d}");
    assert_eq!(code, 0);
}

/// AC3-ERR: two store directories for one id also refuse.
#[test]
fn grok_duplicate_store_blocks_unavailable_and_names_the_reading() {
    let fx = fixture("dup", OWNED_MANIFEST);
    for group in ["%2Frepo-a", "%2Frepo-b"] {
        let dir = fx.grok_home.join("sessions").join(group).join(SID);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("chat_history.jsonl"), "{}\n").unwrap();
    }
    let payload = grok_payload(&fx.repo, serde_json::json!({}), "text");
    let (code, stdout, stderr) = fire(&fx, &payload);
    assert!(stderr.contains("duplicate"), "{stderr}");
    let d: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
    assert_eq!(d["decision"], "block", "{d}");
    assert_eq!(code, 0);
}
