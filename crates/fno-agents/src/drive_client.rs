//! Client side of `fno-agents drive` (Wave 4, ab-8d258ddb).
//!
//! Connects to the daemon, sends the `agent.drive` RPC, reads the ack, upgrades
//! the same stream to a WebSocket, puts the local terminal in raw mode, and runs
//! a single select loop bridging stdin <-> the agent's PTY:
//!
//! - local keystrokes -> binary WS frames (interactive); buffered + confirmed
//!   (step / paranoid); suppressed (watch).
//! - PTY output frames -> stdout.
//! - SIGWINCH -> resize control frame; a periodic ping keeps the daemon's
//!   heartbeat watchdog satisfied.
//! - `Ctrl-\ d` (the configurable sentinel) detaches cleanly without forwarding.
//!
//! The two pieces with real logic — the sentinel detector and the step buffer —
//! are pure and unit-tested; the raw-TTY I/O around them is a thin shell.

use crate::client::ensure_daemon;
use crate::drive::DriveMode;
use crate::paths::AgentsHome;
use crate::protocol::{read_response, write_request, ErrorCode, Request, ResponsePayload};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

/// Default detach sentinel: `Ctrl-\` (0x1C) then `d`. Avoids tmux's `Ctrl-B`
/// prefix collision (LD20). Configurable via `FNO_AGENTS_DRIVE_SENTINEL` as two
/// bytes "<lead>,<follow>" (decimal), e.g. "28,100".
const SENTINEL_LEAD: u8 = 0x1C;
const SENTINEL_FOLLOW: u8 = b'd';

/// Detects the two-byte detach sentinel in a keystroke stream, forwarding all
/// other bytes. Holds a lone lead byte across reads so a sentinel split over two
/// `read()` calls is still caught, and a lead NOT followed by the follow byte is
/// forwarded intact (so `Ctrl-\` keeps working for anything that wants it).
#[derive(Debug)]
pub struct SentinelDetector {
    lead: u8,
    follow: u8,
    armed: bool,
}

impl Default for SentinelDetector {
    fn default() -> Self {
        let (lead, follow) = sentinel_from_env();
        SentinelDetector {
            lead,
            follow,
            armed: false,
        }
    }
}

impl SentinelDetector {
    pub fn with_bytes(lead: u8, follow: u8) -> Self {
        SentinelDetector {
            lead,
            follow,
            armed: false,
        }
    }

    /// Feed raw input; returns `(bytes_to_forward, detached)`. When `detached`
    /// is true the sentinel was seen and the lead+follow are consumed (not
    /// forwarded).
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(input.len() + 1);
        for &b in input {
            if self.armed {
                if b == self.follow {
                    self.armed = false;
                    return (out, true);
                }
                // Lead not followed by the follow byte: emit the held lead, then
                // reconsider the current byte (it may itself be a new lead).
                out.push(self.lead);
                if b == self.lead {
                    self.armed = true;
                } else {
                    self.armed = false;
                    out.push(b);
                }
            } else if b == self.lead {
                self.armed = true;
            } else {
                out.push(b);
            }
        }
        (out, false)
    }
}

fn sentinel_from_env() -> (u8, u8) {
    if let Ok(v) = std::env::var("FNO_AGENTS_DRIVE_SENTINEL") {
        let parts: Vec<&str> = v.split(',').collect();
        if parts.len() == 2 {
            if let (Ok(l), Ok(f)) = (parts[0].trim().parse::<u8>(), parts[1].trim().parse::<u8>()) {
                return (l, f);
            }
        }
        eprintln!("fno-agents: invalid FNO_AGENTS_DRIVE_SENTINEL `{v}`; using default Ctrl-\\ d");
    }
    (SENTINEL_LEAD, SENTINEL_FOLLOW)
}

/// Buffers keystrokes into confirmation units for step / paranoid modes.
/// Per-line (`step`): a unit is a line, emitted when Enter (`\r` or `\n`) is
/// seen, newline included. Per-byte (`paranoid`): every byte is its own unit.
#[derive(Debug)]
pub struct StepBuffer {
    per_byte: bool,
    line: Vec<u8>,
}

