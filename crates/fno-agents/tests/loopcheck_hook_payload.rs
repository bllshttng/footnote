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
