//! `pause-all`/`resume-all` alias the fleet breaker and own only their mail
//! hold leg. The stub avoids a real mail bus or session identity.

use fno_agents::loops_pause::run_loops_capture;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

/// Every test in this file mutates process-global env vars; serialize them
/// and restore the prior values even if a test panics.
static ENV_LOCK: Mutex<()> = Mutex::new(());
const TEST_SESSION_ID: &str = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
const OTHER_SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

struct EnvReset {
    home: Option<std::ffi::OsString>,
    agents_home: Option<std::ffi::OsString>,
    bin: Option<std::ffi::OsString>,
    session_id: Option<std::ffi::OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl Drop for EnvReset {
    fn drop(&mut self) {
        for (key, value) in [
            ("HOME", self.home.take()),
            ("FNO_AGENTS_HOME", self.agents_home.take()),
            ("FNO_LOOPS_MAIL_BIN", self.bin.take()),
            ("FNO_SESSION_ID", self.session_id.take()),
        ] {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn stub(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fno-stub");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// Pin state roots and the mail child. Drop restores them even if a test
/// panics, while the lock excludes concurrent tests in this binary.
fn with_env(home: &Path, mail_bin: Option<&Path>, body: impl FnOnce()) {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let reset = EnvReset {
        home: std::env::var_os("HOME"),
        agents_home: std::env::var_os("FNO_AGENTS_HOME"),
        bin: std::env::var_os("FNO_LOOPS_MAIL_BIN"),
        session_id: std::env::var_os("FNO_SESSION_ID"),
        _guard: guard,
    };
    let agents_home = home.join(".fno/agents");
    fs::create_dir_all(&agents_home).unwrap();
    std::env::set_var("HOME", home);
    std::env::set_var("FNO_AGENTS_HOME", &agents_home);
    std::env::set_var("FNO_SESSION_ID", TEST_SESSION_ID);
    match mail_bin {
        Some(bin) => std::env::set_var("FNO_LOOPS_MAIL_BIN", bin),
        None => std::env::remove_var("FNO_LOOPS_MAIL_BIN"),
    }
    body();
    drop(reset);
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn seed_pause_all_record(home: &Path, mail: &str, mail_session_id: Option<&str>) {
    let agents = home.join(".fno/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("fleet-stop.json"),
        serde_json::json!({
            "version": 1,
            "state": "stopped",
            "generation": 1,
            "changed_at": "2026-09-26T00:00:00Z",
            "changed_by": "op",
            "reason": "pause-all by op",
            "holds": ["spawns", "loops"],
            "source": "file",
            "target": null,
            "expires_at": "2099-12-31T00:00:00Z",
            "origin": "pause-all",
            "mail": mail,
            "mail_session_id": mail_session_id,
        })
        .to_string(),
    )
    .unwrap();
}

fn seed_incident_record(home: &Path) -> String {
    let agents = home.join(".fno/agents");
    fs::create_dir_all(&agents).unwrap();
    let text = serde_json::json!({
        "version": 1,
        "state": "stopped",
        "generation": 7,
        "changed_at": "2026-09-26T00:00:00Z",
        "changed_by": "operator",
        "reason": "incident response",
        "holds": ["spawns", "tests", "merges", "loops"],
        "source": "file",
    })
    .to_string();
    fs::write(agents.join("fleet-stop.json"), &text).unwrap();
    text
}

#[test]
fn ac1_pause_all_holds_mail_with_a_ttl_and_a_reason() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *--status*) echo 'agent: no hold - mail delivers normally' ;;\n  *--for*) echo 'mail hold armed' ;;\n  *--off*) echo 'hold off: delivered 2 held message(s)' ;;\nesac",
            calls.display()
        ),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&[
            "pause-all",
            "--who",
            "op",
            "--reason",
            "talk",
            "--ttl",
            "30m",
            "--json",
        ]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["reason"], "talk");
        assert_eq!(output["ttl_defaulted"], false);
        let breaker_path = home.join(".fno/agents/fleet-stop.json");
        assert!(
            breaker_path.exists(),
            "pause-all must write the fleet breaker"
        );
        let breaker: serde_json::Value =
            serde_json::from_slice(&fs::read(breaker_path).unwrap()).unwrap();
        assert_eq!(breaker["reason"], "talk");
        assert_eq!(breaker["state"], "stopped");
        assert_eq!(breaker["holds"], serde_json::json!(["spawns", "loops"]));
        assert_eq!(breaker["origin"], "pause-all");
        assert_eq!(breaker["mail"], "armed");
        assert_eq!(breaker["mail_session_id"], TEST_SESSION_ID);
        assert!(breaker["expires_at"].is_string());
        assert!(!home.join(".fno/loops-paused.json").exists());
        let calls: Vec<_> = fs::read_to_string(calls)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(calls.len(), 2, "status precedes the owned arm: {calls:?}");
        assert!(calls[0].ends_with("--status"), "{calls:?}");
        assert!(calls[1].ends_with("--for 30"), "{calls:?}");
        let mail_leg = output["silenced"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "armed");
        assert_eq!(mail_leg["detail"], "mail hold armed");
    });
}

