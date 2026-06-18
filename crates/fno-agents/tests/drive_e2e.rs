//! End-to-end drive tests with real subprocesses (Wave 4, ab-8d258ddb).
//!
//! Brings up a real daemon + worker, spawns a `cat` agent (echoes its stdin
//! back onto the PTY), then drives it over a real WebSocket upgrade on the
//! supervisor socket: resize handshake, keystroke -> PTY -> output roundtrip,
//! state.json drive-window lifecycle, and the drive_attached/detached audit
//! events. No monkeypatching.

use fno_agents::client::call;
use fno_agents::paths::AgentsHome;
use fno_agents::protocol::{read_response, write_request, Request};
use fno_agents::state;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

const DAEMON_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-daemon");
const WORKER_BIN: &str = env!("CARGO_BIN_EXE_fno-agents-worker");

fn short_home() -> AgentsHome {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    AgentsHome::at(PathBuf::from(format!(
        "/tmp/abid{}_{}",
        std::process::id(),
        n
    )))
}

fn start_daemon(home: &AgentsHome) -> std::process::Child {
    let child = Command::new(DAEMON_BIN)
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600")
        .spawn()
        .expect("daemon spawns");
    wait_for(&home.supervisor_sock(), Duration::from_secs(10));
    child
}

fn wait_for(path: &Path, budget: Duration) {
    let start = Instant::now();
    while !path.exists() && start.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(path.exists(), "path never appeared: {}", path.display());
}

fn events_contains(home: &AgentsHome, needle: &str) -> bool {
    std::fs::read_to_string(home.events_jsonl())
        .map(|s| s.contains(needle))
        .unwrap_or(false)
}

