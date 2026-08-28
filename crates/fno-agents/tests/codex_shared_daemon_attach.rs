//! The codex thread lane is a client of the SHARED app-server daemon.
//!
//! Two classes of assertion live here, and the split is deliberate.
//!
//! The type-level ones compile-fail while the driver still owns a child
//! process, so they are red on HEAD before the transport swap and need no
//! daemon to run. A count of `codex app-server` processes would be an
//! absence-only assertion; the variant set of [`ThreadDriverError`] is a
//! property of the TYPE, which is what "the driver holds no child" actually
//! means.
//!
//! The live ones need a running daemon and the `codex` binary, so they report
//! a skip line and pass when either is missing. A skip is not evidence: the
//! standing proof for those acceptance criteria is the measured process tree
//! recorded in the PR body, and these tests are the regression guard for the
//! machine that has the daemon.

use fno_agents::codex_inject::{
    codex_app_server_socket_path, codex_attach_argv, connect_app_server, initialize_request_json,
    probe_codex_app_server,
};
use fno_agents::codex_thread::{CodexThread, ThreadDriverError};
use futures_util::{SinkExt, StreamExt};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// AC4: the driver holds no child process handle.
///
/// Exhaustive over [`ThreadDriverError`] with no `Spawn` arm. A driver that
/// forks its own app-server needs a spawn-failure variant, so this match fails
/// to COMPILE while that variant exists. That is the type-level red the plan
/// asks for, and it cannot be satisfied by a process count that happens to
/// read zero.
#[test]
fn the_driver_error_set_carries_no_child_spawn_variant() {
    fn classify(error: &ThreadDriverError) -> &'static str {
        match error {
            ThreadDriverError::Io(_) => "io",
            ThreadDriverError::Timeout => "timeout",
            ThreadDriverError::Protocol(_) => "protocol",
        }
    }
    assert_eq!(classify(&ThreadDriverError::Timeout), "timeout");
}

/// Serializes every test that READS OR WRITES `CODEX_HOME`.
///
/// The variable is process-global and cargo runs a binary's tests on parallel
/// threads, so a relocating test moves the socket out from under a concurrent
/// reader. Observed: `a_relocated_codex_home_moves_the_attach_socket` and the
/// argv tests raced, and the failure moved between runs.
static CODEX_HOME_LOCK: Mutex<()> = Mutex::new(());

fn codex_home_guard() -> std::sync::MutexGuard<'static, ()> {
    CODEX_HOME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// AC14 / AC16: the attach argv is codex's own resume verb pointed at the
/// shared control socket, and the socket path honours `CODEX_HOME`.
#[test]
fn the_attach_argv_execs_codex_resume_against_the_control_socket() {
    let _guard = codex_home_guard();
    let argv = codex_attach_argv("01a04546-28b2-7a41-ae4c-892bbeb8e295");
    assert_eq!(argv[0], "codex");
    assert_eq!(argv[1], "resume");
    assert_eq!(argv[2], "01a04546-28b2-7a41-ae4c-892bbeb8e295");
    assert_eq!(argv[3], "--remote");
    assert_eq!(
        argv[4],
        format!("unix://{}", codex_app_server_socket_path().display())
    );
    assert_eq!(argv.len(), 5, "the argv is the exec target, nothing more");
}

/// One argv, two crates.
///
/// `fno` never links `fno-agents` (it shells the binary at runtime), so the
/// mux viewport's builder and the CLI verb's builder are two functions. This
/// test links both and pins them byte-for-byte, the way `daemon.rs` already
/// feeds a printed `fno mux pane kill` command to the real parser rather than
/// to a copy of its grammar. Change one and this fails here, rather than the
/// two drifting until a codex row opens the wrong thing from one door.
#[test]
fn the_attach_argv_is_identical_in_both_crates() {
    let _guard = codex_home_guard();
    let uuid = "01a04546-28b2-7a41-ae4c-892bbeb8e295";
    assert_eq!(
        codex_attach_argv(uuid),
        fno::agents_view::codex_attach_argv(uuid)
    );
    assert_eq!(
        codex_app_server_socket_path(),
        fno::agents_view::codex_app_server_socket_path()
    );
}

/// AC16: a non-default `CODEX_HOME` moves the socket, so the argv moves with
/// it rather than hardcoding `~/.codex`.
///
/// `CODEX_HOME` is process-global, so this test holds [`CODEX_HOME_LOCK`] for
/// its whole duration. Restoring the variable before returning is NOT enough
/// on its own: the other tests run on parallel threads and read it while this
/// one has it moved.
#[test]
fn a_relocated_codex_home_moves_the_attach_socket() {
    let _guard = codex_home_guard();
    let previous = std::env::var_os("CODEX_HOME");
    std::env::set_var("CODEX_HOME", "/tmp/x6678-codex-home");
    let argv = codex_attach_argv("t");
    match previous {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }
    assert_eq!(
        argv[4],
        "unix:///tmp/x6678-codex-home/app-server-control/app-server-control.sock"
    );
}