#[test]
fn ac2_a_stub_reporting_no_identity_skips_the_mail_leg_and_still_pauses() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = stub(tmp.path(), "exit 3");

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--ttl", "5m", "--json"]));
        assert_eq!(code, 0, "{output}");
        let breaker: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(breaker["state"], "stopped");
        assert_eq!(breaker["origin"], "pause-all");
        assert_eq!(breaker["mail"], "skipped");
        assert!(breaker["mail_session_id"].is_null());
        let mail_leg = output["silenced"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "skipped");
    });
}

#[test]
fn ac3_pause_all_defaults_to_a_sixty_minute_bounded_hold() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let calls_path = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *--status*) echo 'agent: no hold - mail delivers normally' ;;\n  *--for*) echo 'mail hold armed' ;;\nesac",
            calls_path.display()
        ),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        let breaker: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(breaker["holds"], serde_json::json!(["spawns", "loops"]));
        assert_eq!(output["ttl_defaulted"], true);
        let expires_at =
            chrono::DateTime::parse_from_rfc3339(breaker["expires_at"].as_str().unwrap()).unwrap();
        let remaining = expires_at
            .signed_duration_since(chrono::Utc::now())
            .num_seconds();
        assert!((3_590..=3_600).contains(&remaining));
        let calls: Vec<_> = fs::read_to_string(calls_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(calls.len(), 2, "status precedes the owned arm: {calls:?}");
        assert!(calls[0].ends_with("--status"), "{calls:?}");
        assert!(calls[1].ends_with("--for 60"), "{calls:?}");
        let mail_leg = output["silenced"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "armed");
    });
}

#[test]
fn ac5_a_zero_ttl_is_refused_and_writes_no_sentinel() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    with_env(&home, None, || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--ttl", "0m", "--json"]));
        assert_eq!(code, 2, "{output}");
        assert!(!home.join(".fno/loops-paused.json").exists());
        assert!(!home.join(".fno/agents/fleet-stop.json").exists());
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--ttl-ms", "0", "--json"]));
        assert_eq!(code, 2, "{output}");
        assert!(!home.join(".fno/agents/fleet-stop.json").exists());
    });
}

#[test]
fn a_ttl_too_large_to_multiply_is_refused_not_a_panic() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    with_env(&home, None, || {
        let (code, output) = run_loops_capture(&owned(&[
            "pause-all",
            "--ttl",
            "300000000000000d",
            "--json",
        ]));
        assert_eq!(code, 2, "{output}");
        assert!(!home.join(".fno/loops-paused.json").exists());
    });
}

#[test]
fn ac6_resume_all_lifts_the_mail_leg() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".fno")).unwrap();
    fs::write(
        home.join(".fno/loops-paused.json"),
        r#"{"who":"op","paused_at":1,"expires_at":null,"reason":null}"#,
    )
    .unwrap();
    seed_pause_all_record(&home, "armed", Some(TEST_SESSION_ID));
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!("printf '%s\\n' \"$*\" >> '{}'", calls.display()),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["resumed"], true);
        assert!(!home.join(".fno/loops-paused.json").exists());
        let mail_leg = output["lifted"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "lifted");
        assert!(mail_leg["detail"]
            .as_str()
            .unwrap()
            .contains(TEST_SESSION_ID));
        let breaker: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(breaker["state"], "clear");
        assert_eq!(breaker["origin"], "pause-all");
        assert_eq!(breaker["mail"], "lifted");
        assert_eq!(breaker["mail_session_id"], TEST_SESSION_ID);
        assert_eq!(
            fs::read_to_string(calls).unwrap().trim(),
            format!("mail-hold --session {TEST_SESSION_ID} --off")
        );
    });
}

