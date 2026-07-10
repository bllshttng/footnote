//! `mail-inject --provider codex`: LIVE delivery into a running codex session
//! over the codex app-server daemon socket (US8, node x-d899). The codex sibling
//! of [`crate::mail_inject`]'s claude `control.sock` path. Python's send path
//! (`_mail_inject_codex`) runs this as a subprocess and falls back to the durable
//! bus ONLY when it reports not-delivered (live-inject-first).
//!
//! Transport = JSON-RPC text frames over a WebSocket over a Unix socket (mirrors
//! [`crate::logs_client`]). Unlike the claude path, the `turn/start` RPC RESPONSE
//! is itself the delivery confirmation (the daemon accepts and queues the turn
//! synchronously), so there is no transcript growth-poll.
//!
//! # The daemon prerequisite (why this can be a no-op)
//!
//! A default `codex` TUI runs its app-server IN-PROCESS with no socket on disk.
//! The socket exists ONLY when a codex app-server daemon is running
//! (`codex remote-control start`, standalone install + ChatGPT login); TUIs
//! launched afterward auto-attach to it. Absent that daemon, `deliver_via_codex_daemon`
//! returns `"no-daemon"` and the caller writes the durable floor. e2e verification
//! needs the user's daemon; the pure builders + `classify_turn_start_response`
//! below are the correct-by-construction unit-tested core.
//!
//! Protocol map verified against `~/code/tools/codex/codex-rs/` (app-server-protocol
//! rpc.rs / v2/turn.rs, app-server-client remote.rs initialize handshake).

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

/// Overall budget for connect + handshake + turn/start round-trip. The daemon
/// responds promptly; this only bounds a wedged socket so the verb cannot hang.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Frame-skip ceiling per read: notifications and unrelated ids are skipped, but
/// a chatty (or silent) socket must not loop forever before the timeout fires.
const MAX_FRAMES: usize = 64;

/// The shared codex app-server control socket: `$CODEX_HOME/app-server-control/
/// app-server-control.sock` (CODEX_HOME defaults to `~/.codex`). Absent unless a
/// codex app-server daemon is running.
pub fn codex_app_server_socket_path() -> PathBuf {
    let home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{}/.codex", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(home)
        .join("app-server-control")
        .join("app-server-control.sock")
}

/// The `initialize` request frame. Local socket needs no auth/pairing — just the
/// handshake. `id` is the string `"init"` (matched on the response). Note the
/// absence of a `"jsonrpc":"2.0"` field: the codex app-server carries bare
/// JSON-RPC text frames.
pub fn initialize_request_json() -> String {
    serde_json::json!({
        "id": "init",
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "fno-mail-inject", "version": "0.1.0"},
            "capabilities": {"experimentalApi": true}
        }
    })
    .to_string()
}

/// The `initialized` notification (no `id`) sent after the initialize response.
pub fn initialized_notification_json() -> String {
    serde_json::json!({"method": "initialized"}).to_string()
}

/// The `turn/start` request injecting `text` into `thread_id` as a text input
/// item. `id` is `1` (matched on the response). `text` is injected verbatim —
/// the `<fno_mail>` envelope is rendered caller-side (Python), so this is a dumb
/// transport, mirroring [`crate::mail_inject`].
pub fn turn_start_request_json(thread_id: &str, text: &str) -> String {
    serde_json::json!({
        "id": 1,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}]
        }
    })
    .to_string()
}

/// Classify a `turn/start` response frame into delivered / not-delivered.
///
/// - `.result.turn.id` is a string -> `Ok(())` (turn accepted; DELIVERED).
/// - `.error` whose message mentions "not found"/"thread" -> `Err("thread-not-loaded")`
///   (the session is embedded / not attached to the daemon -> durable fallback).
/// - anything else (other rpc error, unparseable) -> `Err("rpc-error")`.
///
/// The `Err` value IS the `mail-inject` JSON `reason` token.
pub fn classify_turn_start_response(raw: &str) -> Result<(), &'static str> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Err("rpc-error"),
    };
    if v.get("result")
        .and_then(|r| r.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(|id| id.as_str())
        .is_some()
    {
        return Ok(());
    }
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_lowercase();
        if msg.contains("not found") || msg.contains("thread") {
            return Err("thread-not-loaded");
        }
    }
    Err("rpc-error")
}

