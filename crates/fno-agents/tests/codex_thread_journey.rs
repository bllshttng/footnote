//! The six-step unattended journey that earns Codex a `native` spawn claim.
//!
//! This is opt-in because it uses the installed Codex account and deliberately
//! kills a private mux server. A default run skips the live journey and must
//! not be used as evidence for a capability claim.

use fno_agents::paths::AgentsHome;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const CLIENT: &str = env!("CARGO_BIN_EXE_fno-agents");
const DAEMON: &str = env!("CARGO_BIN_EXE_fno-agents-daemon");

fn codex_available() -> bool {
    std::env::var("FNO_JOURNEY_CODEX").ok().as_deref() == Some("1")
        && Command::new("codex")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
}

fn run(env: &[(String, String)], args: &[&str]) -> Output {
    let mut command = Command::new(CLIENT);
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("fno-agents command runs")
}

fn run_without_capture(env: &[(String, String)], args: &[&str]) -> bool {
    let mut command = Command::new(CLIENT);
    command
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    command.status().expect("fno-agents command runs").success()
}

fn start_private_mux(env: &[(String, String)], cwd: &Path, session: &str) {
    let mut command = Command::new(std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".into()));
    let output = command
        .args([
            "mux",
            "pane",
            "run",
            "--session",
            session,
            "--cwd",
            cwd.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "sleep 180",
        ])
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .expect("private mux starts");
    assert!(output.status.success(), "mux start: {:?}", output);
}

fn kill_private_mux(env: &[(String, String)], session: &str) {
    let _ = Command::new(std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".into()))
        .args(["mux", "kill-server", session, "--json"])
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output();
}

fn wait_for_file(path: &Path, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!("Codex did not create {} within {budget:?}", path.display());
}

fn registry_row(home: &AgentsHome, name: &str) -> Value {
    let raw = std::fs::read_to_string(home.registry_json()).expect("registry exists");
    let registry: Value = serde_json::from_str(&raw).expect("registry JSON");
    registry["agents"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == name))
        .cloned()
        .expect("Codex thread registry row")
}

#[test]
fn codex_thread_journey_earns_capability() {
    if !codex_available() {
        eprintln!("skipping: set FNO_JOURNEY_CODEX=1 with codex on PATH to run the live journey");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("worker-worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&cwd)
        .status()
        .unwrap()
        .success());
    let agents_home = temp.path().join("agents");
    let mux_dir = PathBuf::from(format!("/tmp/fno-codex-journey-{}", std::process::id()));
    std::fs::create_dir_all(&mux_dir).unwrap();
    let mux_session = format!("codex-journey-{}", std::process::id());
    let marker = cwd.join("journey-marker.txt");
    let env = vec![
        (
            "FNO_AGENTS_HOME".into(),
            agents_home.to_string_lossy().into(),
        ),
        ("FNO_AGENTS_DAEMON_BIN".into(), DAEMON.into()),
        ("FNO_MUX_DIR".into(), mux_dir.to_string_lossy().into()),
        (
            "FNO_CLAIMS_ROOT".into(),
            temp.path().join("claims").to_string_lossy().into(),
        ),
        ("FNO_PROCESS_ADMISSION_MAX".into(), "512".into()),
    ];
    start_private_mux(&env, &cwd, &mux_session);

    let name = "codex-thread-journey";
    let seed = format!(
        "Create {} with exactly CODEX_THREAD_JOURNEY_TOKEN, then reply with the same token.",
        marker.display()
    );
    let output = run(
        &env,
        &[
            "spawn",
            "--name",
            name,
            "--harness",
            "codex",
            "--substrate",
            "thread",
            "--cwd",
            cwd.to_str().unwrap(),
            "--",
            &seed,
        ],
    );
    assert!(output.status.success(), "thread spawn: {:?}", output);
    let spawn_receipt: Value = serde_json::from_slice(&output.stdout).expect("spawn receipt JSON");

    let home = AgentsHome::at(agents_home.clone());
    let row = registry_row(&home, name);
    assert_eq!(row["harness"], "codex");
    let session_id = row["harness_session_id"].as_str().expect("full session id");
    assert!(session_id.len() > 8, "full session id: {session_id}");
    assert_eq!(spawn_receipt["harness_session_id"], session_id);
    assert!(
        row["short_id"].is_null() || row["short_id"] == "",
        "Codex thread must not have a short_id: {}",
        row["short_id"]
    );
    assert_eq!(row["host_mode"], "interactive");

    // Step 2: the first turn writes inside the worker's explicit worktree.
    wait_for_file(&marker, Duration::from_secs(180));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().trim_end(),
        "CODEX_THREAD_JOURNEY_TOKEN"
    );

    // Step 3: the mux is disposable; the app-server thread and registry row are not.
    kill_private_mux(&env, &mux_session);
    let after_mux_kill = registry_row(&home, name);
    assert_eq!(after_mux_kill["harness_session_id"], session_id);

    // Step 4: restart the supervisor, then require a recalled token on turn two.
    assert!(
        run_without_capture(&env, &["restart"]),
        "daemon restart failed"
    );
    let resumed = run(
        &env,
        &[
            "ask",
            name,
            "What exact token did you write in the first turn? Reply with that token.",
            "--harness",
            "codex",
        ],
    );
    assert!(resumed.status.success(), "thread resume: {:?}", resumed);
    assert!(
        String::from_utf8_lossy(&resumed.stdout).contains("CODEX_THREAD_JOURNEY_TOKEN"),
        "missing recalled token: {:?}",
        resumed
    );

    // Step 5: review/start must return a positive reviewThreadId receipt.
    let review = run(
        &env,
        &[
            "review-start",
            "--session",
            session_id,
            "--target",
            "baseBranch:main",
        ],
    );
    assert!(review.status.success(), "review start: {:?}", review);
    let receipt: Value = serde_json::from_slice(&review.stdout).expect("review receipt JSON");
    assert_eq!(receipt["delivered"], true);
    assert!(
        receipt["review_thread_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "missing reviewThreadId: {receipt}"
    );

    // Step 6: selection materialises a live viewport within a bounded budget.
    start_private_mux(&env, &cwd, &mux_session);
    let started = Instant::now();
    let fno = std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".into());
    let viewport = Command::new(fno)
        .args(["mux", "pane", "ls", "--session", &mux_session, "--json"])
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .expect("viewport selection runs");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "selection exceeded budget"
    );
    assert!(
        viewport.status.success(),
        "viewport selection: {:?}",
        viewport
    );
    let panes: Value = serde_json::from_slice(&viewport.stdout).expect("viewport JSON");
    assert!(panes.as_array().is_some_and(|panes| !panes.is_empty()));

    kill_private_mux(&env, &mux_session);
}

/// AC4 live probe: a follow-up ask arriving while the seed turn still drives
/// must answer from the SHARED turn (steer) or report in_flight - never block
/// behind a whole second turn. Skipped without FNO_JOURNEY_CODEX=1.
#[test]
fn codex_thread_followup_ask_steers_or_reports_in_flight() {
    if !codex_available() {
        eprintln!("skipping: set FNO_JOURNEY_CODEX=1 with codex on PATH to run the live journey");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("steer-worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    let agents_home = temp.path().join("agents");
    let mux_dir = PathBuf::from(format!("/tmp/fno-codex-steer-{}", std::process::id()));
    std::fs::create_dir_all(&mux_dir).unwrap();
    let env = vec![
        (
            "FNO_AGENTS_HOME".into(),
            agents_home.to_string_lossy().into(),
        ),
        ("FNO_AGENTS_DAEMON_BIN".into(), DAEMON.into()),
        ("FNO_MUX_DIR".into(), mux_dir.to_string_lossy().into()),
        (
            "FNO_CLAIMS_ROOT".into(),
            temp.path().join("claims").to_string_lossy().into(),
        ),
    ];
    let name = "codex-thread-steer";
    // A seed long enough that the follow-up lands mid-turn.
    let seed = "Count slowly from 1 to 30, writing each number on its own line, then reply DONE.";
    let output = run(
        &env,
        &[
            "spawn",
            "--name",
            name,
            "--harness",
            "codex",
            "--substrate",
            "thread",
            "--cwd",
            cwd.to_str().unwrap(),
            "--",
            seed,
        ],
    );
    assert!(output.status.success(), "thread spawn: {:?}", output);

    let started = Instant::now();
    let followup = run(
        &env,
        &[
            "ask",
            name,
            "In one word: what are you doing right now?",
            "--harness",
            "codex",
        ],
    );
    let elapsed = started.elapsed();
    assert!(followup.status.success(), "follow-up ask: {:?}", followup);
    let stdout = String::from_utf8_lossy(&followup.stdout).into_owned();
    // Either shape is a pass: a steered reply from the shared turn, or the
    // bounded in_flight receipt. What must NOT happen is an error or a block
    // past the ask bound.
    assert!(
        !stdout.trim().is_empty() || stdout.contains("in flight"),
        "follow-up neither replied nor reported in_flight: {stdout:?}"
    );
    assert!(
        elapsed < Duration::from_secs(115),
        "follow-up ask blocked {elapsed:?} - it must not queue behind a whole turn"
    );
    let _ = run(&env, &["stop", name]);
}

/// AC5 + AC6 live probe: stop mid-turn reports `interrupted` (not a bare
/// success over a live turn) and the app-server child is gone by the time the
/// response says stopped.
#[test]
fn codex_thread_stop_mid_turn_interrupts() {
    if !codex_available() {
        eprintln!("skipping: set FNO_JOURNEY_CODEX=1 with codex on PATH to run the live journey");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("stop-worktree");
    std::fs::create_dir_all(&cwd).unwrap();
    let agents_home = temp.path().join("agents");
    let env = vec![
        (
            "FNO_AGENTS_HOME".into(),
            agents_home.to_string_lossy().into(),
        ),
        ("FNO_AGENTS_DAEMON_BIN".into(), DAEMON.into()),
        (
            "FNO_CLAIMS_ROOT".into(),
            temp.path().join("claims").to_string_lossy().into(),
        ),
    ];
    let name = "codex-thread-stop";
    let seed = "Count slowly from 1 to 100, one number per line, then reply DONE.";
    let output = run(
        &env,
        &[
            "spawn",
            "--name",
            name,
            "--harness",
            "codex",
            "--substrate",
            "thread",
            "--cwd",
            cwd.to_str().unwrap(),
            "--",
            seed,
        ],
    );
    assert!(output.status.success(), "thread spawn: {:?}", output);
    std::thread::sleep(Duration::from_secs(8)); // let the turn reach driving

    let home = AgentsHome::at(agents_home.clone());
    let pid = registry_row(&home, name)["pid"]
        .as_u64()
        .expect("child pid");
    let stop = run(&env, &["stop", name]);
    assert!(stop.status.success(), "stop: {:?}", stop);
    let stdout = String::from_utf8_lossy(&stop.stdout).into_owned();
    assert!(
        stdout.contains("interrupted") || stdout.contains("timeout-child-killed"),
        "stop must name the interrupt outcome: {stdout:?}"
    );
    let row = registry_row(&home, name);
    assert_eq!(row["status"], "exited", "row must read Exited: {row}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !alive {
            return; // child gone after a reported stop: the AC5 contract
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("app-server child {pid} outlived a reported stop");
}
