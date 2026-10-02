//! The launcher's subprocess journeys: one typed request becomes
//! exactly one canonical `fno agents spawn` attempt, the seed rides stdin
//! verbatim, and the outcome decodes from the door's receipt. `FNO_BIN`
//! points the server at a RECORDING fake door, so the tests pin the exact
//! argv and stdin the real boundary receives without launching a harness.
//!
//! The popups' editor/focus/refusal units are in
//! `src/client/tests/agent_launcher_tests.rs`; the desk/validation units are
//! in `src/server/tests/agent_launcher_tests.rs`.

mod common;
use common::{spawn_server, FakeClient, Scratch};

use std::path::PathBuf;

use fno::proto::{AgentLaunchRequest, ClientMsg};

fn fake_door(dir: &PathBuf, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A recording door: argv to `argv.log`, stdin to `stdin.log`, one pane
/// receipt on stdout, exit 0.
const RECORDING: &str = r#"#!/bin/sh
for a in "$@"; do echo "$a" >> "$RECORD_DIR/argv.log"; done
cat > "$RECORD_DIR/stdin.log"
echo '{"pane_id":9,"name":"fake-worker","seed":"submitted","pane_observation":"painted","mux_session":"work"}'
"#;

/// A refusing door: nonzero exit, named reason on stderr.
const REFUSING: &str = r#"#!/bin/sh
echo "spawn gate: no free lane" >&2
exit 3
"#;

/// An ambiguous door: exit 0 with no readable receipt.
const AMBIGUOUS: &str = r#"#!/bin/sh
echo "not json at all"
"#;

fn attach_and_launch(scratch: &Scratch, sock: &PathBuf) -> FakeClient {
    let mut client = FakeClient::attach(sock, 24, 80, &scratch.home_cwd());
    // Let the attach settle so the launch reply is attributable. The same
    // 15s budget as every other wait here: the server boots while a dozen
    // other worktrees' cargo runs hold the machine, and a 5s budget read as
    // a product failure under exactly that load.
    client.wait(15, "attach layout", |c| c.layout.as_ref().map(|_| ()));
    client
}

fn send_launch(client: &mut FakeClient, scratch: &Scratch, request_id: u64, message: &str) {
    send_launch_with_flags(client, scratch, request_id, message, Vec::new());
}

#[allow(clippy::too_many_arguments)]
fn send_launch_full(
    client: &mut FakeClient,
    scratch: &Scratch,
    request_id: u64,
    message: &str,
    extra_flags: Vec<String>,
    worktree: bool,
    branch: Option<String>,
) {
    client.raw(&ClientMsg::AgentLaunch(AgentLaunchRequest {
        request_id,
        revision: 1,
        cwd: scratch.home_cwd(),
        harness: "claude".to_string(),
        substrate: "pane".to_string(),
        model: None,
        provider: None,
        route: None,
        model_names_harness: false,
        effort: None,
        permission_mode: None,
        placement: None,
        portal: None,
        split: None,
        node: None,
        message: message.to_string(),
        extra_flags,
        worktree,
        branch,
        force: false,
    }));
}

fn send_launch_with_flags(
    client: &mut FakeClient,
    scratch: &Scratch,
    request_id: u64,
    message: &str,
    extra_flags: Vec<String>,
) {
    send_launch_full(
        client,
        scratch,
        request_id,
        message,
        extra_flags,
        false,
        None,
    );
}

#[test]
fn launcher_journey_model_only_pin_omits_harness() {
    // AC5-HP: a routing-row pick (model_names_harness) omits --harness so
    // the door resolves the row's harness, route and account from the model.
    // AC5-EDGE: the false default keeps today's argv, pinned by
    // launcher_journey_argv_stdin_and_birth_decode.
    let scratch = Scratch::new("launcher-journey-modelpin");
    let record_dir = scratch.0.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let door = fake_door(&scratch.0, "fake-fno", RECORDING);
    let sock = scratch.main_sock();
    let _server = spawn_server(
        &sock,
        &[
            ("FNO_BIN", door.to_string_lossy().as_ref()),
            ("RECORD_DIR", record_dir.to_string_lossy().as_ref()),
        ],
    );
    let mut client = attach_and_launch(&scratch, &sock);
    client.raw(&ClientMsg::AgentLaunch(AgentLaunchRequest {
        request_id: 1,
        revision: 1,
        cwd: scratch.home_cwd(),
        harness: "claude".to_string(),
        substrate: String::new(),
        model: Some("glm-5.3-flash[1m]".to_string()),
        provider: None,
        route: None,
        model_names_harness: true,
        effort: None,
        permission_mode: None,
        placement: None,
        portal: None,
        split: None,
        node: None,
        message: "hi".to_string(),
        extra_flags: Vec::new(),
        worktree: false,
        branch: None,
        force: false,
    }));
    client.wait(15, "launch terminal state", |c| {
        c.launch_updates
            .iter()
            .any(|u| !matches!(u.state, fno::proto::LaunchState::Starting))
            .then_some(())
    });
    let argv = std::fs::read_to_string(record_dir.join("argv.log")).unwrap();
    let argv: Vec<String> = argv.lines().map(str::to_string).collect();
    assert!(
        !argv.contains(&"--harness".to_string()),
        "a model-only pin omits --harness: {argv:?}"
    );
    assert!(
        argv.contains(&"--model".to_string()),
        "the model id rides: {argv:?}"
    );
}

/// Wait for ANY terminal state, not one specific variant: a wait pinned to
/// `Refused` turns a server that answered `Unknown` into a bare 15s timeout,
/// which diagnoses nothing. The caller then matches the state it required.
fn wait_terminal(client: &mut FakeClient, what: &str) {
    client.wait(15, what, |c| {
        c.launch_updates
            .iter()
            .any(|u| !matches!(u.state, fno::proto::LaunchState::Starting))
            .then_some(())
    });
}

#[test]
fn launcher_journey_argv_stdin_and_birth_decode() {
    let scratch = Scratch::new("launcher-journey");
    let record_dir = scratch.0.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let door = fake_door(&scratch.0, "fake-fno", RECORDING);
    let sock = scratch.main_sock();
    let _server = spawn_server(
        &sock,
        &[
            ("FNO_BIN", door.to_string_lossy().as_ref()),
            ("RECORD_DIR", record_dir.to_string_lossy().as_ref()),
        ],
    );
    let mut client = attach_and_launch(&scratch, &sock);

    let message = "line one\nsay \"hi\" $HOME `whoami` \u{1f600}";
    send_launch_with_flags(
        &mut client,
        &scratch,
        1,
        message,
        vec!["--agent".into(), "abc".into(), "--name".into(), "x".into()],
    );

    // The exchange: Starting acknowledgment, then the decoded birth.
    client.wait(15, "launch terminal state", |c| {
        c.launch_updates
            .iter()
            .any(|u| !matches!(u.state, fno::proto::LaunchState::Starting))
            .then_some(())
    });
    let states: Vec<_> = client
        .launch_updates
        .iter()
        .map(|u| u.state.clone())
        .collect();
    assert!(
        matches!(states.first(), Some(fno::proto::LaunchState::Starting)),
        "first update is the Starting ack: {states:?}"
    );
    match states.last() {
        Some(fno::proto::LaunchState::Launched {
            name,
            pane,
            seed_delivered,
        }) => {
            assert_eq!(name, "fake-worker");
            assert_eq!(*pane, Some(9));
            assert_eq!(*seed_delivered, Some(true));
        }
        other => panic!("expected Launched, got {other:?}"),
    }

    // The boundary: exact argv + stdin at the door.
    let argv = std::fs::read_to_string(record_dir.join("argv.log")).unwrap();
    let argv: Vec<String> = argv.lines().map(str::to_string).collect();
    let home_cwd = scratch.home_cwd();
    let expect = [
        "agents",
        "spawn",
        "--harness",
        "claude",
        "--cwd",
        home_cwd.as_str(),
        "--substrate",
        "pane",
        "--mux-session",
        "main",
        "--no-wait",
        "--agent",
        "abc",
        "--name",
        "x",
        "--prompt-file",
        "-",
    ];
    let start = argv.len() - expect.len();
    assert_eq!(&argv[start..], expect, "door argv tail: {argv:?}");
    assert!(
        argv.iter().all(|a| a != "yolo" && a != "--force"),
        "no forced gates: {argv:?}"
    );
    let stdin_seen = std::fs::read_to_string(record_dir.join("stdin.log")).unwrap();
    assert_eq!(stdin_seen, message, "the seed arrives verbatim");
}

#[test]
fn launcher_journey_empty_substrate_takes_the_door_default() {
    // AC6-HP: an EMPTY substrate omits --substrate so the door's thread
    // default decides; a thread placement through a portal rides --portal
    // and its geometry flag. The old pane-pinned argv stays pinned by
    // launcher_journey_argv_stdin_and_birth_decode.
    let scratch = Scratch::new("launcher-journey-thread");
    let record_dir = scratch.0.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let door = fake_door(&scratch.0, "fake-fno", RECORDING);
    let sock = scratch.main_sock();
    let _server = spawn_server(
        &sock,
        &[
            ("FNO_BIN", door.to_string_lossy().as_ref()),
            ("RECORD_DIR", record_dir.to_string_lossy().as_ref()),
        ],
    );
    let mut client = attach_and_launch(&scratch, &sock);
    client.raw(&ClientMsg::AgentLaunch(AgentLaunchRequest {
        request_id: 1,
        revision: 1,
        cwd: scratch.home_cwd(),
        harness: "claude".to_string(),
        substrate: String::new(),
        model: None,
        provider: None,
        route: None,
        model_names_harness: false,
        effort: None,
        permission_mode: None,
        placement: None,
        portal: Some(1),
        split: Some("right".into()),
        node: None,
        message: "hi".to_string(),
        extra_flags: Vec::new(),
        worktree: false,
        branch: None,
        force: false,
    }));
    client.wait(15, "launch terminal state", |c| {
        c.launch_updates
            .iter()
            .any(|u| !matches!(u.state, fno::proto::LaunchState::Starting))
            .then_some(())
    });
    let argv = std::fs::read_to_string(record_dir.join("argv.log")).unwrap();
    let argv: Vec<String> = argv.lines().map(str::to_string).collect();
    let home_cwd = scratch.home_cwd();
    let expect = [
        "agents",
        "spawn",
        "--harness",
        "claude",
        "--cwd",
        home_cwd.as_str(),
        "--no-wait",
        "--portal",
        "1",
        "--split",
        "right",
        "--prompt-file",
        "-",
    ];
    let start = argv.len() - expect.len();
    assert_eq!(&argv[start..], expect, "door argv tail: {argv:?}");
}

#[test]
fn launcher_journey_node_prefill_rides_the_door() {
    // AC5-HP: a board prefill's node rides the canonical spawn as --node
    // (right after the --cwd pair) while the message still arrives
    // verbatim on stdin - the door records both, nothing else moves.
    let scratch = Scratch::new("launcher-journey-node");
    let record_dir = scratch.0.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let door = fake_door(&scratch.0, "fake-fno", RECORDING);
    let sock = scratch.main_sock();
    let _server = spawn_server(
        &sock,
        &[
            ("FNO_BIN", door.to_string_lossy().as_ref()),
            ("RECORD_DIR", record_dir.to_string_lossy().as_ref()),
        ],
    );
    let mut client = attach_and_launch(&scratch, &sock);
    let message = "/fno:target x-1";
    client.raw(&ClientMsg::AgentLaunch(AgentLaunchRequest {
        request_id: 1,
        revision: 1,
        cwd: scratch.home_cwd(),
        harness: "claude".to_string(),
        substrate: String::new(),
        model: None,
        provider: None,
        route: None,
        model_names_harness: false,
        effort: None,
        permission_mode: None,
        placement: None,
        portal: None,
        split: None,
        node: Some("x-1".to_string()),
        message: message.to_string(),
        extra_flags: Vec::new(),
        worktree: false,
        branch: None,
        force: false,
    }));
    client.wait(15, "launch terminal state", |c| {
        c.launch_updates
            .iter()
            .any(|u| !matches!(u.state, fno::proto::LaunchState::Starting))
            .then_some(())
    });
    let argv = std::fs::read_to_string(record_dir.join("argv.log")).unwrap();
    let argv: Vec<String> = argv.lines().map(str::to_string).collect();
    let node_pos = argv
        .iter()
        .position(|a| a == "--node")
        .expect("--node rides the argv");
    assert_eq!(argv[node_pos + 1], "x-1", "node id rides: {argv:?}");
    let cwd_pos = argv.iter().position(|a| a == "--cwd").unwrap();
    assert_eq!(node_pos, cwd_pos + 2, "--node follows the --cwd pair");
    let stdin_seen = std::fs::read_to_string(record_dir.join("stdin.log")).unwrap();
    assert_eq!(stdin_seen, message, "the seed arrives verbatim");
}

#[test]
fn launcher_journey_refusal_and_unknown_are_named() {
    let scratch = Scratch::new("launcher-journey-refused");
    let door = fake_door(&scratch.0, "fake-fno", REFUSING);
    let sock = scratch.main_sock();
    let _server = spawn_server(&sock, &[("FNO_BIN", door.to_string_lossy().as_ref())]);
    let mut client = attach_and_launch(&scratch, &sock);
    send_launch(&mut client, &scratch, 1, "hi");
    wait_terminal(&mut client, "launch terminal state (expected Refused)");
    match &client.launch_updates.last().unwrap().state {
        fno::proto::LaunchState::Refused { reason } => {
            assert!(reason.contains("no free lane"), "reason: {reason}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    // Ambiguous: exit 0, unreadable output. Unknown, never a birth.
    let door2 = fake_door(&scratch.0, "fake-fno-ambiguous", AMBIGUOUS);
    let sock2 = scratch.0.join("ambiguous.sock");
    let _server2 = spawn_server(&sock2, &[("FNO_BIN", door2.to_string_lossy().as_ref())]);
    let mut client2 = FakeClient::attach(&sock2, 24, 80, &scratch.home_cwd());
    // Same load budget as attach_and_launch: a second server booting on a
    // busy machine starved 5s once already.
    client2.wait(15, "attach layout", |c| c.layout.as_ref().map(|_| ()));
    send_launch(&mut client2, &scratch, 1, "hi");
    wait_terminal(&mut client2, "launch terminal state (expected Unknown)");
    match &client2.launch_updates.last().unwrap().state {
        fno::proto::LaunchState::Unknown { reason } => {
            assert!(reason.contains("no readable receipt"), "reason: {reason}");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

/// A recording launch-workdir: the payload to `workdir-payload.log`, one
/// `workdir` answer on stdout, exit 0. Non-launch-workdir invocations exit 1
/// so a mid-test `fno-agents` gesture degrades fail-open instead of lying.
const RECORDING_WD: &str = r#"#!/bin/sh
if [ "$1" = "launch-workdir" ]; then
  cat > "$RECORD_DIR/workdir-payload.log"
  echo '{"workdir":"/tmp/fake-wt-x-276b"}'
  exit 0
fi
exit 1
"#;

/// A holding launch-workdir: the valid `hold` answer, exit 0.
const HOLDING_WD: &str = r#"#!/bin/sh
if [ "$1" = "launch-workdir" ]; then
  cat > "$RECORD_DIR/workdir-payload.log"
  echo '{"hold":"no free tree"}'
  exit 0
fi
exit 1
"#;

#[test]
fn launcher_journey_worktree_launch_rides_the_resolved_cwd() {
    // AC7-HP / AC8-HP: a worktree launch resolves its directory through
    // `fno-agents launch-workdir` the door can never see around: the
    // answered workdir replaces --cwd, and the payload carries the minted
    // composer name, the harness and the picked branch.
    let scratch = Scratch::new("launcher-journey-worktree");
    let record_dir = scratch.0.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let door = fake_door(&scratch.0, "fake-fno", RECORDING);
    let wd = fake_door(&scratch.0, "fake-fno-agents", RECORDING_WD);
    let sock = scratch.main_sock();
    let _server = spawn_server(
        &sock,
        &[
            ("FNO_BIN", door.to_string_lossy().as_ref()),
            ("FNO_AGENTS_BIN", wd.to_string_lossy().as_ref()),
            ("RECORD_DIR", record_dir.to_string_lossy().as_ref()),
        ],
    );
    let mut client = attach_and_launch(&scratch, &sock);
    send_launch_full(
        &mut client,
        &scratch,
        1,
        "hi",
        Vec::new(),
        true,
        Some("feature/x".into()),
    );
    wait_terminal(&mut client, "worktree launch terminal state");
    match client.launch_updates.last().map(|u| &u.state) {
        Some(fno::proto::LaunchState::Launched { .. }) => {}
        other => panic!("expected Launched, got {other:?}"),
    }
    let payload = std::fs::read_to_string(record_dir.join("workdir-payload.log")).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        payload.get("recorded_cwd").and_then(|v| v.as_str()),
        Some(scratch.home_cwd().as_str()),
        "the project cwd rides the payload: {payload}"
    );
    assert!(
        payload
            .get("node")
            .and_then(|v| v.as_str())
            .is_some_and(|n| n.starts_with("composer-")),
        "a bare launch mints a composer- name: {payload}"
    );
    assert_eq!(
        payload.get("harness").and_then(|v| v.as_str()),
        Some("claude"),
        "{payload}"
    );
    assert_eq!(
        payload.get("branch").and_then(|v| v.as_str()),
        Some("feature/x"),
        "{payload}"
    );
    let argv = std::fs::read_to_string(record_dir.join("argv.log")).unwrap();
    let argv: Vec<String> = argv.lines().map(str::to_string).collect();
    let cwd_pos = argv.iter().position(|a| a == "--cwd").expect("--cwd rides");
    assert_eq!(
        argv.get(cwd_pos + 1).map(String::as_str),
        Some("/tmp/fake-wt-x-276b"),
        "the answered workdir replaces --cwd: {argv:?}"
    );
}

#[test]
fn launcher_journey_worktree_hold_refuses_without_a_spawn() {
    // AC7-ERR: a hold answer is a definitive no-birth refusal: the composer
    // reads `refused: worktree: <reason>` and no spawn subprocess runs.
    let scratch = Scratch::new("launcher-journey-worktree-hold");
    let record_dir = scratch.0.join("records");
    std::fs::create_dir_all(&record_dir).unwrap();
    let door = fake_door(&scratch.0, "fake-fno", RECORDING);
    let wd = fake_door(&scratch.0, "fake-fno-agents", HOLDING_WD);
    let sock = scratch.main_sock();
    let _server = spawn_server(
        &sock,
        &[
            ("FNO_BIN", door.to_string_lossy().as_ref()),
            ("FNO_AGENTS_BIN", wd.to_string_lossy().as_ref()),
            ("RECORD_DIR", record_dir.to_string_lossy().as_ref()),
        ],
    );
    let mut client = attach_and_launch(&scratch, &sock);
    send_launch_full(&mut client, &scratch, 1, "hi", Vec::new(), true, None);
    wait_terminal(&mut client, "hold terminal state");
    match &client.launch_updates.last().unwrap().state {
        fno::proto::LaunchState::Refused { reason } => {
            assert!(
                reason.contains("worktree: no free tree"),
                "the hold reason rides the refusal: {reason}"
            );
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert!(
        !record_dir.join("argv.log").exists(),
        "a held launch never spawns"
    );
}
