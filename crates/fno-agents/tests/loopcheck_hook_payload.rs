#![cfg(unix)]
//! End-to-end tests for the Stop-hook payload intent channel (ab-223d2dae).
//!
//! These spawn the REAL `fno-agents` binary with a piped stdin - the exact
//! shape `hooks/target-stop-hook.sh` produces - because the in-process
//! harness in loop_check.rs cannot exercise a stdin read. They pin:
//!
//! 1. AC2-HP: a `<promise>` present ONLY in the stdin payload (not yet
//!    flushed to the transcript file) is detected at that same fire, with
//!    `intent_source: "payload"` in the loop_check event.
//! 2. AC2-ERR: malformed stdin degrades to the transcript scan
//!    (`intent_source: "transcript"`), never an error.
//! 3. AC1-ERR: an old-shim invocation (no flag, no piped payload) behaves
//!    exactly as before - no stdin read, no hang.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn make_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    // Probe-exec until the script actually runs. A parallel test's fork can
    // inherit the just-written fd (CLOEXEC closes it only at the CHILD's
    // exec), so the verb under test exec'ing this script can hit ETXTBSY,
    // read the mock as "unavailable", and degrade fail-open - the
    // ac1_fr/ac2_edge/ac5_hp "allow where block expected" CI flake family.
    // Mock bodies are side-effect-free echoes, so one probe run is harmless;
    // any non-ETXTBSY outcome (nonzero exits included) proves exec works.
    for _ in 0..100 {
        match std::process::Command::new(&path)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            _ => break,
        }
    }
    path
}

/// gh mock: green PR world (mirrors loop_check.rs MockBins::green).
fn green_bins(dir: &Path) -> (PathBuf, PathBuf) {
    let gh = make_script(
        dir,
        "gh",
        r#"
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
"#,
    );
    let git = make_script(
        dir,
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000001""#,
    );
    (gh, git)
}

struct Fixture {
    _tmp: TempDir,
    cwd: PathBuf,
    manifest: PathBuf,
    transcript: PathBuf,
    events: PathBuf,
    gh: PathBuf,
    git: PathBuf,
}

/// A session whose transcript does NOT contain a promise: the only way
/// intent can be read is the stdin payload (or its absence).
fn fixture_with_manifest(manifest_body: &str) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().to_path_buf();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::write(cwd.join(".fno/config.toml"), "# isolated test settings\n").unwrap();

    let manifest = cwd.join("target-state.md");
    fs::write(&manifest, manifest_body).unwrap();

    // Transcript: assistant text WITHOUT any tag (the flush-race shape: the
    // promise-bearing final message has not landed in the file yet).
    let transcript = cwd.join("transcript.jsonl");
    let line = serde_json::json!({
        "message": {"role": "assistant", "content": "wrapping up the run"}
    });
    fs::write(&transcript, serde_json::to_string(&line).unwrap() + "\n").unwrap();

    let events = cwd.join(".fno/events.jsonl");
    let (gh, git) = green_bins(tmp.path());
    Fixture {
        _tmp: tmp,
        cwd,
        manifest,
        transcript,
        events,
        gh,
        git,
    }
}

/// Advisory (no_ship) variant: promise short-circuits to DoneAdvisory
/// without needing the done() world-reads.
fn fixture() -> Fixture {
    fixture_with_manifest(
        "---\nsession_id: sess-payload\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\nno_ship: true\n---\n",
    )
}

