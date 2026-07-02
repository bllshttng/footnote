#![cfg(unix)]
/// Regression test: loop-check with a NONEXISTENT --state file returns exit 0
/// and decision "allow" with message containing "missing manifest".
///
/// This pins the behavior the Task 1.3 delegated close depends on: the
/// handoff helper (wave 2) archives target-state.md before the parent session
/// closes, so when the stop hook fires for the delegated close the manifest is
/// gone.  Loop-check must allow (not block) in that case.  The
/// session_satisfied(trigger="delegated") event is the audit trail; manifest
/// absence is the mechanical unlock.
///
/// Existing coverage: `corrupt_manifest_allows_exit` in loop_check.rs covers
/// the "file exists but is malformed" branch.  This test covers the distinct
/// "file does not exist at all" branch (Err from fs::read_to_string).
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ── minimal helpers (mirrors loop_check.rs, kept local so this file is
//    self-contained and the existing test file is not modified) ────────────────

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

struct MockBins {
    _dir: TempDir,
    pub gh: PathBuf,
    pub git: PathBuf,
}

impl MockBins {
    fn no_pr() -> Self {
        let dir = TempDir::new().unwrap();
        let gh = make_script(
            dir.path(),
            "gh",
            r#"
if echo "$*" | grep -q -- "--version"; then
  echo 'gh version 2.x'; exit 0
fi
exit 1
"#,
        );
        let git = make_script(
            dir.path(),
            "git",
            r#"echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa""#,
        );
        MockBins { _dir: dir, gh, git }
    }
}

#[derive(Debug, serde::Deserialize)]
struct Decision {
    decision: String,
    message: String,
}

fn fire_with_nonexistent_manifest(
    nonexistent_manifest: &Path,
    transcript: &Path,
    cwd: &Path,
    mock: &MockBins,
) -> (i32, Decision) {
    let mut args: Vec<String> = vec![
        "loop-check".to_string(),
        "--state".to_string(),
        nonexistent_manifest.to_str().unwrap().to_string(),
        "--transcript".to_string(),
        transcript.to_str().unwrap().to_string(),
        "--cwd".to_string(),
        cwd.to_str().unwrap().to_string(),
        "--now".to_string(),
        "2026-06-05T12:00:00Z".to_string(),
        format!("--gh-bin={}", mock.gh.display()),
        format!("--git-bin={}", mock.git.display()),
        // Isolate from the developer's real ~/.fno/settings.yaml
        "--global-settings".to_string(),
        "/nonexistent/global-settings.yaml".to_string(),
    ];
    // Suppress global-events write to avoid creating files outside tmp
    args.push("--global-events".to_string());
    args.push("/dev/null".to_string());

    let (code, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args);
    let d: Decision = serde_json::from_str(&json_str)
        .unwrap_or_else(|_| panic!("non-JSON output (code={code}): {json_str}"));
    (code, d)
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// AC1-HP: a nonexistent --state path returns exit 0, decision "allow",
/// and a message that contains "missing manifest" (the audit phrase the
/// delegated close relies on).
#[test]
fn missing_manifest_allows_exit_zero() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    // Isolate settings
    fs::write(cwd.join(".fno/settings.yaml"), "# isolated test settings\n").unwrap();

    // Point --state at a path that does NOT exist
    let nonexistent = cwd.join("target-state.md");
    assert!(
        !nonexistent.exists(),
        "pre-condition: manifest must not exist"
    );

    // Transcript can be empty (a user-only message)
    let transcript = cwd.join("transcript.jsonl");
    let user_msg = serde_json::json!({"message": {"role": "user", "content": "go"}});
    fs::write(
        &transcript,
        serde_json::to_string(&user_msg).unwrap() + "\n",
    )
    .unwrap();

    let mock = MockBins::no_pr();
    let (code, d) = fire_with_nonexistent_manifest(&nonexistent, &transcript, cwd, &mock);

    assert_eq!(code, 0, "missing manifest must exit 0 (allow), got {code}");
    assert_eq!(
        d.decision, "allow",
        "missing manifest must produce decision=allow, got {:?}",
        d.decision
    );
    assert!(
        d.message.contains("missing manifest"),
        "message must mention 'missing manifest'; got: {:?}",
        d.message
    );
}

/// AC2-EDGE: manifest file is absent AND transcript is also absent - still
/// allows exit (the manifest-read error fires before the transcript parse).
#[test]
fn missing_manifest_with_absent_transcript_allows_exit() {
    let tmp = TempDir::new().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join(".fno")).unwrap();
    fs::write(cwd.join(".fno/settings.yaml"), "# isolated test settings\n").unwrap();

    let nonexistent_manifest = cwd.join("target-state.md");
    let nonexistent_transcript = cwd.join("transcript.jsonl");

    let mock = MockBins::no_pr();
    let mut args: Vec<String> = vec![
        "loop-check".to_string(),
        "--state".to_string(),
        nonexistent_manifest.to_str().unwrap().to_string(),
        "--transcript".to_string(),
        nonexistent_transcript.to_str().unwrap().to_string(),
        "--cwd".to_string(),
        cwd.to_str().unwrap().to_string(),
        "--now".to_string(),
        "2026-06-05T12:00:00Z".to_string(),
        format!("--gh-bin={}", mock.gh.display()),
        format!("--git-bin={}", mock.git.display()),
        "--global-settings".to_string(),
        "/nonexistent/global-settings.yaml".to_string(),
        "--global-events".to_string(),
        "/dev/null".to_string(),
    ];
    let (code, json_str) = fno_agents::loopcheck::run_loop_check_capture(&args);
    // Accept exit 0 (allow) - the manifest read short-circuits before transcript
    assert_eq!(
        code, 0,
        "missing manifest + missing transcript must exit 0; json={json_str}"
    );
    let d: Decision =
        serde_json::from_str(&json_str).unwrap_or_else(|_| panic!("non-JSON output: {json_str}"));
    assert_eq!(d.decision, "allow");
}
