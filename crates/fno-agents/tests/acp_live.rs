use fno_agents::acp_stdio::{
    dsh_acp_argv, grok_acp_argv, kimi_acp_argv, session_new_params, AcpError, AcpSession,
    PermissionPolicy, DSH_PROFILE, GROK_PROFILE, KIMI_PROFILE,
};
use serde_json::Value;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn scratch_repo() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "fno-grok-acp-live-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&path)
        .status();
    assert!(
        status.is_ok_and(|status| status.success()),
        "initialize scratch repo"
    );
    path
}

fn session(cwd: &Path, policy: PermissionPolicy) -> AcpSession {
    let plugin = fno_agents::acp_stdio::stage_plugin_dir();
    let mut argv = grok_acp_argv(None, None, plugin.as_deref());
    let debug_file = cwd.join("grok-debug.log");
    argv.splice(
        2..2,
        [
            "--debug-file".into(),
            debug_file.to_string_lossy().into_owned(),
        ],
    );
    AcpSession::start_with_policy(&GROK_PROFILE, argv, cwd, None, policy).unwrap()
}

fn updates_text(session: &AcpSession) -> String {
    session
        .notifications()
        .iter()
        .filter(|value| value.get("method").and_then(Value::as_str) == Some("session/update"))
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_update(session: &AcpSession) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while session.notifications().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !session.notifications().is_empty(),
        "grok sent no session/update"
    );
}

fn record(key: &str, value: &str, version: &str, date: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok-acp-trials.txt");
    use std::io::Write;
    let old = fs::read_to_string(&path).unwrap_or_default();
    let prefix = format!("{key}=");
    let mut rows: Vec<&str> = old
        .lines()
        .filter(|row| !row.starts_with(&prefix))
        .collect();
    let mut file = fs::File::create(path).unwrap();
    for row in rows.drain(..) {
        writeln!(file, "{row}").unwrap();
    }
    writeln!(file, "{key}={value} source=measured grok {version} {date}").unwrap();
}

#[test]
fn grok_live_acp_and_headless_journeys() {
    if std::env::var("FNO_GROK_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipping Grok live journey; set FNO_GROK_LIVE=1 to use operator credentials");
        return;
    }
    let Ok(version) = Command::new("grok").arg("--version").output() else {
        eprintln!("skipping Grok live journey; grok is not on PATH");
        return;
    };
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    let date = Command::new("date").arg("+%F").output().unwrap();
    let date = String::from_utf8_lossy(&date.stdout).trim().to_string();
    record("status", "running", &version, &date);
    let cwd = scratch_repo();
    let token = "ACP_5BB9_RECALL";

    let first = session(&cwd, PermissionPolicy::Refuse);
    first.initialize().unwrap();
    let id = match first.session_new(session_new_params(&cwd, &[])) {
        Ok(id) => id,
        Err(error @ AcpError::AuthRequired { .. }) => {
            record("status", "unavailable-not-authenticated", &version, &date);
            record("acp_journey", "not-run-not-authenticated", &version, &date);
            record(
                "headless_create_resume",
                "not-run-not-authenticated",
                &version,
                &date,
            );
            record(
                "plugin_hook_row",
                "not-run-not-authenticated",
                &version,
                &date,
            );
            eprintln!("skipping live Grok journey: {error}");
            return;
        }
        Err(error) => panic!("Grok session/new failed: {error}"),
    };
    if let Err(error) = first.prompt(&format!(
        "Reply with exactly this token and nothing else: {token}"
    )) {
        if error.to_string().contains("usage-exhausted") {
            record("acp_create_prompt", "skip-usage-exhausted", &version, &date);
            record("status", "grok-quota-exhausted", &version, &date);
            return;
        }
        panic!("Grok ACP prompt failed: {error}");
    }
    let first_updates = updates_text(&first);
    assert!(
        first_updates.contains(token),
        "token missing from updates: {first_updates}"
    );
    record("acp_create_prompt", "pass", &version, &date);
    drop(first);
    let resumed = Arc::new(session(&cwd, PermissionPolicy::Refuse));
    resumed.initialize().unwrap();
    resumed.session_resume(&id).unwrap();
    resumed
        .prompt("Repeat the token from our prior turn.")
        .unwrap();
    let recall = updates_text(&resumed);
    assert!(
        recall.contains(token),
        "session did not recall token: {recall}"
    );
    record("acp_resume_recall", "pass", &version, &date);

    let cancel = resumed.cancel_handle().unwrap();
    let running = Arc::clone(&resumed);
    let prompt = thread::spawn(move || running.prompt("Count slowly from 1 to 500."));
    wait_for_update(&resumed);
    cancel.cancel().unwrap();
    let cancelled = prompt.join().unwrap().unwrap();
    assert_eq!(
        cancelled.get("stopReason").and_then(Value::as_str),
        Some("cancelled")
    );
    record("acp_cancel", "pass", &version, &date);
    resumed.session_close(&id).unwrap();

    let denied_path = cwd.join("refused.txt");
    let denied = session(&cwd, PermissionPolicy::Refuse);
    denied.initialize().unwrap();
    let denied_id = denied
        .session_new(session_new_params(&cwd, &[]))
        .expect("permission refusal session created");
    let denial = denied.prompt(&format!(
        "Create {} containing no text.",
        denied_path.display()
    ));
    assert!(denial.is_err(), "Refuse policy allowed a tool call");
    assert!(!denied_path.exists(), "refused tool call created a file");
    let _ = denied.session_close(&denied_id);
    record("permission_refuse", "pass", &version, &date);

    let allowed_path = cwd.join("allowed.txt");
    let allowed = session(&cwd, PermissionPolicy::AllowOnce);
    allowed.initialize().unwrap();
    let allowed_id = allowed
        .session_new(session_new_params(&cwd, &[]))
        .expect("allow-once session created");
    allowed
        .prompt(&format!(
            "Create {} containing no text.",
            allowed_path.display()
        ))
        .unwrap();
    assert!(allowed_path.exists(), "allow-once did not create the file");
    let _ = allowed.session_close(&allowed_id);
    record("permission_allow_once", "pass", &version, &date);

    let home = fno_agents::paths::AgentsHome::at(cwd.join("fno-home"));
    let form = fno_agents::harness_capabilities::HarnessContract::packaged().and_then(|contract| {
        contract.render_session_argv("grok", "headless_create", Some("probe-session-id"))
    });
    if form.is_ok() {
        let outcome = fno_agents::grok_ask::dispatch_grok_once(
            &home,
            "live-grok",
            "Reply with the token HEADLESS_5BB9.",
            "fno",
            &cwd,
            None,
            None,
            false,
            None,
            Some(Duration::from_secs(600)),
            &[],
        );
        assert_eq!(outcome.exit_code, 0, "{}", outcome.stderr);
        assert!(outcome.stdout.contains("HEADLESS_5BB9"));
        let id = outcome
            .stderr
            .split_whitespace()
            .find_map(|field| field.strip_prefix("session_id="))
            .expect("headless receipt has session id");
        let resumed = Command::new("grok")
            .args(["--trust", "--resume", id, "-p", "Repeat the token."])
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(
            resumed.status.success(),
            "{}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert!(String::from_utf8_lossy(&resumed.stdout).contains("HEADLESS_5BB9"));
        record("headless_create_resume", "pass", &version, &date);
    } else {
        record(
            "headless_create_resume",
            "not-run-capability-disabled",
            &version,
            &date,
        );
    }
    record("status", "acp-measured", &version, &date);
}

