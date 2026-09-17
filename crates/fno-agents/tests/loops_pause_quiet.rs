//! `pause-all`/`resume-all` compose the loops sentinel with the mail hold
//! leg. These tests stub `fno agents mail hold` via `FNO_LOOPS_MAIL_BIN` so
//! no real mail bus or session identity is needed.

use fno_agents::loops_pause::run_loops_capture;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

/// Every test in this file mutates process-global env vars (HOME,
/// FNO_LOOPS_MAIL_BIN); this binary's tests run on separate threads by
/// default, so the mutation must be serialized against itself.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn stub(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fno-stub");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// Pins HOME (the sentinel lives under `$HOME/.fno`) and FNO_LOOPS_MAIL_BIN
/// (the mail leg's child) for the duration of `body`, holding `ENV_LOCK` so
/// a parallel test in this binary cannot observe the mutation.
fn with_env(home: &Path, mail_bin: Option<&Path>, body: impl FnOnce()) {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved_home = std::env::var_os("HOME");
    let saved_bin = std::env::var_os("FNO_LOOPS_MAIL_BIN");
    std::env::set_var("HOME", home);
    match mail_bin {
        Some(bin) => std::env::set_var("FNO_LOOPS_MAIL_BIN", bin),
        None => std::env::remove_var("FNO_LOOPS_MAIL_BIN"),
    }
    body();
    match saved_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match saved_bin {
        Some(v) => std::env::set_var("FNO_LOOPS_MAIL_BIN", v),
        None => std::env::remove_var("FNO_LOOPS_MAIL_BIN"),
    }
    drop(guard);
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

#[test]
fn ac1_pause_all_holds_mail_with_a_ttl_and_a_reason() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let bin = stub(tmp.path(), "echo holding");

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
        let sentinel: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(home.join(".fno/loops-paused.json")).unwrap())
                .unwrap();
        assert_eq!(sentinel["reason"], "talk");
        let mail_leg = output["silenced"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "held");
        assert_eq!(mail_leg["detail"], "holding");
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
        assert!(home.join(".fno/loops-paused.json").exists());
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
fn ac3_no_ttl_never_invokes_the_stub() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let marker = tmp.path().join("called");
    let bin = stub(tmp.path(), &format!("touch {}", marker.display()));

    with_env(&home, Some(&bin), || {
        let (code, output) = run_loops_capture(&owned(&["pause-all", "--json"]));
        assert_eq!(code, 0, "{output}");
        assert!(!marker.exists(), "stub must not run with no --ttl");
        let mail_leg = output["silenced"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leg"] == "mail")
            .unwrap();
        assert_eq!(mail_leg["state"], "skipped");
        assert_eq!(
            mail_leg["detail"],
            "pass --ttl so the mail hold lifts by itself"
        );
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
    let bin = stub(
        tmp.path(),
        "echo 'hold off: delivered 2 held message(s) (0 deduped) - delivered'",
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
            .contains("delivered 2 held message(s)"));
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