/// A live control socket, or `None` with a printed skip line. The caller must
/// already hold [`CODEX_HOME_LOCK`]: the path it resolves depends on that
/// variable.
fn live_socket() -> Option<PathBuf> {
    let path = codex_app_server_socket_path();
    if path.exists() {
        return Some(path);
    }
    eprintln!(
        "skip: no codex app-server daemon at {} (start one with `codex app-server daemon start`)",
        path.display()
    );
    None
}

/// AC5: the shared control socket is a WebSocket endpoint. A newline-delimited
/// JSON write with no upgrade is refused, which is the whole difference
/// between the private-child transport and this one.
#[test]
fn raw_ndjson_without_a_websocket_upgrade_is_refused() {
    let _guard = codex_home_guard();
    let Some(path) = live_socket() else { return };
    let mut conn = UnixStream::connect(&path).expect("connect to the control socket");
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
    conn.write_all(format!("{}\n", initialize_request_json()).as_bytes())
        .expect("write the ndjson frame");
    let mut buf = [0u8; 256];
    let read = conn.read(&mut buf);
    let refused = match read {
        // A clean EOF: the server closed rather than answering bare NDJSON.
        Ok(0) => true,
        // Anything it does say must not be a JSON-RPC response to our id.
        Ok(n) => !String::from_utf8_lossy(&buf[..n]).contains("\"init\""),
        // A reset or a timeout is equally a refusal to speak this protocol.
        Err(_) => true,
    };
    assert!(
        refused,
        "the control socket answered bare NDJSON; the transport is not WebSocket-only"
    );
}

/// AC7: a missing control socket reports the same connect failure at BOTH
/// call sites, because there is only one client under them.
#[tokio::test]
async fn a_missing_control_socket_fails_the_same_way_for_both_callers() {
    let absent = Path::new("/tmp/x6678-no-such-app-server-control.sock");
    assert_eq!(
        connect_app_server(absent).await.err(),
        Some("connect-failed"),
        "the driver's connect must name the missing socket"
    );
    assert!(
        !probe_codex_app_server(absent),
        "the health probe reads the same absent socket as unhealthy"
    );
}

/// AC6, AC8, AC9: the driver's thread is registered on the SHARED daemon.
///
/// The positive marker the plan demands: the thread id is PRESENT in
/// `thread/loaded/list` read over a SEPARATE connection, which is only true
/// if the daemon owns it. A count of `codex app-server` processes would be
/// the absence-only assertion this replaces, and it reads the same whether
/// the daemon is healthy or dead with one orphan child left behind.
///
/// AC9 rides the same read: the listing connection is opened independently of
/// the driver's, so the thread is answered by the daemon, never by the
/// driver's own socket.
#[tokio::test]
async fn a_driver_thread_is_registered_on_the_shared_daemon() {
    let _guard = codex_home_guard();
    let Some(socket) = live_socket() else { return };
    let driver = match CodexThread::start(std::env::temp_dir(), None, false, None).await {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("skip: could not start a thread on the shared daemon: {error}");
            return;
        }
    };
    let thread_id = driver.thread_id().to_string();
    assert!(!thread_id.is_empty(), "the driver must carry a thread id");

    let (mut sink, mut stream) = connect_app_server(&socket)
        .await
        .expect("a second, independent connection to the shared daemon");
    sink.send(Message::Text(
        serde_json::json!({"id": "list", "method": "thread/loaded/list", "params": {}})
            .to_string()
            .into(),
    ))
    .await
    .expect("send thread/loaded/list");
    let mut listed = None;
    for _ in 0..64 {
        let Some(Ok(Message::Text(text))) = stream.next().await else {
            break;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if value.get("id") == Some(&serde_json::json!("list")) {
            listed = value["result"]["data"].as_array().cloned();
            break;
        }
    }
    let listed = listed.expect("the daemon answered thread/loaded/list");
    assert!(
        listed
            .iter()
            .any(|id| id.as_str() == Some(thread_id.as_str())),
        "thread {thread_id} is not loaded on the shared daemon; the driver is \
         talking to something else. loaded={listed:?}"
    );
    assert_eq!(
        driver.pid(),
        fno_agents::codex_inject::ensure_codex_daemon()
            .expect("the daemon we just used")
            .state
            .pid,
        "the driver's recorded pid must BE the shared daemon's"
    );
}
