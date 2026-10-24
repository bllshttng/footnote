//! `-H muse`: the MSP client, the delivery tracker and the exec parser read
//! frames recorded from `muse serve --provider echo` and `muse exec --json`
//! (Muse Code 1.4.12, tests/fixtures/muse/). The live test runs the real
//! host on the echo provider where `muse` is installed.

use fno_agents::muse::host::Tracker;
use fno_agents::muse::msp::{classify, Client, Incoming, MspError};
use fno_agents::muse::once::{exec_argv, parse_exec};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/muse")
        .join(name)
}

fn frames(name: &str) -> Vec<Value> {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Feed every notification of a fixture to the tracker, in order.
fn replay(tracker: &mut Tracker, frames: &[Value]) {
    for f in frames {
        if let Some(Incoming::Notification { method, params }) = classify(f) {
            tracker.observe(&method, &params);
        }
    }
}

/// A server that replays the recorded frames answers the handshake, the
/// session start and two submits in the order a fresh client asks.
#[test]
fn client_replays_the_handshake_and_both_turn_acks() {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!(
        "cat '{}'; sleep 5",
        fixture("turns.ndjson").display()
    ));
    let mut client = Client::spawn(cmd).unwrap();
    let wait = Duration::from_secs(5);
    let init = client.initialize(wait).unwrap();
    assert_eq!(init["serverInfo"]["version"], "1.4.12");
    let started = client.call("session/start", json!({}), wait).unwrap();
    assert_eq!(
        started["session"]["sessionId"],
        "01a16c76-623c-717d-a78d-e10a6d772c86"
    );
    let first = client.call("turn/start", json!({}), wait).unwrap();
    let second = client.call("turn/start", json!({}), wait).unwrap();
    assert_eq!(
        (
            first["disposition"].as_str(),
            second["disposition"].as_str()
        ),
        (Some("started"), Some("queued"))
    );
    assert_ne!(first["turnId"], second["turnId"]);
    // The session/started notification the calls stepped over stays queued.
    let held = client.next(wait).unwrap();
    assert!(
        matches!(held, Some(Incoming::Notification { method, .. }) if method == "session/started")
    );
    client.close(wait);
}

/// Errors are typed by `data.kind`, never by message text.
#[test]
fn error_frames_map_to_their_typed_kind() {
    let in_use = json!({"jsonrpc":"2.0","id":7,"error":{"code":-32021,"message":"session x is already in use","data":{"kind":"sessionInUse"}}});
    let Some(Incoming::Response {
        id: 7,
        result: Err(e),
    }) = classify(&in_use)
    else {
        panic!("an error frame reads as an error response");
    };
    assert!(matches!(e, MspError::SessionInUse(_)));
    let rejected = json!({"code":-32030,"message":"turn/unqueue command c rejected: missing_run","data":{"kind":"commandRejected","retryable":false}});
    assert_eq!(
        MspError::from_error(&rejected),
        MspError::CommandRejected {
            reason: "missing_run".into(),
            message: "turn/unqueue command c rejected: missing_run".into()
        }
    );
    let bad = json!({"code":-32602,"message":"clientInfo.name","data":{"kind":"invalidParams"}});
    assert!(matches!(
        MspError::from_error(&bad),
        MspError::InvalidParams(_)
    ));
}

/// Our submit was queued behind two other turns. It parks, stays parked
/// while the other turns run, and confirms only on the userMessage item
/// with our commandId and envelope id. A sibling submit that carries the
/// same envelope text under another commandId never confirms.
#[test]
fn a_queued_submit_confirms_only_on_its_own_user_message() {
    let all = frames("turns.ndjson");
    let ours = "01a16c76-640e-7dec-a618-3896e285055e";
    let mut t = Tracker::default();
    t.submit(ours, Some("fmail-aaaa"));
    t.submit("01a16c76-0000-7000-8000-00000000beef", Some("fmail-aaaa"));
    let ack = all
        .iter()
        .find(|f| f["result"]["commandId"] == ours)
        .unwrap();
    t.ack(ours, &Ok(ack["result"].clone()));
    assert_eq!(t.receipt(ours)["state"], "parked");
    let our_item = all
        .iter()
        .position(|f| f["params"]["item"]["commandId"] == ours)
        .unwrap();
    replay(&mut t, &all[..our_item]);
    assert_eq!(
        t.receipt(ours)["state"],
        "parked",
        "other turns never confirm ours"
    );
    replay(&mut t, &all[our_item..]);
    let r = t.receipt(ours);
    assert_eq!(
        (
            r["state"].as_str(),
            r["delivered"].as_bool(),
            r["envelope_id"].as_str()
        ),
        (Some("accepted-new-turn"), Some(true), Some("fmail-aaaa"))
    );
    assert_eq!(r["turn_id"], ours);
    assert_eq!(
        t.receipt("01a16c76-0000-7000-8000-00000000beef")["state"],
        "injected"
    );
}

