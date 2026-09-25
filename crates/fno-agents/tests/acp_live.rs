use fno_agents::acp_stdio::{
    grok_acp_argv, session_new_params, AcpSession, PermissionPolicy, GROK_PROFILE,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    let version = Command::new("grok").arg("--version").output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    let date = Command::new("date").arg("+%F").output().unwrap();
    let date = String::from_utf8_lossy(&date.stdout).trim().to_string();
    let auth = Command::new("grok").arg("models").output().unwrap();
    let auth_output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&auth.stdout),
        String::from_utf8_lossy(&auth.stderr)
    );
    if auth_output
        .to_ascii_lowercase()
        .contains("not authenticated")
    {
        record("status", "unavailable-not-authenticated", &version, &date);
        eprintln!("skipping live Grok journey: `grok models` reports no authentication");
        return;
    }
    record("status", "running", &version, &date);
    let cwd = scratch_repo();
    let token = "ACP_5BB9_RECALL";

    let first = session(&cwd, PermissionPolicy::Refuse);
    first.initialize().unwrap();
    let id = first
        .session_new(session_new_params(&cwd, &[]))
        .expect("session/new mints a session id");
    assert_ne!(id, "12345678-1234-4234-8234-123456789abc");
    first
        .prompt(&format!(
            "Reply with exactly this token and nothing else: {token}"
        ))
        .unwrap();
    let first_updates = updates_text(&first);
    assert!(
        first_updates.contains(token),
        "token missing from updates: {first_updates}"
    );
    record("acp_create_prompt", "pass", &version, &date);
    let debug = fs::read_to_string(cwd.join("grok-debug.log")).unwrap_or_default();
    record(
        "plugin_hook_row",
        if debug.contains("fno") {
            "seen"
        } else {
            "not-seen"
        },
        &version,
        &date,
    );

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
    record("status", "measured", &version, &date);
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
