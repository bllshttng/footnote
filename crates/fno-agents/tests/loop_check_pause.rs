//! Pause-all integration coverage for both loop-check drivers.

use fno_agents::loopcheck::run_loop_check_capture;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Debug, serde::Deserialize)]
struct Decision {
    decision: String,
    termination_reason: Option<String>,
    message: String,
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

fn setup(cwd: &Path, home: &Path) {
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::create_dir_all(home.join(".fno")).unwrap();
    fs::write(
        cwd.join(".fno/config.toml"),
        "[review]\nrequired_bots = [\"chatgpt-codex-connector\"]\n",
    )
    .unwrap();
    std::env::set_var("FNO_NUDGE_DISABLED", "1");
    std::env::set_var("FNO_LOOPCHECK_MIN_FIRE_GAP_SECS", "0");
    std::env::set_var("HOME", home);
}

fn fire(args: &[&str]) -> Decision {
    let mut owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    owned.extend([
        "--global-settings".to_string(),
        "/nonexistent/global-settings.yaml".to_string(),
        "--global-events".to_string(),
        "/nonexistent/global-events.jsonl".to_string(),
        "--author-harness".to_string(),
        "none".to_string(),
    ]);
    let (_, output) = run_loop_check_capture(&owned);
    serde_json::from_str(&output).unwrap_or_else(|error| panic!("{error}: {output}"))
}

#[test]
fn paused_target_and_king_allow_without_terminal_reason() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    setup(tmp.path(), &home);
    fs::write(
        home.join(".fno/loops-paused.json"),
        r#"{"who":"operator","paused_at":10}"#,
    )
    .unwrap();

    for driver in ["target", "king"] {
        let state = tmp.path().join(format!("{driver}-state.md"));
        let transcript = tmp.path().join(format!("{driver}-transcript.jsonl"));
        let decision = fire(&[
            "loop-check",
            "--state",
            state.to_str().unwrap(),
            "--transcript",
            transcript.to_str().unwrap(),
            "--cwd",
            tmp.path().to_str().unwrap(),
            "--driver",
            driver,
        ]);
        assert_eq!(decision.decision, "allow");
        assert!(decision.termination_reason.is_none());
        assert!(decision.message.contains("operator"));
    }
}

#[test]
fn clear_loop_check_keeps_normal_manifest_decision() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    setup(tmp.path(), &home);
    let bin_dir = tmp.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let gh = script(
        &bin_dir,
        "gh",
        r#"case "$*" in
  *--version*) echo 'gh version 2.x' ;;
  *headRefName*) echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeef"}' ;;
  *checks*) echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]' ;;
  *reviews*) echo '{"reviews":[],"comments":[]}' ;;
  *pulls/*) echo '[]' ;;
  *) exit 1 ;;
esac"#,
    );
    let git = script(&bin_dir, "git", "echo deadbeef");
    let state = tmp.path().join("state.md");
    let transcript = tmp.path().join("transcript.jsonl");
    fs::write(
        &state,
        "---\nsession_id: clear-test\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\n---\n",
    )
    .unwrap();
    fs::write(&transcript, "{}").unwrap();
    let decision = fire(&[
        "loop-check",
        "--state",
        state.to_str().unwrap(),
        "--transcript",
        transcript.to_str().unwrap(),
        "--cwd",
        tmp.path().to_str().unwrap(),
        "--gh-bin",
        gh.to_str().unwrap(),
        "--git-bin",
        git.to_str().unwrap(),
    ]);
    assert_eq!(decision.decision, "block");
}