impl StepBuffer {
    pub fn new(per_byte: bool) -> Self {
        StepBuffer {
            per_byte,
            line: Vec::new(),
        }
    }

    /// Push sentinel-filtered input; return completed units awaiting
    /// confirmation, in order.
    pub fn push(&mut self, input: &[u8]) -> Vec<Vec<u8>> {
        let mut units = Vec::new();
        for &b in input {
            if self.per_byte {
                units.push(vec![b]);
                continue;
            }
            self.line.push(b);
            if b == b'\n' || b == b'\r' {
                units.push(std::mem::take(&mut self.line));
            }
        }
        units
    }

    /// The unsent partial line (shown as the local "pending" preview).
    pub fn pending_preview(&self) -> &[u8] {
        &self.line
    }
}

/// Run an interactive drive session. Returns the process exit code.
pub async fn drive(
    home: &AgentsHome,
    daemon_bin: &std::path::Path,
    name: &str,
    mode: DriveMode,
) -> i32 {
    if let Err(e) = ensure_daemon(home, daemon_bin).await {
        eprintln!("fno-agents: {e}");
        return 1;
    }

    // --- agent.drive RPC + ack on the raw stream, before the upgrade. ---
    let mut conn = match UnixStream::connect(home.supervisor_sock()).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fno-agents: cannot reach daemon: {e}");
            return 1;
        }
    };
    let req = Request::new(
        1,
        "agent.drive",
        json!({"name": name, "mode": mode.as_str()}),
    );
    if let Err(e) = write_request(&mut conn, &req).await {
        eprintln!("fno-agents: drive request failed: {e}");
        return 1;
    }
    let ack = match read_response(&mut conn).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fno-agents: no drive ack: {e}");
            return 1;
        }
    };
    let session_id = match ack.payload {
        ResponsePayload::Err(err) => {
            eprintln!("fno-agents: {}", err.message);
            // Mirror bin/client.rs's verb exit codes for the codes drive returns.
            return match err.code {
                ErrorCode::AgentNotFound | ErrorCode::InvalidStatus => 13,
                ErrorCode::Busy => 18,
                ErrorCode::InvalidParams => 2,
                _ => 1,
            };
        }
        ResponsePayload::Ok(ref v) => v
            .get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
    };

    // --- Upgrade to a WebSocket on the same stream. ---
    let ws = match tokio_tungstenite::client_async("ws://localhost/drive", conn).await {
        Ok((ws, _resp)) => ws,
        Err(e) => {
            eprintln!("fno-agents: drive upgrade failed: {e}");
            return 1;
        }
    };

    let _raw = RawMode::enable();
    let reason = run_loop(ws, mode).await;
    drop(_raw); // restore the terminal before printing the summary
    let _ = session_id; // (carried for future per-session client logging)
    reason.print_summary();
    reason.exit_code()
}

/// The outcome of a drive session, for the exit summary + code.
struct DriveOutcome {
    reason: String,
    duration: Duration,
    keystrokes: u64,
    stepped: u64,
}

impl DriveOutcome {
    fn print_summary(&self) {
        eprintln!(
            "Drive ended: {} ({}s / {} keystrokes / {} step-confirmed)",
            self.reason,
            self.duration.as_secs(),
            self.keystrokes,
            self.stepped,
        );
    }

    fn exit_code(&self) -> i32 {
        match self.reason.as_str() {
            "user_sentinel" | "stdin_eof" | "agent_exited" => 0,
            _ => 1,
        }
    }
}