#[test]
fn grok_live_fixture_has_explicit_readings() {
    let text = include_str!("fixtures/grok-acp-trials.txt");
    assert!(
        text.contains("status="),
        "fixture records measured or unavailable status"
    );
    assert!(
        text.contains("source="),
        "fixture names its evidence source"
    );
}

#[test]
fn installed_grok_and_kimi_acp_smoke() {
    let cwd = scratch_repo();
    let mut checked = 0;

    if Command::new("grok").arg("--version").output().is_ok() {
        let session =
            AcpSession::start(&GROK_PROFILE, grok_acp_argv(None, None, None), &cwd, None).unwrap();
        assert_eq!(session.initialize().unwrap()["protocolVersion"], 1);
        assert!(session.session_list().unwrap()["sessions"].is_array());
        checked += 1;
    }

    if Command::new("kimi").arg("--version").output().is_ok() {
        let session = AcpSession::start(&KIMI_PROFILE, kimi_acp_argv(None), &cwd, None).unwrap();
        assert_eq!(session.initialize().unwrap()["protocolVersion"], 1);
        assert!(session.session_list().unwrap()["sessions"].is_array());
        checked += 1;
    }

    if checked == 0 {
        eprintln!("skipping ACP smoke: neither Grok nor Kimi is installed");
    }
}

#[test]
fn kimi_print_smoke_emits_version_before_unconfigured_provider_fails() {
    let home = scratch_repo();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut command = Command::new("kimi");
    command
        .args(["-p", "say ok", "--output-format", "stream-json"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let Ok(child) = command.spawn() else {
        eprintln!("skipping Kimi print smoke: kimi is not on PATH");
        return;
    };
    let mut watchdog =
        fno_agents::subprocess_ask::AskWatchdog::spawn(child.id(), Some(Duration::from_secs(60)));
    let output = child.wait_with_output().unwrap();
    watchdog.cancel();
    watchdog.join();
    assert!(!watchdog.timed_out(), "Kimi print smoke exceeded 60s");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("\"type\":\"system.version\""),
        "Kimi emitted no system.version marker"
    );
}

#[test]
fn kimi_live_session_returns_a_planted_token() {
    if std::env::var("FNO_KIMI_LIVE").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("kimi").arg("--version").output().is_err() {
        eprintln!("skipping Kimi live journey; kimi is not on PATH");
        return;
    }
    let cwd = scratch_repo();
    let session = AcpSession::start(&KIMI_PROFILE, kimi_acp_argv(None), &cwd, None).unwrap();
    session.initialize().unwrap();
    let id = session
        .session_new(session_new_params(&cwd, &[]))
        .expect("Kimi session/new returns a session id");
    session
        .prompt("Reply with exactly KIMI_ACP_LIVE_TOKEN. Do not call tools.")
        .unwrap();
    assert!(updates_text(&session).contains("KIMI_ACP_LIVE_TOKEN"));
    session.session_close(&id).unwrap();
}

#[test]
fn dsh_live_session_returns_a_planted_token() {
    if std::env::var("FNO_DSH_LIVE").ok().as_deref() != Some("1") {
        return;
    }
    if Command::new("dsh").arg("--version").output().is_err() {
        eprintln!("skipping DSH live journey; dsh is not on PATH");
        return;
    }
    let cwd = scratch_repo();
    let session = AcpSession::start(&DSH_PROFILE, dsh_acp_argv(), &cwd, None).unwrap();
    session.initialize().unwrap();
    let id = session
        .session_new(session_new_params(&cwd, &[]))
        .expect("DSH session/new returns a session id");
    session
        .prompt("Reply with exactly DSH_ACP_LIVE_TOKEN. Do not call tools.")
        .unwrap();
    assert!(updates_text(&session).contains("DSH_ACP_LIVE_TOKEN"));
    session.session_close(&id).unwrap();
}