#[test]
fn resume_all_releases_the_session_that_armed_the_mail_hold() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    seed_pause_all_record(&home, "armed", Some(TEST_SESSION_ID));
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!("printf '%s\\n' \"$*\" >> '{}'", calls.display()),
    );

    with_env(&home, Some(&bin), || {
        std::env::set_var("FNO_SESSION_ID", OTHER_SESSION_ID);
        let (code, output) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(
            fs::read_to_string(calls).unwrap().trim(),
            format!("mail-hold --session {TEST_SESSION_ID} --off")
        );
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(record["state"], "clear");
        assert_eq!(record["mail_session_id"], TEST_SESSION_ID);
    });
}

#[test]
fn ac7_a_failing_stub_still_lifts_the_sentinel() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".fno")).unwrap();
    fs::write(
        home.join(".fno/loops-paused.json"),
        r#"{"who":"op","paused_at":1,"expires_at":null}"#,
    )
    .unwrap();
    seed_pause_all_record(&home, "armed", Some(TEST_SESSION_ID));
    let bin = stub(tmp.path(), "echo boom 1>&2\nexit 1");

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["resumed"], true);
        assert!(!home.join(".fno/loops-paused.json").exists());
        let mail_leg = output["lifted"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "failed");
        let breaker: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(breaker["state"], "clear");
        assert_eq!(breaker["mail"], "armed");
    });
}

#[test]
fn a_failing_stub_with_multibyte_stderr_does_not_panic_on_truncation() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    // 90 copies of a 3-byte char: byte offset 200 falls inside the 67th
    // character (198..201), so a byte-indexed truncate(200) panics on a
    // non-char-boundary; a char-indexed one cannot.
    let payload = tmp.path().join("payload");
    fs::write(&payload, "中".repeat(90)).unwrap();
    let bin = stub(
        tmp.path(),
        &format!("cat {} 1>&2\nexit 1", payload.display()),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--ttl", "5m", "--json"]));
        assert_eq!(code, 0, "{output}");
        let mail_leg = output["silenced"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "failed");
    });
}

#[test]
fn pause_all_refuses_to_replace_or_clear_an_incident() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let original = seed_incident_record(&home);
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        &tmp.path(),
        &format!("echo called >> '{}'", calls.display()),
    );

    with_env(&home, Some(&bin), || {
        let (pause_code, pause) = run_loops_capture(&owned(&["pause-all", "--json"]));
        let (resume_code, resume) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(pause_code, 1, "{pause}");
        assert_eq!(resume_code, 1, "{resume}");
        assert!(pause["error"]
            .as_str()
            .unwrap()
            .contains("fno agents incident"));
        assert!(resume["error"]
            .as_str()
            .unwrap()
            .contains("fno agents incident clear"));
        assert_eq!(
            fs::read_to_string(home.join(".fno/agents/fleet-stop.json")).unwrap(),
            original
        );
        assert!(
            !calls.exists(),
            "an incident refusal must precede mail calls"
        );
    });
}

#[test]
fn an_existing_unowned_mail_hold_is_kept_and_resume_leaves_it_alone() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; echo 'agent: holding mail, wall clock, fixed deadline 12:00:00 UTC, lifts in ~90m'",
            calls.display()
        ),
    );

    with_env(&home, Some(&bin), || {
        let (pause_code, pause) =
            run_loops_capture(&owned(&["pause-all", "--ttl", "5m", "--json"]));
        assert_eq!(pause_code, 0, "{pause}");
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(record["mail"], "kept");
        let (resume_code, resume) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(resume_code, 0, "{resume}");
        let leg = resume["lifted"]
            .as_array()
            .unwrap()
            .iter()
            .find(|leg| leg["leg"] == "mail")
            .unwrap();
        assert_eq!(leg["state"], "left");
        assert_eq!(
            fs::read_to_string(calls).unwrap().trim(),
            "agents mail hold --status"
        );
    });
}

#[test]
fn renewing_our_mail_hold_does_not_shorten_its_remaining_window() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    seed_pause_all_record(&home, "armed", Some(TEST_SESSION_ID));
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; case \"$*\" in *--status*) echo 'agent: holding mail, wall clock, fixed deadline 12:00:00 UTC, lifts in ~90m' ;; *--for*) echo armed ;; esac",
            calls.display()
        ),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--ttl", "5m", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(
            fs::read_to_string(calls).unwrap().lines().last().unwrap(),
            "agents mail hold --for 90"
        );
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(record["mail"], "armed");
    });
}