async fn run_loop(
    ws: tokio_tungstenite::WebSocketStream<UnixStream>,
    mode: DriveMode,
) -> DriveOutcome {
    let (mut sink, mut source) = ws.split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let start = Instant::now();
    let mut sentinel = SentinelDetector::default();
    let mut keystrokes = 0u64;
    let mut stepped = 0u64;

    let per_byte = matches!(mode, DriveMode::Paranoid);
    let stepping = matches!(mode, DriveMode::Step | DriveMode::Paranoid);
    let mut step = StepBuffer::new(per_byte);
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    let mut awaiting_confirm = false;

    // Initial resize handshake (LD18).
    let (rows, cols) = term_size();
    let _ = sink
        .send(Message::Text(
            json!({"t": "resize", "rows": rows, "cols": cols})
                .to_string()
                .into(),
        ))
        .await;

    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();
    let mut ping = tokio::time::interval(Duration::from_secs(3));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut buf = [0u8; 4096];
    let reason = loop {
        tokio::select! {
            n = stdin.read(&mut buf) => {
                match n {
                    Ok(0) => break "stdin_eof".to_string(),
                    Ok(n) => {
                        // Filter the sentinel, but forward any data that preceded
                        // it in the SAME chunk before detaching (dropping it would
                        // silently lose the operator's last keystrokes).
                        let (fwd, detached) = sentinel.feed(&buf[..n]);
                        if !mode.is_watch() && !fwd.is_empty() {
                            if awaiting_confirm {
                                // The first forwarded byte is the confirmation key;
                                // any bytes after it in the same chunk are fresh
                                // step input, not dropped (review LOW #2).
                                if let Some(&key) = fwd.first() {
                                    if key == b'y' || key == b'Y' {
                                        if let Some(unit) = pending.pop_front() {
                                            keystrokes += unit.len() as u64;
                                            stepped += 1;
                                            let _ = sink.send(Message::Binary(unit.into())).await;
                                        }
                                    } else {
                                        pending.pop_front(); // discard on anything else
                                    }
                                    awaiting_confirm = false;
                                    for unit in step.push(&fwd[1..]) {
                                        pending.push_back(unit);
                                    }
                                }
                                maybe_prompt(&mut stdout, &mut pending, &mut awaiting_confirm).await;
                            } else if stepping {
                                for unit in step.push(&fwd) {
                                    pending.push_back(unit);
                                }
                                maybe_prompt(&mut stdout, &mut pending, &mut awaiting_confirm).await;
                            } else {
                                keystrokes += fwd.len() as u64;
                                let _ = sink.send(Message::Binary(fwd.into())).await;
                            }
                        }
                        if detached { break "user_sentinel".to_string(); }
                    }
                    Err(_) => break "stdin_error".to_string(),
                }
            }
            msg = source.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        let _ = stdout.write_all(&b).await;
                        let _ = stdout.flush().await;
                    }
                    Some(Ok(Message::Text(t))) => {
                        if t.contains("child_exited") {
                            break "agent_exited".to_string();
                        }
                        // pong / dropped: nothing to render.
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Ok(Message::Frame(_))) => {
                        break "connection_lost".to_string();
                    }
                    Some(Err(_)) => break "connection_lost".to_string(),
                    Some(Ok(_)) => {}
                }
            }
            _ = async { sigwinch.as_mut().unwrap().recv().await }, if sigwinch.is_some() => {
                let (rows, cols) = term_size();
                let _ = sink.send(Message::Text(
                    json!({"t": "resize", "rows": rows, "cols": cols}).to_string().into())).await;
            }
            _ = ping.tick() => {
                let _ = sink.send(Message::Text(json!({"t": "ping"}).to_string().into())).await;
            }
        }
    };

    // Best-effort clean detach (a lost connection can't be told).
    if reason == "user_sentinel" || reason == "stdin_eof" {
        let _ = sink
            .send(Message::Text(
                json!({"t": "detach", "reason": reason}).to_string().into(),
            ))
            .await;
        let _ = sink.close().await;
    }

    DriveOutcome {
        reason,
        duration: start.elapsed(),
        keystrokes,
        stepped,
    }
}