/// Deliver `text` into codex `thread_id` over the app-server daemon socket.
/// `Ok(())` == turn accepted (delivered); every `Err(reason)` is a clean
/// not-delivered signal whose value is the `mail-inject` JSON `reason` token.
/// Socket absent -> `Err("no-daemon")`; a wedged socket -> `Err("io-error")`
/// after [`HANDSHAKE_TIMEOUT`].
pub async fn deliver_via_codex_daemon(thread_id: &str, text: &str) -> Result<(), &'static str> {
    let sock = codex_app_server_socket_path();
    if !sock.exists() {
        return Err("no-daemon");
    }
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, inject(&sock, thread_id, text)).await {
        Ok(r) => r,
        Err(_) => Err("io-error"),
    }
}

/// The connect + initialize handshake + `turn/start` round-trip. Split out so
/// [`deliver_via_codex_daemon`] can wrap it in a total timeout.
async fn inject(sock: &Path, thread_id: &str, text: &str) -> Result<(), &'static str> {
    let conn = UnixStream::connect(sock).await.map_err(|_| "io-error")?;
    let ws = match tokio_tungstenite::client_async("ws://localhost/rpc", conn).await {
        Ok((ws, _resp)) => ws,
        Err(_) => return Err("handshake-failed"),
    };
    let (mut sink, mut stream) = ws.split();

    sink.send(Message::Text(initialize_request_json().into()))
        .await
        .map_err(|_| "io-error")?;
    read_until_id(&mut stream, &serde_json::json!("init")).await?;

    sink.send(Message::Text(initialized_notification_json().into()))
        .await
        .map_err(|_| "io-error")?;

    sink.send(Message::Text(
        turn_start_request_json(thread_id, text).into(),
    ))
    .await
    .map_err(|_| "io-error")?;
    let resp = read_until_id(&mut stream, &serde_json::json!(1)).await?;
    classify_turn_start_response(&resp)
}

/// Read Text frames until one whose `id` equals `want`, returning its raw text.
/// Skips notifications (no `id`) and frames for other ids; ignores non-Text
/// frames. Bounded by [`MAX_FRAMES`]; a read error / closed stream is `"io-error"`.
async fn read_until_id<S>(stream: &mut S, want: &serde_json::Value) -> Result<String, &'static str>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..MAX_FRAMES {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if v.get("id") == Some(want) {
                        return Ok(t.to_string());
                    }
                }
            }
            Some(Ok(_)) => {} // non-Text frame (ping/binary/close-less); skip
            Some(Err(_)) | None => return Err("io-error"),
        }
    }
    Err("io-error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_request_is_bare_jsonrpc_with_string_id() {
        let v: serde_json::Value = serde_json::from_str(&initialize_request_json()).unwrap();
        assert_eq!(v["id"], "init");
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["clientInfo"]["name"], "fno-mail-inject");
        // The codex app-server carries bare JSON-RPC frames: no "jsonrpc" field.
        assert!(v.get("jsonrpc").is_none());
    }

    #[test]
    fn initialized_notification_has_no_id() {
        let v: serde_json::Value = serde_json::from_str(&initialized_notification_json()).unwrap();
        assert_eq!(v["method"], "initialized");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn turn_start_carries_thread_id_and_text_item() {
        let v: serde_json::Value =
            serde_json::from_str(&turn_start_request_json("THREAD-9", "hello MARKER")).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "turn/start");
        assert_eq!(v["params"]["threadId"], "THREAD-9");
        assert_eq!(v["params"]["input"][0]["type"], "text");
        assert_eq!(v["params"]["input"][0]["text"], "hello MARKER");
    }

    #[test]
    fn classify_delivered_on_result_turn_id() {
        let raw = r#"{"id":1,"result":{"turn":{"id":"turn-abc","status":"inProgress"}}}"#;
        assert_eq!(classify_turn_start_response(raw), Ok(()));
    }

    #[test]
    fn classify_thread_not_loaded_on_thread_error() {
        let raw = r#"{"id":1,"error":{"code":-32000,"message":"thread not found"}}"#;
        assert_eq!(classify_turn_start_response(raw), Err("thread-not-loaded"));
    }

    #[test]
    fn classify_rpc_error_on_other_error_or_garbage() {
        let other = r#"{"id":1,"error":{"code":-32601,"message":"method not implemented"}}"#;
        assert_eq!(classify_turn_start_response(other), Err("rpc-error"));
        assert_eq!(classify_turn_start_response("not json"), Err("rpc-error"));
        // A result without a string turn id is not a confirmed delivery.
        let no_turn = r#"{"id":1,"result":{}}"#;
        assert_eq!(classify_turn_start_response(no_turn), Err("rpc-error"));
    }

    #[test]
    fn socket_path_honors_codex_home() {
        // Sanity: the tail is fixed; the head follows CODEX_HOME/HOME. We only
        // assert the stable suffix to avoid mutating process env in a unit test.
        let p = codex_app_server_socket_path();
        assert!(p.ends_with("app-server-control/app-server-control.sock"));
    }
}
