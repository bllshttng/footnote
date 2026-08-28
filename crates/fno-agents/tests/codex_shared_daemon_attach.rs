//! The shared codex app-server daemon owns a thread; fno is one of its clients.
//!
//! Three properties, and the reason each is here:
//!
//! - WebSocket-or-nothing. The control socket answers an HTTP upgrade and
//!   nothing else, so a client that writes newline-delimited JSON straight at
//!   it gets its connection closed. That single fact is the whole difference
//!   between the shared-daemon lane and the private-child lane the driver used
//!   to run, and it is why one connect helper is shared rather than copied.
//! - A missing socket reads as `no-daemon` from the one helper, so every call
//!   site reports the same thing.
//! - The daemon outlives the client that started a thread. Asserted as a
//!   POSITIVE marker: a fresh connection must LIST the thread id. "No second
//!   app-server is running" would pass just as well against a daemon that
//!   never started, so it is never the assertion.
//!
//! The TUI half of this contract (`codex resume <id> --remote unix://<sock>`
//! draws the real codex interface on that thread) needs a PTY and is not a
//! cargo test. It is run at a terminal and recorded in
//! `docs/architecture/codex-thread-driver.md`.

use fno_agents::codex_inject::{
    codex_app_server_socket_path, connect_shared_app_server, discover_loaded_threads,
};
use fno_agents::codex_thread::thread_start_request_json;
use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::Message;

/// AC7-ERR: a socket that is not there is `no-daemon` from the shared helper,
/// which is the same reason every other call site already reports.
#[tokio::test]
async fn missing_control_socket_reports_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("nothing-here.sock");
    let reason = connect_shared_app_server(&absent).await.err();
    assert_eq!(reason, Some("no-daemon"));
}

/// AC5-ERR: newline-delimited JSON with no WebSocket upgrade is refused by the
/// transport. The listener speaks the same `accept_async` the daemon does.
#[tokio::test]
async fn ndjson_without_a_websocket_upgrade_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("control.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // A WebSocket server hands a non-upgrading client an error, never a
        // JSON-RPC response.
        tokio_tungstenite::accept_async(stream).await.err().is_some()
    });

    let mut raw = UnixStream::connect(&sock).await.unwrap();
    raw.write_all(b"{\"id\":\"init\",\"method\":\"initialize\"}\n")
        .await
        .unwrap();
    raw.flush().await.unwrap();

    assert!(
        server.await.unwrap(),
        "an NDJSON writer must be refused by the WebSocket transport"
    );
}

/// AC8-HP / AC9-HP: a thread started over the shared control socket is listed
/// by a FRESH connection after the starting connection is dropped. The daemon
/// owns the thread; the client is replaceable.
///
/// Live: needs a running `codex app-server` daemon. Run with
/// `cargo test -p fno-agents --test codex_shared_daemon_attach -- --ignored`.
#[tokio::test]
#[ignore = "requires a live codex app-server daemon"]
async fn shared_daemon_owns_the_thread_after_its_client_disconnects() {
    let sock = codex_app_server_socket_path();
    let (mut sink, mut stream) = connect_shared_app_server(&sock)
        .await
        .expect("shared control socket must accept a client");

    let cwd = std::env::current_dir().unwrap();
    sink.send(Message::Text(
        thread_start_request_json(cwd.to_str().unwrap(), "on-request").into(),
    ))
    .await
    .unwrap();

    let mut thread_id = String::new();
    while let Some(Ok(Message::Text(text))) = stream.next().await {
        let frame: serde_json::Value = match serde_json::from_str(&text) {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        if frame.get("id") != Some(&serde_json::json!(1)) {
            continue;
        }
        thread_id = frame["result"]["thread"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        break;
    }
    assert!(!thread_id.is_empty(), "thread/start must confirm an id");

    // The starting client goes away. Nothing else does.
    drop(sink);
    drop(stream);

    let loaded = discover_loaded_threads()
        .await
        .expect("a fresh connection must reach the daemon");
    assert!(
        loaded.iter().any(|t| t.session_id == thread_id),
        "the daemon must still list {thread_id} after its client disconnected; listed: {:?}",
        loaded.iter().map(|t| &t.session_id).collect::<Vec<_>>()
    );
}