/// If a unit is queued and we are not already confirming, show the prompt.
async fn maybe_prompt(
    stdout: &mut tokio::io::Stdout,
    pending: &mut VecDeque<Vec<u8>>,
    awaiting: &mut bool,
) {
    if *awaiting {
        return;
    }
    if let Some(unit) = pending.front() {
        let preview = String::from_utf8_lossy(unit);
        let line = format!(
            "\r\n[step] send {} byte(s): {:?} ? [y/N] ",
            unit.len(),
            preview.trim_end()
        );
        let _ = stdout.write_all(line.as_bytes()).await;
        let _ = stdout.flush().await;
        *awaiting = true;
    }
}

/// Current terminal size via `TIOCGWINSZ`, defaulting to 24x80.
fn term_size() -> (u16, u16) {
    // SAFETY: ioctl(TIOCGWINSZ) fills a winsize; we check the return code.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 {
            (ws.ws_row, ws.ws_col)
        } else {
            (24, 80)
        }
    }
}

/// RAII raw-terminal guard. Enabling is a no-op when stdin is not a TTY (piped
/// input, tests), so the client still works headless.
struct RawMode {
    fd: i32,
    orig: Option<libc::termios>,
}

impl RawMode {
    fn enable() -> RawMode {
        let fd = libc::STDIN_FILENO;
        // SAFETY: isatty / tcgetattr / cfmakeraw / tcsetattr on a valid fd; all
        // return codes are checked, and the original termios is restored on Drop.
        unsafe {
            if libc::isatty(fd) == 0 {
                return RawMode { fd, orig: None };
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return RawMode { fd, orig: None };
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            let _ = libc::tcsetattr(fd, libc::TCSANOW, &t);
            RawMode {
                fd,
                orig: Some(orig),
            }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(orig) = self.orig {
            // SAFETY: restoring the saved termios on the same fd.
            unsafe {
                let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &orig);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_detects_lead_then_follow() {
        let mut d = SentinelDetector::with_bytes(0x1C, b'd');
        let (out, det) = d.feed(b"hello\x1cd");
        assert_eq!(out, b"hello");
        assert!(det);
    }

    #[test]
    fn sentinel_split_across_reads() {
        let mut d = SentinelDetector::with_bytes(0x1C, b'd');
        let (out1, det1) = d.feed(b"abc\x1c");
        assert_eq!(out1, b"abc"); // lead held, not forwarded
        assert!(!det1);
        let (out2, det2) = d.feed(b"d");
        assert_eq!(out2, b"");
        assert!(det2);
    }

    #[test]
    fn sentinel_lead_not_followed_is_forwarded() {
        let mut d = SentinelDetector::with_bytes(0x1C, b'd');
        // Ctrl-\ then 'x' is NOT the sentinel: both bytes are forwarded.
        let (out, det) = d.feed(b"\x1cx");
        assert_eq!(out, b"\x1cx");
        assert!(!det);
    }

    #[test]
    fn sentinel_double_lead_holds_one() {
        let mut d = SentinelDetector::with_bytes(0x1C, b'd');
        let (out, det) = d.feed(b"\x1c\x1c");
        assert_eq!(out, b"\x1c"); // first lead emitted, second held
        assert!(!det);
        let (out2, det2) = d.feed(b"d");
        assert_eq!(out2, b"");
        assert!(det2);
    }

    #[test]
    fn step_buffer_per_line_emits_on_newline() {
        let mut s = StepBuffer::new(false);
        assert!(s.push(b"ls -la").is_empty());
        assert_eq!(s.pending_preview(), b"ls -la");
        let units = s.push(b"\n");
        assert_eq!(units, vec![b"ls -la\n".to_vec()]);
        assert_eq!(s.pending_preview(), b"");
    }

    #[test]
    fn step_buffer_per_line_handles_multiple_lines() {
        let mut s = StepBuffer::new(false);
        let units = s.push(b"a\nb\n");
        assert_eq!(units, vec![b"a\n".to_vec(), b"b\n".to_vec()]);
    }

    #[test]
    fn step_buffer_per_byte_emits_each() {
        let mut s = StepBuffer::new(true);
        let units = s.push(b"hi");
        assert_eq!(units, vec![b"h".to_vec(), b"i".to_vec()]);
    }
}