/// A queued submit whose reclaim won never runs: failed, reason reclaimed.
#[test]
fn an_unqueued_submit_reads_failed_reclaimed() {
    let all = frames("unqueued.ndjson");
    let cid = all[1]["params"]["commandId"].as_str().unwrap();
    let mut t = Tracker::default();
    t.submit(cid, Some("fmail-dddd"));
    t.ack(cid, &Ok(all[0]["result"].clone()));
    replay(&mut t, &all);
    let r = t.receipt(cid);
    assert_eq!(
        (
            r["state"].as_str(),
            r["reason"].as_str(),
            r["delivered"].as_bool()
        ),
        (Some("failed"), Some("reclaimed"), Some(false))
    );
}

/// The once lane renders the capability row's form and reads the turn's
/// `run_terminal`; a stream with none is a failure naming the last record.
#[test]
fn exec_lane_renders_the_row_form_and_reads_the_terminal() {
    let argv = exec_argv(
        "/bin/muse",
        "sid-1",
        &["--provider".into(), "echo".into()],
        Some("never"),
        "-x",
    );
    assert_eq!(
        argv,
        [
            "/bin/muse",
            "exec",
            "--json",
            "--session-id",
            "sid-1",
            "--approval-mode",
            "never",
            "--provider",
            "echo",
            "--",
            "-x"
        ]
    );
    let text = std::fs::read_to_string(fixture("exec.jsonl")).unwrap();
    assert_eq!(
        parse_exec(&text),
        Ok(("completed".to_string(), "echo: env-0009 once".to_string()))
    );
    let cut: String = text
        .lines()
        .filter(|l| !l.contains("run_terminal"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(parse_exec(&cut), Err("run_output_delta".to_string()));
}

/// The real host on the echo provider: a spawn registers the row, mail
/// confirms from Muse's own userMessage record, and SIGINT leaves the
/// session and marks the row Exited with no `muse serve` child left.
#[test]
fn live_echo_thread_takes_mail_and_a_stop() {
    let Ok(muse) = fno_agents::muse::resolve_muse_bin() else {
        eprintln!("skipped: muse is not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("FNO_AGENTS_HOME", tmp.path().join("agents"));
    std::env::set_var(
        "FNO_AGENTS_WORKER_BIN",
        env!("CARGO_BIN_EXE_fno-agents-worker"),
    );
    std::env::set_var("FNO_MUSE_BIN", &muse);
    let home = fno_agents::paths::AgentsHome::from_env();
    let params = json!({"harness_args": ["--provider", "echo", "--no-session-log"]});
    let spawned =
        fno_agents::muse::thread::dispatch_thread(&home, "m1", "", "lead", tmp.path(), &params);
    assert_eq!(spawned.exit_code, 0, "{}", spawned.stderr);
    let row = || {
        let reg = fno_agents::state::load_registry(&home.registry_json()).unwrap();
        reg.find("m1").cloned().unwrap()
    };
    let entry = row();
    let body = "<fno_mail id=\"fmail-0123456789ab\" from=\"lead\">ping</fno_mail>";
    let receipt = fno_agents::muse::thread::deliver(&entry, body);
    assert_eq!(receipt["state"], "accepted-new-turn", "{receipt}");
    assert_eq!(receipt["envelope_id"], "fmail-0123456789ab");
    let pid = entry.pid.unwrap();
    let socket = entry.messaging_socket_path.clone().unwrap();
    let child: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(&socket).with_extension("child")).unwrap(),
    )
    .unwrap();
    let serve = child["pid"].as_i64().unwrap() as libc::pid_t;
    // SAFETY: the host leads its own group (setsid in dispatch_thread).
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(20);
    while row().status != fno_agents::AgentStatus::Exited {
        assert!(
            Instant::now() < deadline,
            "the host did not mark the row Exited"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!Path::new(&socket).exists(), "the host removed its socket");
    // SAFETY: signal 0 sends nothing; it only asks whether the pid exists.
    let alive = unsafe { libc::kill(serve, 0) } == 0;
    assert!(!alive, "muse serve ({serve}) outlived its host");
}
