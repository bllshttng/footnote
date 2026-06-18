//! Daemon-side `agent.logs` streaming handler (Category C / ab-d82655d7).
//!
//! Honors Locked Decision #5: `fno agents logs <name> --follow` for codex/gemini
//! reuses the same WebSocket-upgrade transport `drive` established (Wave 4)
//! rather than a bespoke stream. The client prints the initial `--tail` block
//! itself (client-side file read, full parity with Python's one-shot path); this
//! handler streams only the lines appended *after* the connect point, mirroring
//! Python's `_follow_jsonl` (seek to end, 500ms poll, rotation/truncation
//! detection). New bytes are relayed verbatim as binary frames, so the operator's
//! stdout is byte-identical to Python's readline-and-write loop.
//!
//! The only behavioral divergence from Python's client-side poll is the
//! signed-off one (Failure Modes table): if the daemon cannot serve, the client
//! reports it and exits non-zero rather than polling a dead socket. The client
//! lazy-starts the daemon, so this is unreachable in a healthy environment.

use crate::paths::AgentsHome;
use crate::protocol::{write_response, ErrorCode, Request, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

/// Bound the WS handshake so a client that got the ack but never upgrades does
/// not pin the task (mirrors drive's `UPGRADE_HANDSHAKE_TIMEOUT`).
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);
/// Follow poll cadence -- matches Python `_follow_jsonl`'s 500ms (Locked Decision 3).
const POLL: Duration = Duration::from_millis(500);
/// Liveness-ping cadence: bounds how long a dead client can pin the stream task.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// Handle an `agent.logs` connection: resolve the agent's tee'd log path, ack,
/// upgrade to a WebSocket, and stream appended bytes until the client detaches
/// or the file rotates/truncates/disappears.
pub async fn handle_logs(home: &AgentsHome, req: &Request, mut stream: UnixStream) {
    let name = req
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let log_path = registry_log_path(&home.registry_json(), name);
    let log_path = match log_path {
        Some(p) => PathBuf::from(p),
        None => {
            let resp = Response::err(
                req.id,
                ErrorCode::AgentNotFound,
                format!("agent `{name}` has no tee'd log path to follow"),
            );
            let _ = write_response(&mut stream, &resp).await;
            return;
        }
    };

    // Ack so the client switches to the WS handshake.
    let ack = Response::ok(req.id, json!({"streaming": true}));
    if write_response(&mut stream, &ack).await.is_err() {
        return;
    }

    let ws = match tokio::time::timeout(UPGRADE_TIMEOUT, tokio_tungstenite::accept_async(stream))
        .await
    {
        Ok(Ok(ws)) => ws,
        Ok(Err(_)) | Err(_) => return,
    };

    stream_follow(ws, &log_path).await;
}

/// Resolve an agent's tee'd `log_path` from the registry, reading raw JSON so a
/// Python-written registry row (which lacks the Rust `RegistryEntry`'s required
/// `short_id`/`project_root` fields) is tolerated -- the typed `state::Registry`
/// would fail to deserialize it and silently yield zero agents. Rows live under
/// the top-level `"agents"` key (Python `write_registry`); `"entries"` is the
/// fallback for a registry last written by the Rust daemon.
fn registry_log_path(registry_path: &Path, name: &str) -> Option<String> {
    let bytes = std::fs::read(registry_path).ok()?;
    let raw: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()?;
    let rows = raw
        .get("agents")
        .or_else(|| raw.get("entries"))?
        .as_array()?;
    rows.iter()
        .find(|e| e.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|e| e.get("log_path").and_then(Value::as_str))
        .map(str::to_string)
}

/// Read the current `(size, inode)` of `path`, or `None` if it is gone.
fn stat(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.ino()))
}

/// A control frame telling the client to print `msg` to stderr and exit cleanly
/// (exit 0), matching the diagnostics Python's `_follow_jsonl` writes before it
/// returns on rotation / truncation / disappearance.
fn end_frame(msg: String) -> Message {
    Message::Text(json!({"t": "end", "msg": msg}).to_string().into())
}

async fn stream_follow(ws: tokio_tungstenite::WebSocketStream<UnixStream>, path: &Path) {
    let (mut sink, mut source) = ws.split();

    // Open the log once and keep the handle for the whole session (Gemini: a
    // fresh open every 500ms tick is needless I/O). Python `_follow_jsonl`
    // likewise holds one `fh` and detects rotation via a separate `path.stat()`.
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            let _ = sink
                .send(end_frame(format!(
                    "log file disappeared: {}\n",
                    path.display()
                )))
                .await;
            return;
        }
    };
    // Seek to the current end; we stream only lines appended after this point
    // (Python `fh.seek(0, SEEK_END)`). `last_size` tracks the size seen on the
    // previous tick so a truncate-then-refill that regrows past `offset` is
    // still caught (codex P1).
    let (mut offset, inode) = match stat(path) {
        Some(s) => s,
        None => {
            let _ = sink
                .send(end_frame(format!(
                    "log file disappeared: {}\n",
                    path.display()
                )))
                .await;
            return;
        }
    };
    let mut last_size = offset;

    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Liveness ping: a half-open client (peer gone without a close frame) never
    // resolves `source.next()`, so without this the daemon would poll the file
    // forever. A periodic ping fails to send once the OS notices the dead peer,
    // ending the loop and freeing the task + fd (drive uses a heartbeat watchdog
    // for the same purpose).
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break; // client gone
                }
            }
            _ = poll.tick() => {
                let (size, ino) = match stat(path) {
                    Some(s) => s,
                    None => {
                        let _ = sink.send(end_frame(
                            format!("log file disappeared: {}\n", path.display()))).await;
                        break;
                    }
                };
                if ino != inode {
                    let _ = sink.send(end_frame(
                        format!("log file rotated (inode changed): {}\n", path.display()))).await;
                    break;
                }
                if size < offset {
                    let _ = sink.send(end_frame(
                        format!("log file truncated: {}\n", path.display()))).await;
                    break;
                }
                // Truncate-then-refill across polls: the file shrank then regrew
                // past `offset` between ticks, so the `size < offset` guard above
                // misses it and we would read torn/mid-record bytes from a stale
                // offset. Matches Python `_follow_jsonl`'s `st.st_size < last_size`
                // check (codex P1).
                if size < last_size {
                    let _ = sink.send(end_frame(
                        format!("log file truncated (size shrank across poll): {}\n", path.display()))).await;
                    break;
                }
                last_size = size;
                if size > offset {
                    match read_from(&mut file, offset) {
                        Ok(bytes) if !bytes.is_empty() => {
                            offset += bytes.len() as u64;
                            if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                break; // client gone
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            // Transient read error; re-stat next tick.
                        }
                    }
                }
            }
            msg = source.next() => {
                match msg {
                    // The client signals a clean detach (Ctrl-C) with a text frame.
                    Some(Ok(Message::Text(_))) => break,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    let _ = sink.close().await;
}

/// Read bytes from `offset` to EOF on the already-open handle (no re-open).
fn read_from(f: &mut std::fs::File, offset: u64) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}