/// Drive a cat agent: keystrokes echo back, and the full session lifecycle
/// (attach event + window, output roundtrip, clean detach event + window
/// clear) is observable.
#[tokio::test]
async fn drive_roundtrips_input_output_and_manages_window_and_events() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);

    let mut daemon = start_daemon(&home);

    // Spawn a cat agent: stdin is echoed onto the PTY, so a driven keystroke
    // comes back as output.
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "driveme", "provider": "codex", "argv": ["cat"]}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());
    let short_id = resp.result().unwrap()["short_id"]
        .as_str()
        .unwrap()
        .to_string();

    // --- Open the drive: agent.drive RPC -> ack -> WS upgrade. ---
    let mut conn = UnixStream::connect(home.supervisor_sock()).await.unwrap();
    write_request(
        &mut conn,
        &Request::new(
            2,
            "agent.drive",
            json!({"name": "driveme", "mode": "interactive"}),
        ),
    )
    .await
    .unwrap();
    let ack = read_response(&mut conn).await.unwrap();
    assert!(!ack.is_err(), "drive ack errored: {:?}", ack.error());
    let session_id = ack.result().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(ack.result().unwrap()["mode"], "interactive");

    // drive_attached must be emitted BEFORE any frame (ordering invariant).
    assert!(
        events_contains(&home, "drive_attached"),
        "drive_attached not emitted before frames"
    );

    let (ws, _http) = tokio_tungstenite::client_async("ws://localhost/drive", conn)
        .await
        .expect("ws upgrade");
    let (mut sink, mut source) = ws.split();

    // Initial resize handshake, then a keystroke.
    sink.send(Message::Text(
        json!({"t": "resize", "rows": 30, "cols": 100})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    sink.send(Message::Binary(b"echo-roundtrip\n".to_vec().into()))
        .await
        .unwrap();

    // The state.json drive window is set with this session while driving.
    let mut window_seen = false;
    for _ in 0..50 {
        if let Ok(Some(st)) = state::load_state(&home.state_json(&short_id)) {
            if let Some(d) = st.pty.and_then(|p| p.drive) {
                if d.session_id.as_deref() == Some(&session_id)
                    && d.mode.as_deref() == Some("interactive")
                {
                    window_seen = true;
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(window_seen, "drive window never reflected the session");

    // Read output frames until cat echoes our keystroke back.
    let mut acc: Vec<u8> = Vec::new();
    let mut echoed = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), source.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => {
                acc.extend_from_slice(&b);
                if acc
                    .windows(b"echo-roundtrip".len())
                    .any(|w| w == b"echo-roundtrip")
                {
                    echoed = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {}
        }
    }
    assert!(echoed, "driven keystroke never echoed back as PTY output");

    // --- Clean detach via the sentinel control frame. ---
    sink.send(Message::Text(
        json!({"t": "detach", "reason": "user_sentinel"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    drop(sink);
    drop(source);

    // The window clears and drive_detached is emitted.
    let mut cleared = false;
    for _ in 0..50 {
        if let Ok(Some(st)) = state::load_state(&home.state_json(&short_id)) {
            if st.pty.and_then(|p| p.drive).is_none() {
                cleared = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cleared, "drive window never cleared after detach");
    assert!(
        events_contains(&home, "drive_detached"),
        "drive_detached not emitted"
    );
    assert!(
        events_contains(&home, "user_sentinel"),
        "detach reason not recorded"
    );

    // Cleanup.
    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(9, "agent.stop", json!({"name": "driveme"})),
    )
    .await;
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// A second interactive driver is refused while the first is active (exit 18 /
/// Busy), and a Claude agent (no worker / PTY) cannot be driven.
#[tokio::test]
async fn drive_refuses_second_driver_and_non_pty_agent() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "solo", "provider": "codex", "argv": ["cat"]}),
        ),
    )
    .await
    .expect("spawn");
    assert!(!resp.is_err());

    // First driver attaches and stays connected.
    let mut conn = UnixStream::connect(home.supervisor_sock()).await.unwrap();
    write_request(
        &mut conn,
        &Request::new(
            2,
            "agent.drive",
            json!({"name": "solo", "mode": "interactive"}),
        ),
    )
    .await
    .unwrap();
    let ack = read_response(&mut conn).await.unwrap();
    assert!(!ack.is_err());
    let (ws, _h) = tokio_tungstenite::client_async("ws://localhost/drive", conn)
        .await
        .unwrap();
    let (mut sink, _src) = ws.split();
    sink.send(Message::Text(
        json!({"t": "resize", "rows": 24, "cols": 80})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    // Keep the heartbeat fresh so the first driver is not "stale".
    sink.send(Message::Text(json!({"t": "ping"}).to_string().into()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second driver is refused with Busy.
    let resp2 = call(
        &home,
        &daemon_bin,
        &Request::new(
            3,
            "agent.drive",
            json!({"name": "solo", "mode": "interactive"}),
        ),
    )
    .await
    .expect("second drive call");
    assert!(resp2.is_err(), "second driver should be refused");
    assert_eq!(
        resp2.error().unwrap().code,
        fno_agents::protocol::ErrorCode::Busy
    );

    // Driving a non-existent agent yields AgentNotFound.
    let resp3 = call(
        &home,
        &daemon_bin,
        &Request::new(
            4,
            "agent.drive",
            json!({"name": "ghost", "mode": "interactive"}),
        ),
    )
    .await
    .expect("ghost drive call");
    assert!(resp3.is_err());
    assert_eq!(
        resp3.error().unwrap().code,
        fno_agents::protocol::ErrorCode::AgentNotFound
    );

    drop(sink);
    // `solo` still has a controlling driver attached (dropping the client sink
    // does not synchronously detach server-side), so a plain stop now refuses
    // (US6.7). Force-stop to evict the driver and reap the worker — otherwise
    // the leaked worker inherits the test harness's stdout pipe and hangs cargo.
    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(9, "agent.stop", json!({"name": "solo", "force": true})),
    )
    .await;
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// Spawn the daemon with a long heartbeat (watchdog will NOT fire within the
/// test window) and a short stale-takeover threshold, so the takeover-after-
/// stale branch is reachable fast. This deliberately INVERTS the production
/// ordering (where `STALE_DRIVER_IDLE` > `HEARTBEAT_TIMEOUT`, making takeover an
/// orphan-recovery fallback the watchdog normally pre-empts); the inversion is
/// what lets the e2e exercise the eviction/event/slot-reassignment mechanism
/// without an orphaned handle or a 30s wait.
fn start_daemon_long_hb_short_stale(home: &AgentsHome) -> std::process::Child {
    let child = Command::new(DAEMON_BIN)
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600")
        .env("FNO_AGENTS_DRIVE_HEARTBEAT_MS", "60000")
        .env("FNO_AGENTS_DRIVE_STALE_MS", "400")
        .spawn()
        .expect("daemon spawns");
    wait_for(&home.supervisor_sock(), Duration::from_secs(10));
    child
}

/// A new controlling driver takes over an idle (stale) existing driver: the old
/// session is force-closed with reason `takeover_after_stale`, the
/// `drive_takeover_after_stale` event is emitted, and the new driver is admitted
/// (Wave 4 task 4.2, ab-a2395a59). The first driver goes silent but the long
/// heartbeat keeps the watchdog from pre-empting, isolating the takeover path.
#[tokio::test]
async fn drive_takeover_after_stale_evicts_idle_driver() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon_long_hb_short_stale(&home);

    // `sleep` stays alive and never touches stdin, so neither the agent exiting
    // nor the heartbeat watchdog (60s) can end the first session out from under
    // the takeover under test.
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "idle1", "provider": "codex", "argv": ["sleep", "600"]}),
        ),
    )
    .await
    .expect("spawn");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());

    // First driver attaches (interactive => opens the authority window), sends
    // the resize handshake, then goes silent. Resize does NOT bump the
    // heartbeat clock (only attach + `ping` do), so the driver ages toward stale.
    let mut conn = UnixStream::connect(home.supervisor_sock()).await.unwrap();
    write_request(
        &mut conn,
        &Request::new(
            2,
            "agent.drive",
            json!({"name": "idle1", "mode": "interactive"}),
        ),
    )
    .await
    .unwrap();
    let ack = read_response(&mut conn).await.unwrap();
    assert!(!ack.is_err(), "first drive refused: {:?}", ack.error());
    let (ws, _h) = tokio_tungstenite::client_async("ws://localhost/drive", conn)
        .await
        .unwrap();
    let (mut sink, _src) = ws.split();
    sink.send(Message::Text(
        json!({"t": "resize", "rows": 24, "cols": 80})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // Let the first driver age past the 400ms stale threshold.
    tokio::time::sleep(Duration::from_millis(700)).await;

    // Second driver attaches: admission finds the first driver stale and takes
    // over. `call` reads the ack only (no WS upgrade); the takeover eviction +
    // event are emitted inside admit(), BEFORE the ack, so a non-error ack here
    // proves the takeover fired.
    let resp2 = call(
        &home,
        &daemon_bin,
        &Request::new(
            3,
            "agent.drive",
            json!({"name": "idle1", "mode": "interactive"}),
        ),
    )
    .await
    .expect("second drive call");
    assert!(
        !resp2.is_err(),
        "second driver should take over a stale driver, got: {:?}",
        resp2.error()
    );

    // The takeover event is emitted at admission time.
    let mut saw_takeover = false;
    for _ in 0..50 {
        if events_contains(&home, "drive_takeover_after_stale") {
            saw_takeover = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_takeover,
        "drive_takeover_after_stale event not emitted after stale takeover"
    );

    drop(sink);
    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(9, "agent.stop", json!({"name": "idle1", "force": true})),
    )
    .await;
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// Spawn the daemon with a short heartbeat timeout so the watchdog fires fast.
fn start_daemon_short_heartbeat(home: &AgentsHome) -> std::process::Child {
    let child = Command::new(DAEMON_BIN)
        .env("FNO_AGENTS_HOME", home.root())
        .env("FNO_AGENTS_WORKER_BIN", WORKER_BIN)
        .env("FNO_AGENTS_IDLE_EXIT_SECS", "3600")
        .env("FNO_AGENTS_DRIVE_HEARTBEAT_MS", "500")
        .spawn()
        .expect("daemon spawns");
    wait_for(&home.supervisor_sock(), Duration::from_secs(10));
    child
}

/// A driver that stops pinging is evicted by the heartbeat watchdog: the window
/// clears and drive_detached{reason:heartbeat_lost} is emitted (AC3-EDGE-2).
#[tokio::test]
async fn drive_heartbeat_watchdog_evicts_silent_driver() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon_short_heartbeat(&home);

    // Use a long-lived `sleep` (not `cat`): this test only needs an agent that
    // STAYS ALIVE so the heartbeat watchdog is the thing that ends the session.
    // Under heavy parallel-test load many cat-on-PTY processes contend and one
    // can exit early, racing the watchdog with a child_exited detach; sleep does
    // not interact with stdin and cannot exit out from under the watchdog.
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "silent", "provider": "codex", "argv": ["sleep", "600"]}),
        ),
    )
    .await
    .expect("spawn");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());
    let short_id = resp.result().unwrap()["short_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut conn = UnixStream::connect(home.supervisor_sock()).await.unwrap();
    write_request(
        &mut conn,
        &Request::new(
            2,
            "agent.drive",
            json!({"name": "silent", "mode": "interactive"}),
        ),
    )
    .await
    .unwrap();
    let ack = read_response(&mut conn).await.unwrap();
    assert!(!ack.is_err());
    let (ws, _h) = tokio_tungstenite::client_async("ws://localhost/drive", conn)
        .await
        .unwrap();
    let (mut sink, mut source) = ws.split();
    // Resize, then go silent: never send a ping. The 1200ms watchdog fires.
    sink.send(Message::Text(
        json!({"t": "resize", "rows": 24, "cols": 80})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // Drain output frames until the daemon force-closes the connection.
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), source.next()).await {
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break, // server closed the WS
            Err(_) => {}
        }
    }

    // Window cleared + heartbeat_lost detach event.
    let mut cleared = false;
    for _ in 0..50 {
        if let Ok(Some(st)) = state::load_state(&home.state_json(&short_id)) {
            if st.pty.and_then(|p| p.drive).is_none() {
                cleared = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(cleared, "window never cleared after heartbeat loss");
    // Poll for the detach event: the daemon emits drive_detached AFTER the WS
    // closes (which is what ended our drain loop), so a single read can race it.
    let mut saw_heartbeat_lost = false;
    for _ in 0..50 {
        if events_contains(&home, "heartbeat_lost") {
            saw_heartbeat_lost = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !saw_heartbeat_lost {
        let ev = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        let detaches: Vec<&str> = ev
            .lines()
            .filter(|l| l.contains("drive_detached") || l.contains("agent_exited"))
            .collect();
        panic!("expected heartbeat_lost; detach/exit events were: {detaches:?}");
    }
    assert!(
        saw_heartbeat_lost,
        "drive_detached{{heartbeat_lost}} not emitted"
    );

    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(9, "agent.stop", json!({"name": "silent"})),
    )
    .await;
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// Watch mode: input is rejected server-side (audit event, connection stays
/// open), no state.json drive window opens (no authority, LD24), and multiple
/// watchers coexist.
#[tokio::test]
async fn drive_watch_rejects_input_opens_no_window_and_allows_multiple() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "watched", "provider": "codex", "argv": ["sleep", "600"]}),
        ),
    )
    .await
    .expect("spawn");
    assert!(!resp.is_err());
    let short_id = resp.result().unwrap()["short_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Two watchers attach and coexist (AC4-UI).
    let watcher = |id: u64| {
        let home = home.clone();
        async move {
            let mut conn = UnixStream::connect(home.supervisor_sock()).await.unwrap();
            write_request(
                &mut conn,
                &Request::new(
                    id,
                    "agent.drive",
                    json!({"name": "watched", "mode": "watch"}),
                ),
            )
            .await
            .unwrap();
            let ack = read_response(&mut conn).await.unwrap();
            assert!(!ack.is_err(), "watch attach errored: {:?}", ack.error());
            tokio_tungstenite::client_async("ws://localhost/drive", conn)
                .await
                .unwrap()
                .0
        }
    };
    let w1 = watcher(2).await;
    let w2 = watcher(3).await;
    let (mut s1, mut src1) = w1.split();
    let (mut s2, _src2) = w2.split();
    s1.send(Message::Text(
        json!({"t": "resize", "rows": 24, "cols": 80})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    s2.send(Message::Text(
        json!({"t": "resize", "rows": 24, "cols": 80})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // Watch never opens a state.json drive window (no authority).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let st = state::load_state(&home.state_json(&short_id))
        .unwrap()
        .unwrap();
    assert!(
        st.pty.and_then(|p| p.drive).is_none(),
        "watch must NOT open a drive window"
    );

    // A watcher that sends input is rejected server-side; the connection stays
    // open (we can still receive output afterward).
    s1.send(Message::Binary(b"illegal-input\n".to_vec().into()))
        .await
        .unwrap();
    let mut rejected = false;
    for _ in 0..50 {
        if events_contains(&home, "drive_watch_input_rejected") {
            rejected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(rejected, "drive_watch_input_rejected not emitted");
    // Connection still alive: a ping gets a pong.
    s1.send(Message::Text(json!({"t": "ping"}).to_string().into()))
        .await
        .unwrap();
    let mut ponged = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), src1.next()).await {
            Ok(Some(Ok(Message::Text(t)))) if t.contains("pong") => {
                ponged = true;
                break;
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {}
        }
    }
    assert!(ponged, "watch connection closed after input rejection");

    drop(s1);
    drop(s2);
    let _ = call(
        &home,
        &daemon_bin,
        &Request::new(9, "agent.stop", json!({"name": "watched"})),
    )
    .await;
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}

/// Lifecycle refusal while a driver is active (Wave 5, US6.7 / US6.8):
/// `stop` without `--force` is refused (exit 18 / Busy), `rm --force` is refused
/// UNCONDITIONALLY, and `stop --force` evicts the driver
/// (`drive_detached{reason:"stop_force"}`) then stops the agent.
#[tokio::test]
async fn stop_and_rm_respect_active_driver() {
    let home = short_home();
    home.ensure_root().unwrap();
    let daemon_bin = PathBuf::from(DAEMON_BIN);
    std::env::set_var("FNO_AGENTS_WORKER_BIN", WORKER_BIN);
    let mut daemon = start_daemon(&home);

    // Spawn a cat agent and attach an interactive driver.
    let resp = call(
        &home,
        &daemon_bin,
        &Request::new(
            1,
            "agent.spawn",
            json!({"name": "guarded", "provider": "codex", "argv": ["cat"]}),
        ),
    )
    .await
    .expect("spawn call");
    assert!(!resp.is_err(), "spawn failed: {:?}", resp.error());

    let mut conn = UnixStream::connect(home.supervisor_sock()).await.unwrap();
    write_request(
        &mut conn,
        &Request::new(
            2,
            "agent.drive",
            json!({"name": "guarded", "mode": "interactive"}),
        ),
    )
    .await
    .unwrap();
    let ack = read_response(&mut conn).await.unwrap();
    assert!(!ack.is_err(), "drive ack errored: {:?}", ack.error());
    let (ws, _http) = tokio_tungstenite::client_async("ws://localhost/drive", conn)
        .await
        .expect("ws upgrade");
    let (mut sink, _source) = ws.split();
    sink.send(Message::Text(
        json!({"t": "resize", "rows": 30, "cols": 100})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();

    // stop without --force is refused with Busy (exit 18).
    let stopped = call(
        &home,
        &daemon_bin,
        &Request::new(3, "agent.stop", json!({"name": "guarded"})),
    )
    .await
    .unwrap();
    assert!(
        stopped.is_err(),
        "stop should refuse while a driver is active"
    );
    assert_eq!(
        stopped.error().unwrap().code,
        fno_agents::protocol::ErrorCode::Busy
    );

    // rm --force is refused UNCONDITIONALLY while a driver is active.
    let removed = call(
        &home,
        &daemon_bin,
        &Request::new(4, "agent.rm", json!({"name": "guarded", "force": true})),
    )
    .await
    .unwrap();
    assert!(
        removed.is_err(),
        "rm --force must still refuse with an active driver"
    );
    assert_eq!(
        removed.error().unwrap().code,
        fno_agents::protocol::ErrorCode::Busy
    );

    // stop --force evicts the driver and stops the agent.
    let forced = call(
        &home,
        &daemon_bin,
        &Request::new(5, "agent.stop", json!({"name": "guarded", "force": true})),
    )
    .await
    .unwrap();
    assert!(
        !forced.is_err(),
        "stop --force failed: {:?}",
        forced.error()
    );

    // The eviction is attributed in the audit trail.
    let mut saw_force = false;
    for _ in 0..50 {
        if events_contains(&home, "stop_force") {
            saw_force = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        saw_force,
        "expected drive_detached{{reason:stop_force}} after stop --force"
    );

    let reg = state::load_registry(&home.registry_json()).unwrap();
    assert_eq!(
        reg.find("guarded").map(|e| e.status),
        Some(fno_agents::AgentStatus::Exited),
        "agent should be exited after stop --force"
    );

    drop(sink);
    let _ = daemon.kill();
    let _ = daemon.wait();
    std::fs::remove_dir_all(home.root()).ok();
}