/// Spawn the real binary. `stdin_payload: Some(s)` pipes `s` and passes
/// `--hook-input-stdin`; `None` mimics an old shim (no flag, stdin null).
fn spawn_loop_check(fx: &Fixture, stdin_payload: Option<&str>) -> (i32, serde_json::Value) {
    let bin = env!("CARGO_BIN_EXE_fno-agents");
    let mut cmd = Command::new(bin);
    cmd.arg("loop-check")
        .arg("--state")
        .arg(&fx.manifest)
        .arg("--transcript")
        .arg(&fx.transcript)
        .arg("--cwd")
        .arg(&fx.cwd)
        .arg("--now")
        .arg("2026-06-05T00:10:00Z")
        .arg(format!("--gh-bin={}", fx.gh.display()))
        .arg(format!("--git-bin={}", fx.git.display()))
        .arg("--events")
        .arg(&fx.events)
        .arg("--global-events")
        .arg("/dev/null")
        .arg("--global-settings")
        .arg("/nonexistent/global-settings.yaml")
        // ab-098967b4: disable the P2 inbox-nudge shell-out so the e2e block
        // path does not spawn `fno agents nudge-peek` (latency + real-bus side
        // effects); the nudge enrichment is unit-tested separately.
        .env("FNO_NUDGE_DISABLED", "1")
        .current_dir(&fx.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = match stdin_payload {
        Some(payload) => {
            cmd.arg("--hook-input-stdin").stdin(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        }
        None => {
            cmd.stdin(Stdio::null());
            cmd.spawn().unwrap().wait_with_output().unwrap()
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let decision: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "non-JSON stdout: {stdout}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap_or(-1), decision)
}

/// Last loop_check event's intent_source from the events file.
fn last_intent_source(events: &Path) -> Option<String> {
    let content = fs::read_to_string(events).ok()?;
    content
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("loop_check"))
        .and_then(|v| {
            v.pointer("/data/intent_source")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
}

/// AC2-HP (e2e): promise visible at its own fire via the payload, even though
/// the transcript file does not carry it yet. Advisory unit -> DoneAdvisory.
#[test]
fn payload_promise_detected_at_own_fire() {
    let fx = fixture();
    let payload = serde_json::json!({
        "transcript_path": fx.transcript.to_str().unwrap(),
        "last_assistant_message": "all done <promise>MISSION COMPLETE: shipped</promise>"
    })
    .to_string();

    let (code, d) = spawn_loop_check(&fx, Some(&payload));
    assert_eq!(code, 0, "decision: {d}");
    assert_eq!(d["decision"], "allow", "decision: {d}");
    assert_eq!(d["termination_reason"], "DoneAdvisory", "decision: {d}");
    assert_eq!(
        last_intent_source(&fx.events).as_deref(),
        Some("payload"),
        "loop_check event must attribute the intent to the payload channel"
    );
}

/// AC2-ERR (e2e): malformed stdin degrades to the transcript scan; with no
/// tag anywhere the fire blocks normally and the event says "transcript".
#[test]
fn malformed_payload_falls_back_to_transcript() {
    let fx = fixture();
    let (code, d) = spawn_loop_check(&fx, Some("not json {{{"));
    assert_eq!(code, 0, "decision: {d}");
    assert_eq!(d["decision"], "block", "decision: {d}");
    assert_eq!(
        last_intent_source(&fx.events).as_deref(),
        Some("transcript"),
        "malformed payload must fall back to the transcript channel"
    );
}

/// AC1-ERR (e2e): old-shim shape - no flag, no payload. The binary must not
/// touch stdin (no hang with a null stdin) and reads the transcript as today.
#[test]
fn old_shim_without_flag_unchanged() {
    let fx = fixture();
    let (code, d) = spawn_loop_check(&fx, None);
    assert_eq!(code, 0, "decision: {d}");
    assert_eq!(d["decision"], "block", "decision: {d}");
    assert_eq!(
        last_intent_source(&fx.events).as_deref(),
        Some("transcript"),
        "flag-less invocation must use the transcript channel"
    );
}

/// AC2-HP (e2e, sigma-review GAP 1): the headline production path - a
/// payload-sourced promise on a NON-advisory code unit must flow through the
/// real done() reads and terminate DonePRGreen (green PR, reviewed,
/// head_shipped via the green_bins mock).
#[test]
fn payload_promise_reaches_done_pr_green() {
    let fx = fixture_with_manifest(
        "---\nsession_id: sess-payload-code\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\n---\n",
    );
    // x-0eaf: seed a local review attestation so the coverage gate sees review
    // (the empty test config fetches no GitHub reviews; without this the green
    // PR is DoneUnreviewed, not DonePRGreen).
    fs::write(
        &fx.events,
        "{\"type\":\"review_attestation\",\"data\":{\"reviewer\":\"code-review\",\
\"head_sha\":\"deadbeefdeadbeefdeadbeefdeadbeef00000001\",\"verdict\":\"pass\"}}\n",
    )
    .unwrap();
    let payload = serde_json::json!({
        "transcript_path": fx.transcript.to_str().unwrap(),
        "last_assistant_message": "<promise>MISSION COMPLETE: shipped</promise>"
    })
    .to_string();

    let (code, d) = spawn_loop_check(&fx, Some(&payload));
    assert_eq!(code, 0, "decision: {d}");
    assert_eq!(d["decision"], "allow", "decision: {d}");
    assert_eq!(d["termination_reason"], "DonePRGreen", "decision: {d}");
    assert_eq!(
        last_intent_source(&fx.events).as_deref(),
        Some("payload"),
        "the DonePRGreen loop_check event must attribute the payload channel"
    );
}

/// Regression (sigma-review CRITICAL): the SHIM must honor a block decision
/// from an OLD binary that never reads stdin, even when the hook payload far
/// exceeds the OS pipe buffer. The original pipe-based wiring died SIGPIPE
/// (141) under pipefail and fail-opened into allow-exit, discarding the
/// block; the herestring wiring removes the SIGPIPE surface entirely.
#[test]
fn shim_honors_block_when_old_binary_ignores_large_payload() {
    let fx = fixture();
    // The shim reads its state from .fno/target-state.md in $PWD.
    fs::copy(&fx.manifest, fx.cwd.join(".fno/target-state.md")).unwrap();

    // Mock OLD binary: ignores stdin and argv, emits a block decision, exit 0.
    let bin_dir = TempDir::new().unwrap();
    let old_bin = make_script(
        bin_dir.path(),
        "old-fno-agents",
        r#"echo '{"decision":"block","termination_reason":null,"message":"continue working; no completion signal","fires":1,"fingerprint":"x"}'"#,
    );

    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/target-stop-hook.sh");
    assert!(shim.exists(), "shim not found at {}", shim.display());

    // 200KB payload: larger than any default OS pipe buffer (16-64KB).
    let payload = serde_json::json!({
        "transcript_path": fx.transcript.to_str().unwrap(),
        "last_assistant_message": "x".repeat(200_000)
    })
    .to_string();

    let mut child = Command::new("bash")
        .arg(&shim)
        .current_dir(&fx.cwd)
        .env("FNO_AGENTS_BIN", &old_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "the old binary said block; the shim must exit 2, not fail-open. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn delivery_finalize_retry_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path().join("repo");
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status()
        .unwrap();
    fs::write(
        cwd.join(".fno/target-state.md"),
        "---\nsession_id: sess-delivery-retry\nharness_session_id: null\nclaude_session_id: null\n---\n",
    )
    .unwrap();
    let transcript = cwd.join("sess-delivery-retry.jsonl");
    fs::write(&transcript, "").unwrap();
    let mock = make_script(
        tmp.path(),
        "mock-fno-agents",
        r#"
if [ "$1" = "--version" ]; then exit 0; fi
if [ "$1" = "loop-check" ]; then
  mock_root="${MOCK_ROOT:-.}"
  count=0; [ -f "$mock_root/.fno/loop-count" ] && count=$(cat "$mock_root/.fno/loop-count")
  echo $((count + 1)) > "$mock_root/.fno/loop-count"
  rm -f "$mock_root/.fno/target-state.md"
  echo '{"decision":"allow","termination_reason":"DoneDelivery","message":"done"}'
  exit 0
fi
if [ "$1" = "finalize" ]; then
  mock_root="${MOCK_ROOT:-.}"
  count=0; [ -f "$mock_root/.fno/finalize-count" ] && count=$(cat "$mock_root/.fno/finalize-count")
  count=$((count + 1)); echo "$count" > "$mock_root/.fno/finalize-count"
  [ "$count" -eq 1 ] && exit 1
  touch "$mock_root/.fno/finalize-complete"
  exit 0
fi
exit 2
"#,
    );
    (tmp, cwd, transcript, mock)
}

fn git_path(cwd: &Path, name: &str) -> PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .current_dir(cwd)
        .output()
        .unwrap();
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn write_other_pending(cwd: &Path) {
    fs::write(
        git_path(cwd, "fno-delivery-finalize-pending-000-other.md"),
        "---\nsession_id: session-other\nharness_session_id: session-other\nclaude_session_id: session-other\n---\n",
    )
    .unwrap();
}

fn write_same_harness_pending(cwd: &Path) {
    fs::write(
        git_path(
            cwd,
            "fno-delivery-finalize-pending-sess-delivery-retry.session-old.md",
        ),
        "---\nsession_id: session-old\nharness_session_id: sess-delivery-retry\nclaude_session_id: sess-delivery-retry\n---\n",
    )
    .unwrap();
}

#[test]
fn claude_hook_retries_delivery_finalize_after_manifest_disappears() {
    let (_tmp, cwd, transcript, mock) = delivery_finalize_retry_fixture();
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/target-stop-hook.sh");
    let payload = serde_json::json!({"transcript_path": transcript}).to_string();
    let fire = || {
        let mut child = Command::new("bash")
            .arg(&shim)
            .current_dir(&cwd)
            .env("FNO_AGENTS_BIN", &mock)
            .env("MOCK_ROOT", &cwd)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait().unwrap().code()
    };

    write_same_harness_pending(&cwd);
    assert_eq!(fire(), Some(2));
    let retry = git_path(
        &cwd,
        "fno-delivery-finalize-pending-sess-delivery-retry.sess-delivery-retry.md",
    );
    assert!(
        retry.exists(),
        "missing retry snapshot at {}",
        retry.display()
    );
    write_other_pending(&cwd);
    assert_eq!(fire(), Some(0));
    assert!(cwd.join(".fno/finalize-complete").exists());
    assert_eq!(
        fs::read_to_string(cwd.join(".fno/finalize-count"))
            .unwrap()
            .trim(),
        "2"
    );
    assert_eq!(
        fs::read_to_string(cwd.join(".fno/loop-count"))
            .unwrap()
            .trim(),
        "1"
    );
}

#[test]
fn agy_hook_retries_delivery_finalize_after_manifest_disappears() {
    let (_tmp, cwd, transcript, mock) = delivery_finalize_retry_fixture();
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/agy-target-stop-hook.sh");
    let payload = serde_json::json!({
        "conversationId": "sess-delivery-retry",
        "transcriptPath": transcript,
        "workspacePaths": [cwd],
        "fullyIdle": true
    })
    .to_string();
    let fire = || {
        let mut child = Command::new("bash")
            .arg(&shim)
            .current_dir(cwd.parent().unwrap())
            .env("FNO_AGENTS_BIN", &mock)
            .env("MOCK_ROOT", &cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    };

    write_same_harness_pending(&cwd);
    assert!(fire().contains("generic delivery finalization failed"));
    assert!(git_path(
        &cwd,
        "fno-delivery-finalize-pending-sess-delivery-retry.sess-delivery-retry.md",
    )
    .exists());
    write_other_pending(&cwd);
    assert_eq!(fire().trim(), "{}");
    assert!(cwd.join(".fno/finalize-complete").exists());
    assert_eq!(
        fs::read_to_string(cwd.join(".fno/finalize-count"))
            .unwrap()
            .trim(),
        "2"
    );
    assert_eq!(
        fs::read_to_string(cwd.join(".fno/loop-count"))
            .unwrap()
            .trim(),
        "1"
    );
}

#[test]
fn snapshot_failure_does_not_gate_a_legacy_terminal() {
    let (_tmp, cwd, transcript, _mock) = delivery_finalize_retry_fixture();
    let mock = make_script(
        cwd.parent().unwrap(),
        "legacy-fno-agents",
        r#"
if [ "$1" = "--version" ]; then exit 0; fi
if [ "$1" = "loop-check" ]; then
  echo '{"decision":"allow","termination_reason":"DoneAdvisory","message":"legacy done"}'
  exit 0
fi
if [ "$1" = "finalize" ]; then touch .fno/legacy-finalized; exit 0; fi
exit 2
"#,
    );
    make_script(cwd.parent().unwrap(), "cp", "exit 1");
    let path = format!(
        "{}:{}",
        cwd.parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/target-stop-hook.sh");
    let payload = serde_json::json!({"transcript_path": transcript}).to_string();
    let mut child = Command::new("bash")
        .arg(&shim)
        .current_dir(&cwd)
        .env("FNO_AGENTS_BIN", &mock)
        .env("PATH", path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(0));
    assert!(cwd.join(".fno/legacy-finalized").exists());
}

fn stale_pending_with_live_session_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let (tmp, cwd, transcript, _mock) = delivery_finalize_retry_fixture();
    fs::write(
        cwd.join(".fno/target-state.md"),
        "---\nsession_id: session-live\nharness_session_id: sess-delivery-retry\nclaude_session_id: sess-delivery-retry\n---\n",
    )
    .unwrap();
    let pending = Command::new("git")
        .args([
            "rev-parse",
            "--git-path",
            "fno-delivery-finalize-pending-sess-delivery-retry.session-old.md",
        ])
        .current_dir(&cwd)
        .output()
        .unwrap();
    let pending = PathBuf::from(String::from_utf8(pending.stdout).unwrap().trim());
    let pending = if pending.is_absolute() {
        pending
    } else {
        cwd.join(pending)
    };
    fs::write(
        &pending,
        "---\nsession_id: session-old\nharness_session_id: sess-delivery-retry\nclaude_session_id: sess-delivery-retry\n---\n",
    )
    .unwrap();
    let mock = make_script(
        tmp.path(),
        "collision-fno-agents",
        r#"
if [ "$1" = "--version" ]; then exit 0; fi
if [ "$1" = "loop-check" ]; then
  touch .fno/live-loopchecked
  echo '{"decision":"block","termination_reason":null,"message":"live session incomplete"}'
  exit 0
fi
if [ "$1" = "finalize" ]; then touch .fno/stale-finalized; exit 0; fi
exit 2
"#,
    );
    (tmp, cwd, transcript, mock)
}

#[test]
fn claude_stale_pending_cannot_bypass_a_live_session() {
    let (_tmp, cwd, transcript, mock) = stale_pending_with_live_session_fixture();
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/target-stop-hook.sh");
    let payload = serde_json::json!({"transcript_path": transcript}).to_string();
    let mut child = Command::new("bash")
        .arg(&shim)
        .current_dir(&cwd)
        .env("FNO_AGENTS_BIN", &mock)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();

    assert_eq!(child.wait().unwrap().code(), Some(2));
    assert!(cwd.join(".fno/live-loopchecked").exists());
    assert!(!cwd.join(".fno/stale-finalized").exists());
}

#[test]
fn agy_stale_pending_cannot_bypass_a_live_session() {
    let (_tmp, cwd, transcript, mock) = stale_pending_with_live_session_fixture();
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/agy-target-stop-hook.sh");
    let payload = serde_json::json!({
        "conversationId": "sess-delivery-retry",
        "transcriptPath": transcript,
        "workspacePaths": [cwd],
        "fullyIdle": true
    })
    .to_string();
    let mut child = Command::new("bash")
        .arg(&shim)
        .current_dir(&cwd)
        .env("FNO_AGENTS_BIN", &mock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("live session incomplete"));
    assert!(cwd.join(".fno/live-loopchecked").exists());
    assert!(!cwd.join(".fno/stale-finalized").exists());
}

#[test]
fn agy_foreign_conversation_cannot_judge_a_live_session() {
    let (_tmp, cwd, transcript, mock) = stale_pending_with_live_session_fixture();
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/agy-target-stop-hook.sh");
    let payload = serde_json::json!({
        "conversationId": "conversation-foreign",
        "transcriptPath": transcript,
        "workspacePaths": [cwd],
        "fullyIdle": true
    })
    .to_string();
    let mut child = Command::new("bash")
        .arg(&shim)
        .current_dir(&cwd)
        .env("FNO_AGENTS_BIN", &mock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "{}");
    assert!(!cwd.join(".fno/live-loopchecked").exists());
    assert!(!cwd.join(".fno/stale-finalized").exists());
}

/// The original user path, closed end to end: the REAL shim runs the REAL
/// binary, one external read wedges past its bound, and what reaches the hook
/// protocol is the bounded decision with the exact killed-read cause - never
/// checker-unavailable handling and never the generic failed-GitHub-read line.
#[test]
fn shim_prints_the_exact_timeout_cause_when_a_read_wedges() {
    let fx = fixture_with_manifest(
        "---\nsession_id: sess-wedge-e2e\ncreated_at: 2026-06-05T00:00:00Z\nattended: true\n---\n",
    );
    // The shim reads its state from .fno/target-state.md in $PWD.
    fs::copy(&fx.manifest, fx.cwd.join(".fno/target-state.md")).unwrap();

    // gh mock: --version fast; the fingerprint's exact argv wedges. The
    // full-field view and every other read answer green.
    let bins = TempDir::new().unwrap();
    let gh = make_script(
        bins.path(),
        "gh",
        r#"if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
if echo "$*" | grep -q "state,number,headRefName" && ! echo "$*" | grep -q "headRefOid"; then
  sleep 30
  echo '{"state":"OPEN","number":1,"headRefName":"main"}'
  exit 0
fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"deadbeefdeadbeefdeadbeefdeadbeef00000001"}'
  exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[],"comments":[]}'
  exit 0
fi
exit 1"#,
    );
    let git = make_script(
        bins.path(),
        "git",
        r#"echo "deadbeefdeadbeefdeadbeefdeadbeef00000001""#,
    );

    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks/target-stop-hook.sh");
    assert!(shim.exists(), "shim not found at {}", shim.display());

    let payload = serde_json::json!({
        "transcript_path": fx.transcript.to_str().unwrap(),
        "last_assistant_message": "still working on it"
    })
    .to_string();

    let started = std::time::Instant::now();
    let mut child = Command::new("bash")
        .arg(&shim)
        .current_dir(&fx.cwd)
        .env("FNO_AGENTS_BIN", env!("CARGO_BIN_EXE_fno-agents"))
        .env("FNO_LOOPCHECK_GH_BIN", &gh)
        .env("FNO_LOOPCHECK_GIT_BIN", &git)
        .env("FNO_LOOPCHECK_READ_TIMEOUT_MS", "1000")
        .env("FNO_NUDGE_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "the bounded block must reach the hook protocol. stderr: {stderr}"
    );
    assert!(
        stderr.contains("external read 'fingerprint_pr_view' timed out after"),
        "the shim must print the exact killed-read cause: {stderr}"
    );
    assert!(
        stderr.contains("was killed"),
        "the cause must say the child was killed: {stderr}"
    );
    assert!(
        !stderr.contains("checker unavailable"),
        "a bounded decision is the checker WORKING, not unavailable: {stderr}"
    );
    assert!(
        !stderr.contains("gh read '"),
        "the generic failed-read line must not appear for a kill: {stderr}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "the whole shim path must stay inside the bound plus slack, took {:?}",
        started.elapsed()
    );
}
