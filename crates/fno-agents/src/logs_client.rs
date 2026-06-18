//! Client side of `fno agents logs <name> --follow` for codex/gemini
//! (Category C / ab-d82655d7).
//!
//! The caller has already printed the initial `--tail` block (client-side file
//! read). This connects to the daemon, sends the `agent.logs` RPC, upgrades the
//! same stream to a WebSocket, and relays streamed log bytes to stdout until the
//! file rotates/truncates (an `end` control frame) or the operator hits Ctrl-C.
//! Exit is always 0 on a clean stop, matching Python's `read_logs` follow path
//! (which returns `EXIT_OK` on `KeyboardInterrupt` and on rotation/truncation).

use crate::client::{ensure_daemon, resolve_daemon_bin};
use crate::paths::AgentsHome;
use crate::protocol::{read_response, write_request, Request, ResponsePayload};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

/// Stream new log lines for `name` from the daemon. Returns the process exit
/// code (0 on a clean detach / rotation; non-zero only when the daemon cannot
/// be reached -- the signed-off divergence from Python's client-side poll).
pub async fn follow(home: &AgentsHome, name: &str) -> i32 {
    let daemon_bin = resolve_daemon_bin();
    if let Err(e) = ensure_daemon(home, &daemon_bin).await {
        eprintln!("fno agents logs: daemon not running: {e}");
        return 1;
    }

    let mut conn = match UnixStream::connect(home.supervisor_sock()).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fno agents logs: cannot reach daemon: {e}");
            return 1;
        }
    };
    let req = Request::new(1, "agent.logs", json!({"name": name}));
    if let Err(e) = write_request(&mut conn, &req).await {
        eprintln!("fno agents logs: follow request failed: {e}");
        return 1;
    }
    let ack = match read_response(&mut conn).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno agents logs: no follow ack: {e}");
            return 1;
        }
    };
    if let ResponsePayload::Err(err) = ack.payload {
        eprintln!("fno agents logs: {}", err.message);
        return 1;
    }

    let ws = match tokio_tungstenite::client_async("ws://localhost/logs", conn).await {
        Ok((ws, _resp)) => ws,
        Err(e) => {
            eprintln!("fno agents logs: follow upgrade failed: {e}");
            return 1;
        }
    };
    let (mut sink, mut source) = ws.split();
    let mut stdout = tokio::io::stdout();
    let mut sigint = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    {
        Ok(s) => s,
        Err(_) => {
            // No SIGINT handler available; stream until the daemon ends it.
            return relay_until_end(&mut source, &mut stdout).await;
        }
    };

    loop {
        tokio::select! {
            msg = source.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        // Stop on a downstream-closed pipe (e.g. `... --follow | head`)
                        // instead of consuming the stream forever (codex P2 / gemini).
                        if stdout.write_all(&b).await.is_err() || stdout.flush().await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(t))) => {
                        // Daemon end-of-stream control: print its diagnostic to
                        // stderr (matching Python's _follow_jsonl messages) and exit 0.
                        if let Some(m) = parse_end_msg(&t) {
                            eprint!("{m}");
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = sigint.recv() => {
                // Clean detach (Python returns EXIT_OK on KeyboardInterrupt).
                let _ = sink.send(Message::Text(json!({"t": "detach"}).to_string().into())).await;
                let _ = sink.close().await;
                break;
            }
        }
    }
    0
}

/// Parse a daemon `{"t":"end","msg":"..."}` control frame, returning the message.
fn parse_end_msg(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("t").and_then(|t| t.as_str()) == Some("end") {
        Some(
            v.get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        )
    } else {
        None
    }
}

/// Fallback relay used when a SIGINT handler can't be installed.
async fn relay_until_end(
    source: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    stdout: &mut tokio::io::Stdout,
) -> i32 {
    while let Some(msg) = source.next().await {
        match msg {
            Ok(Message::Binary(b)) => {
                if stdout.write_all(&b).await.is_err() || stdout.flush().await.is_err() {
                    break;
                }
            }
            Ok(Message::Text(t)) => {
                if let Some(m) = parse_end_msg(&t) {
                    eprint!("{m}");
                    break;
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    0
}
