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

use fno::proto::{AgentLaunchRequest, ClientMsg, ServerMsg};

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
    // Let the attach settle so the launch reply is attributable.
    client.wait(5, "attach layout", |c| c.layout.as_ref().map(|_| ()));
    client
}

fn send_launch(client: &mut FakeClient, scratch: &Scratch, request_id: u64, message: &str) {
    client.raw(&ClientMsg::AgentLaunch(AgentLaunchRequest {
        request_id,
        revision: 1,
        cwd: scratch.home_cwd(),
        harness: "claude".to_string(),
        substrate: "pane".to_string(),
        model: None,
        effort: None,
        permission_mode: None,
        placement: None,
        message: message.to_string(),
    }));
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
    send_launch(&mut client, &scratch, 1, message);

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
fn launcher_journey_refusal_and_unknown_are_named() {
    let scratch = Scratch::new("launcher-journey-refused");
    let door = fake_door(&scratch.0, "fake-fno", REFUSING);
    let sock = scratch.main_sock();
    let _server = spawn_server(&sock, &[("FNO_BIN", door.to_string_lossy().as_ref())]);
    let mut client = attach_and_launch(&scratch, &sock);
    send_launch(&mut client, &scratch, 1, "hi");
    client.wait(15, "refused terminal state", |c| {
        c.launch_updates
            .iter()
            .any(|u| matches!(u.state, fno::proto::LaunchState::Refused { .. }))
            .then_some(())
    });
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
    client2.wait(5, "attach layout", |c| c.layout.as_ref().map(|_| ()));
    send_launch(&mut client2, &scratch, 1, "hi");
    client2.wait(15, "unknown terminal state", |c| {
        c.launch_updates
            .iter()
            .any(|u| matches!(u.state, fno::proto::LaunchState::Unknown { .. }))
            .then_some(())
    });
    match &client2.launch_updates.last().unwrap().state {
        fno::proto::LaunchState::Unknown { reason } => {
            assert!(reason.contains("no readable receipt"), "reason: {reason}");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