#[test]
fn mail_status_exit_three_is_recorded_without_arming_a_hold() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; echo 'ambiguous identity' >&2; exit 3",
            calls.display()
        ),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(record["mail"], "skipped");
        let leg = &output["silenced"][1];
        assert_eq!(leg["state"], "skipped");
        assert_eq!(leg["detail"], "ambiguous identity");
        assert_eq!(
            fs::read_to_string(calls).unwrap().trim(),
            "agents mail hold --status"
        );
    });
}

#[test]
fn a_failed_breaker_write_releases_a_mail_hold_it_just_armed() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let agents = home.join(".fno/agents");
    fs::create_dir_all(&agents).unwrap();
    fs::create_dir(agents.join("fleet-stop.json.lock")).unwrap();
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; case \"$*\" in *--status*) echo 'agent: no hold - mail delivers normally' ;; *--for*) echo armed ;; *--off*) echo off ;; esac",
            calls.display()
        ),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--json"]));
        assert_eq!(code, 1, "{output}");
        assert!(output["error"]
            .as_str()
            .unwrap()
            .contains("fleet-stop.json.lock"));
        let calls: Vec<_> = fs::read_to_string(calls)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert!(calls[0].ends_with("--status"), "{calls:?}");
        assert!(calls[1].ends_with("--for 60"), "{calls:?}");
        assert!(calls[2].ends_with("--off"), "{calls:?}");
        assert!(!home.join(".fno/agents/fleet-stop.json").exists());
    });
}

#[test]
fn legacy_sentinel_still_drives_status_and_resume_removes_it() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".fno")).unwrap();
    let sentinel = home.join(".fno/loops-paused.json");
    fs::write(&sentinel, r#"{"who":"op","paused_at":1,"expires_at":null}"#).unwrap();

    with_env(&home, None, || {
        let (status_code, status) = run_loops_capture(&owned(&["status", "--json"]));
        assert_eq!(status_code, 0, "{status}");
        assert_eq!(status["paused"], true);
        let (resume_code, resume) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(resume_code, 0, "{resume}");
        assert_eq!(resume["legacy_sentinel_removed"], true);
        assert!(!sentinel.exists());
    });
}

#[test]
fn pause_all_refuses_to_stack_an_active_legacy_sentinel() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".fno")).unwrap();
    let sentinel = home.join(".fno/loops-paused.json");
    fs::write(&sentinel, r#"{"who":"op","paused_at":1,"expires_at":null}"#).unwrap();
    let calls = tmp.path().join("mail-calls.txt");
    let bin = stub(
        tmp.path(),
        &format!("printf '%s\\n' \"$*\" >> '{}'", calls.display()),
    );

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--ttl", "5m", "--json"]));
        assert_eq!(code, 1, "{output}");
        assert!(output["error"]
            .as_str()
            .unwrap()
            .contains("legacy pause sentinel is active"));
        assert!(sentinel.exists(), "refusal preserves the old hold");
        assert!(!home.join(".fno/agents/fleet-stop.json").exists());
        assert!(!calls.exists(), "refusal precedes the mail leg");
    });
}

#[test]
fn resume_all_retries_a_failed_owned_mail_release_without_releasing_twice() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    seed_pause_all_record(&home, "armed", Some(TEST_SESSION_ID));
    let calls = tmp.path().join("mail-calls.txt");
    let failing_bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; echo transient >&2; exit 1",
            calls.display()
        ),
    );

    with_env(&home, Some(&failing_bin), || {
        let (code, output) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["lifted"][1]["state"], "failed");
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(record["state"], "clear");
        assert_eq!(record["mail"], "armed");
        assert_eq!(record["mail_session_id"], TEST_SESSION_ID);
    });

    let successful_bin = stub(
        tmp.path(),
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; echo 'hold off'",
            calls.display()
        ),
    );
    with_env(&home, Some(&successful_bin), || {
        let (code, output) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["lifted"][1]["state"], "lifted");
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".fno/agents/fleet-stop.json")).unwrap())
                .unwrap();
        assert_eq!(record["state"], "clear");
        assert_eq!(record["mail"], "lifted");
        assert_eq!(record["mail_session_id"], TEST_SESSION_ID);
    });

    with_env(&home, None, || {
        let (code, output) = run_loops_capture(&owned(&["resume-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert_eq!(output["lifted"][1]["state"], "left");
    });
    assert_eq!(
        fs::read_to_string(calls).unwrap().lines().count(),
        2,
        "a successful owner release is not repeated"
    );
}
