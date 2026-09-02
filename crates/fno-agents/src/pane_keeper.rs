//! The pane keeper: a per-worker process that owns a hosted child's pty
//! master and outlives the mux server. A pane is its lane A (`--pane`, the
//! mux server's spelling); a pane-less lane-B thread is this same keeper
//! launched `--keeper` with no pane behind it.
//!
//! A pane child spawned directly by the mux server carries the pty slave as
//! its controlling terminal while the SERVER holds the master. When the
//! server exits, its master fds close and the kernel SIGHUPs the child's
//! foreground process group - a terminal hangup, not a kill. This module is
//! the un-retirement of the lane the deleted daemon worker.rs named as its
//! own undo ("supervisor fd-keeper"): the keeper `setsid()`s out of the
//! server's process group, opens the pty pair itself, spawns the provider
//! argv on the slave, and serves the mux server over a single-client unix
//! socket. The server becomes a viewer of panes; the keeper keeps. A mux
//! restart re-adopts the SAME child through the socket instead of spawning
//! a replacement.
//!
//! Shape (deliberately tiny and single-client, like the deleted lane):
//! no tokio. Two blocking threads (pty reader, socket accept) plus the main
//! thread waiting on the child. The pty master never crosses a thread
//! boundary: master reads live on the pty-reader thread's cloned reader,
//! writes and resizes take the master through its `Mutex` on the socket
//! thread.
//!
//! Frame protocol (mirrored by the client in `crates/fno/src/pty.rs`; the
//! two sides ship from this repo but a server restart after an upgrade can
//! meet an older keeper, so [`PROTOCOL_VERSION`] rides the Identify reply
//! and a mismatch is a loud refusal, never garbage bytes):
//! `u8 tag | u32 LE payload length | payload`.
//! Client -> keeper: `Input(bytes)`, `Resize(rows u16, cols u16)`, `Kill`,
//! `Identify`. Keeper -> client: `IdentifyReply(json)`, `Output(bytes)`,
//! `Exited(i32 LE)`.

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The keeper frame protocol version. Bump on any frame-shape change; a
/// client reading a NEWER version than it speaks refuses the keeper.
pub const PROTOCOL_VERSION: u32 = 1;

/// Bounded recent-output retention when `--ring-bytes` is not given. A
/// re-adopting server replays this, so it bounds how much of the detached
/// window a pane loses; 1 MiB is a generous scrollback sliver at 80x24.
pub const DEFAULT_RING_BYTES: usize = 1024 * 1024;

// Frame tags. Mirrored by the client side in crates/fno/src/pty.rs.
pub(crate) const TAG_INPUT: u8 = 1;
pub(crate) const TAG_RESIZE: u8 = 2;
pub(crate) const TAG_KILL: u8 = 3;
pub(crate) const TAG_IDENTIFY: u8 = 4;
pub(crate) const TAG_IDENTIFY_REPLY: u8 = 5;
pub(crate) const TAG_OUTPUT: u8 = 6;
pub(crate) const TAG_EXITED: u8 = 7;

/// One keeper-protocol frame.
#[derive(Debug, PartialEq)]
pub enum Frame {
    Input(Vec<u8>),
    Resize(u16, u16),
    Kill,
    Identify,
    IdentifyReply(Vec<u8>),
    Output(Vec<u8>),
    Exited(i32),
}

/// Encode one frame: tag byte + u32 LE length + payload.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut owned: Vec<u8> = Vec::new();
    let (tag, payload): (u8, &[u8]) = match frame {
        Frame::Input(bytes) => (TAG_INPUT, bytes),
        Frame::Resize(rows, cols) => {
            owned.extend_from_slice(&rows.to_le_bytes());
            owned.extend_from_slice(&cols.to_le_bytes());
            (TAG_RESIZE, &owned)
        }
        Frame::Kill => (TAG_KILL, &[]),
        Frame::Identify => (TAG_IDENTIFY, &[]),
        Frame::IdentifyReply(json) => (TAG_IDENTIFY_REPLY, json),
        Frame::Output(bytes) => (TAG_OUTPUT, bytes),
        Frame::Exited(code) => {
            owned.extend_from_slice(&code.to_le_bytes());
            (TAG_EXITED, &owned)
        }
    };
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// How much of `buf` a complete frame needs, and the decode result.
#[derive(Debug)]
pub enum Decode {
    /// A whole frame, and how many bytes it consumed.
    Frame(Frame, usize),
    /// The buffer holds a valid prefix; read more.
    NeedMore,
    /// The buffer holds a whole frame this protocol does not carry.
    Violation(String),
}

/// Decode the first frame in `buf`.
pub fn decode(buf: &[u8]) -> Decode {
    if buf.len() < 5 {
        return Decode::NeedMore;
    }
    let tag = buf[0];
    let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return Decode::NeedMore;
    }
    let payload = &buf[5..5 + len];
    let frame = match tag {
        TAG_INPUT => Frame::Input(payload.to_vec()),
        TAG_RESIZE if payload.len() == 4 => Frame::Resize(
            u16::from_le_bytes([payload[0], payload[1]]),
            u16::from_le_bytes([payload[2], payload[3]]),
        ),
        TAG_KILL => Frame::Kill,
        TAG_IDENTIFY => Frame::Identify,
        TAG_IDENTIFY_REPLY => Frame::IdentifyReply(payload.to_vec()),
        TAG_OUTPUT => Frame::Output(payload.to_vec()),
        TAG_EXITED if payload.len() == 4 => Frame::Exited(i32::from_le_bytes([
            payload[0], payload[1], payload[2], payload[3],
        ])),
        _ => {
            return Decode::Violation(format!(
                "frame tag {tag} with {len} payload byte(s) is not a keeper frame"
            ))
        }
    };
    Decode::Frame(frame, 5 + len)
}

/// The pty reader's retention: bounded recent output plus a count of the
/// bytes a re-adopting server will NOT see (stated at replay, never silent).
struct Ring {
    bytes: VecDeque<u8>,
    cap: usize,
    dropped: u64,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap: cap.max(1),
            dropped: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= self.cap {
            // One chunk bigger than the whole ring: keep its tail.
            self.dropped += self.bytes.len() as u64 + (chunk.len() - self.cap) as u64;
            self.bytes.clear();
            self.bytes.extend(&chunk[chunk.len() - self.cap..]);
            return;
        }
        let overflow = (self.bytes.len() + chunk.len()).saturating_sub(self.cap);
        self.dropped += overflow as u64;
        for _ in 0..overflow {
            self.bytes.pop_front();
        }
        self.bytes.extend(chunk);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

/// Parsed `--pane` lane argv.
pub struct KeeperConfig {
    pub sock: PathBuf,
    pub session: String,
    pub pane_key: String,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    pub ring_bytes: usize,
    pub argv: Vec<String>,
}

/// Parse the `--keeper` lane argv (`--pane` is the alias the mux server's
/// call sites spell it by; a lane-B thread spawn writes `--keeper`). The
/// shape: `--keeper --sock <path> --session <name> --pane-key <id>
/// --cwd <dir> [--rows N] [--cols N] [--ring-bytes N] -- <provider argv...>`.
pub fn parse_pane_args(args: &[String]) -> Result<KeeperConfig, String> {
    let mut sock: Option<String> = None;
    let mut session: Option<String> = None;
    let mut pane_key: Option<String> = None;
    let mut cwd = std::env::temp_dir();
    let mut rows = 24u16;
    let mut cols = 80u16;
    let mut ring_bytes = DEFAULT_RING_BYTES;
    let mut argv: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--keeper" | "--pane" => {}
            "--sock" => sock = Some(it.next().ok_or("--sock needs a value")?.clone()),
            "--session" => session = Some(it.next().ok_or("--session needs a value")?.clone()),
            "--pane-key" => pane_key = Some(it.next().ok_or("--pane-key needs a value")?.clone()),
            "--cwd" => cwd = PathBuf::from(it.next().ok_or("--cwd needs a value")?),
            "--rows" => {
                rows = it
                    .next()
                    .ok_or("--rows needs a value")?
                    .parse()
                    .map_err(|_| "--rows needs a number")?;
            }
            "--cols" => {
                cols = it
                    .next()
                    .ok_or("--cols needs a value")?
                    .parse()
                    .map_err(|_| "--cols needs a number")?;
            }
            "--ring-bytes" => {
                ring_bytes = it
                    .next()
                    .ok_or("--ring-bytes needs a value")?
                    .parse()
                    .map_err(|_| "--ring-bytes needs a number")?;
            }
            "--" => {
                argv.extend(it.cloned());
                break;
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    if argv.is_empty() {
        return Err("missing provider argv after `--`".into());
    }
    Ok(KeeperConfig {
        sock: PathBuf::from(sock.ok_or("missing --sock")?),
        session: session.ok_or("missing --session")?,
        pane_key: pane_key.ok_or("missing --pane-key")?,
        cwd,
        rows,
        cols,
        ring_bytes: ring_bytes.max(1024),
        argv,
    })
}

/// The shared keeper state the threads touch.
struct Keeper {
    ring: Mutex<Ring>,
    /// The SUBSCRIBER: the one client the pty's output streams to and the
    /// one whose Input/Resize/Kill frames drive the child. The first live
    /// client takes it (the mux server, at spawn or adoption); a later
    /// client (the `keeper list` probe) is served Identify but never steals
    /// the stream. `gen` is that client's accept generation.
    client: Mutex<Option<(u64, std::os::unix::net::UnixStream)>>,
    /// Bumped per accept so a departing old client cannot clear a newer
    /// subscriber's slot.
    client_gen: AtomicU64,
    /// The IdentifyReply body (version, both pids, argv, cwd, age), encoded
    /// per Identify so the answer can carry whether THIS connection holds
    /// the subscriber seat - a re-adopting server that loses the seat race
    /// must know it is a probe, or it wires a pane that never receives
    /// output and never gets input honored.
    identify: OnceLock<serde_json::Value>,
    /// The child's pid: the Kill frame's target and the Identify reply's
    /// answer. The keeper owns the master; the CHILD is the process every
    /// fleet count and later kill must aim at.
    child_pid: AtomicU32,
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// The pty input writer, taken ONCE at startup and reused for every
    /// Input frame. portable-pty's `take_writer` is take-once by contract:
    /// its writer sends EOT (a literal Ctrl-D) when dropped, so a
    /// write-and-drop per frame would end the child's stdin after the first
    /// keystroke, and a second `take_writer` refuses outright.
    input: Mutex<Option<Box<dyn Write + Send>>>,
}

impl Keeper {
    /// Send one frame to the subscriber, if any. The lock-held write
    /// serializes the pty-reader thread and any subscriber-side reply.
    fn send(&self, frame: &Frame) {
        let encoded = encode(frame);
        self.send_raw(&encoded);
    }

    fn send_raw(&self, encoded: &[u8]) {
        let mut guard = self.client.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, stream)) = guard.as_mut() {
            let _ = stream.write_all(encoded);
            let _ = stream.flush();
        }
    }
}

/// The harness session id out of the provider argv, when the argv carries
/// one. A lane-B harness's rendered create form rides the id fno minted
/// BEFORE launch (`pi --session-id <id>`), so this is a read of state the
/// keeper already holds, not a second field to keep in sync. `None` when
/// the argv names no id - never a guess.
/// A full UUID: 36 chars with dashes at 8, 13, 18 and 23. The shape check is
/// the point: a bare `--resume` opens a picker and a truncated handle
/// addresses nothing, and neither may ever be read as an identity.
fn is_full_uuid(value: &str) -> bool {
    value.len() == 36
        && value.as_bytes()[8] == b'-'
        && value.as_bytes()[13] == b'-'
        && value.as_bytes()[18] == b'-'
        && value.as_bytes()[23] == b'-'
}

fn session_id_from_argv(argv: &[String]) -> Option<String> {
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        if arg == "--session-id" {
            return it
                .next()
                .map(String::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        if let Some(value) = arg.strip_prefix("--session-id=") {
            return (!value.is_empty()).then(|| value.to_string());
        }
        // cursor-agent's create form (callee-minted-read-back): the chat id
        // rides `--resume <uuid>`. The UUID-shape filter keeps a bare
        // `--resume` (an interactive picker) and a truncated handle answering
        // None, never a guess.
        if arg == "--resume" {
            return it
                .next()
                .map(String::as_str)
                .filter(|s| is_full_uuid(s))
                .map(str::to_string);
        }
    }
    None
}

/// Run the keeper lane to completion. Only returns on a startup failure
/// (the caller prints and exits nonzero); a normal lifecycle ends inside
/// with `std::process::exit` after the child exits.
pub fn run(cfg: KeeperConfig) -> Result<(), String> {
    // Step 1: our own session, so a group-kill aimed at the mux server's
    // process group never reaches us. The SIGHUP ignore in the binary's
    // main() covers the hangup path; setsid covers the group-kill path.
    // SAFETY: setsid is a single syscall with no preconditions beyond
    // single-threadedness, and this runs before any thread exists.
    unsafe {
        libc::setsid();
    }

    // The socket must not already be served by a live keeper: connecting
    // succeeds only then. Connect-before-bind makes a double-keeper a loud
    // refusal instead of a silently stolen socket.
    if std::os::unix::net::UnixStream::connect(&cfg.sock).is_ok() {
        return Err(format!(
            "keeper socket {} already has a live listener behind it",
            cfg.sock.display()
        ));
    }
    // A file at the path with nobody behind it is a dead keeper's leftover:
    // unlink and rebind.
    let _ = std::fs::remove_file(&cfg.sock);
    if let Some(parent) = cfg.sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let listener = UnixListener::bind(&cfg.sock)
        .map_err(|e| format!("cannot bind keeper socket {}: {e}", cfg.sock.display()))?;

    // Step 2: the pty pair. The keeper is now the master holder.
    let size = PtySize {
        rows: cfg.rows.max(1),
        cols: cfg.cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|e| format!("openpty: {e}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("pty reader: {e}"))?;

    // Step 3: the provider argv on the slave. Env mirrors the mux's
    // base_command: stated colors (a pane host IS a terminal), the session
    // and pane identity a hosted agent names itself by.
    let mut cmd = CommandBuilder::new(&cfg.argv[0]);
    for a in &cfg.argv[1..] {
        cmd.arg(a);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("NO_COLOR");
    cmd.env("FNO_SESSION", &cfg.session);
    cmd.env("FNO_PANE", &cfg.pane_key);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    cmd.env("FNO_PANE_EPOCH", epoch.to_string());
    if cfg.cwd.is_dir() {
        cmd.cwd(&cfg.cwd);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {}: {e}", cfg.argv[0]))?;
    drop(pair.slave);
    // Take the single writer NOW: portable-pty allows exactly one
    // `take_writer` per master, and the Input arm reuses it for the pane's
    // whole life.
    let input = pair
        .master
        .take_writer()
        .map_err(|e| format!("pty writer: {e}"))?;
    let child_pid = child.process_id().unwrap_or(0);
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let keeper = Arc::new(Keeper {
        ring: Mutex::new(Ring::new(cfg.ring_bytes)),
        client: Mutex::new(None),
        client_gen: AtomicU64::new(0),
        identify: OnceLock::new(),
        child_pid: AtomicU32::new(child_pid),
        master: Mutex::new(pair.master),
        input: Mutex::new(Some(input)),
    });
    let identify = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "keeper_pid": std::process::id(),
        "child_pid": child_pid,
        "argv": cfg.argv,
        "cwd": cfg.cwd.to_string_lossy(),
        "rows": cfg.rows,
        "cols": cfg.cols,
        "started_at": started_at,
        "session_id": session_id_from_argv(&cfg.argv),
    });
    let _ = keeper.identify.set(identify);

    // The pty-reader thread: master -> ring + Output frames. Blocking reads
    // live here and only here; EOF/EIO means the child is gone.
    let pty_keeper = Arc::clone(&keeper);
    std::thread::Builder::new()
        .name("fno-keeper-pty".into())
        .spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        pty_keeper
                            .ring
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(&chunk);
                        pty_keeper.send(&Frame::Output(chunk));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("pty reader thread: {e}"))?;

    // The accept loop: every connection is served on its own thread, so a
    // `keeper list` probe is answered even while the server holds the
    // subscriber seat. On a subscriber's disconnect the keeper keeps
    // running (AC1-HP); the next subscriber is the server that re-adopts.
    {
        let keeper = Arc::clone(&keeper);
        std::thread::Builder::new()
            .name("fno-keeper-sock".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let gen = keeper.client_gen.fetch_add(1, Ordering::SeqCst) + 1;
                    let keeper = Arc::clone(&keeper);
                    std::thread::Builder::new()
                        .name("fno-keeper-cli".into())
                        .spawn(move || {
                            // First come, first seated: an empty subscriber
                            // slot takes THIS connection.
                            let mut slot = keeper.client.lock().unwrap_or_else(|e| e.into_inner());
                            let is_subscriber = slot.is_none();
                            if is_subscriber {
                                *slot =
                                    Some((gen, stream.try_clone().expect("clone keeper client")));
                            }
                            drop(slot);
                            serve_client(&keeper, stream, gen, is_subscriber);
                        })
                        .expect("spawn keeper client thread");
                }
            })
            .map_err(|e| format!("accept thread: {e}"))?;
    }

    // The main thread waits on the child (AC5-ERR): on exit, the exit frame,
    // unlink, exit. No client is required - a child that dies while nobody
    // watches still cleans its socket.
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    // portable_pty's ExitStatus carries the raw code; a signal death reads
    // as a nonzero code on unix. The frame is informational - the pane's own
    // exit path is the authoritative fact.
    let code = status.exit_code() as i32;
    keeper.send(&Frame::Exited(code));
    let _ = std::fs::remove_file(&cfg.sock);
    std::process::exit(code);
}

/// Serve one accepted connection to completion, then drop it. Only the
/// subscriber drives the child; every connection may Identify.
fn serve_client(
    keeper: &Keeper,
    mut stream: std::os::unix::net::UnixStream,
    gen: u64,
    is_subscriber: bool,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut read_buf = [0u8; 8192];
    loop {
        // Drain whole frames already buffered before blocking on the socket.
        loop {
            match decode(&buf) {
                Decode::NeedMore => break,
                Decode::Violation(reason) => {
                    eprintln!("fno-agents-worker: keeper client: {reason}; dropping it");
                    return;
                }
                Decode::Frame(frame, used) => {
                    buf.drain(..used);
                    // Only the subscriber drives the child; a probe's
                    // driving frames are ignored, never honored.
                    if !is_subscriber
                        && matches!(frame, Frame::Input(_) | Frame::Resize(_, _) | Frame::Kill)
                    {
                        continue;
                    }
                    match frame {
                        Frame::Input(bytes) => {
                            // A full kernel input buffer blocks here, which
                            // backpressures the client - bounded, never a
                            // queue that grows.
                            let mut writer = keeper.input.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(writer) = writer.as_mut() {
                                let _ = writer.write_all(&bytes);
                                let _ = writer.flush();
                            }
                        }
                        Frame::Resize(rows, cols) => {
                            let master = keeper.master.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = master.resize(PtySize {
                                rows: rows.max(1),
                                cols: cols.max(1),
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                        Frame::Kill => {
                            // A deliberate close kills the CHILD (the keeper
                            // itself dies right after it, through the main
                            // thread's wait). The pid is the real process:
                            // the child handle stays owned by the waiter.
                            let pid = keeper.child_pid.load(Ordering::SeqCst);
                            if pid > 0 {
                                // SAFETY: kill with a valid pid and SIGKILL
                                // is the deliberate-close contract.
                                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                            }
                        }
                        Frame::Identify => {
                            // The reply goes to the ASKING connection. For
                            // the subscriber that write must serialize with
                            // the pty reader's Output writes (shared slot
                            // lock); a probe's stream has a single writer.
                            let mut reply_on = |encoded: &[u8]| {
                                if is_subscriber {
                                    keeper.send_raw(encoded);
                                } else {
                                    let _ = stream.write_all(encoded);
                                    let _ = stream.flush();
                                }
                            };
                            if let Some(value) = keeper.identify.get() {
                                let mut value = value.clone();
                                value["subscriber"] = serde_json::json!(is_subscriber);
                                reply_on(&encode(&Frame::IdentifyReply(
                                    value.to_string().into_bytes(),
                                )));
                            }
                            // Replay the retained window (AC3-HP), with the
                            // drop stated rather than silent.
                            let (snapshot, dropped) = {
                                let mut ring =
                                    keeper.ring.lock().unwrap_or_else(|e| e.into_inner());
                                (ring.snapshot(), std::mem::take(&mut ring.dropped))
                            };
                            if dropped > 0 {
                                reply_on(&encode(&Frame::Output(
                                    format!(
                                        "\r\n[keeper: {dropped} byte(s) of earlier output \
                                             fell off the ring]\r\n"
                                    )
                                    .into_bytes(),
                                )));
                            }
                            if !snapshot.is_empty() {
                                reply_on(&encode(&Frame::Output(snapshot)));
                            }
                        }
                        // Frames only the keeper sends arriving from a
                        // client are the peer talking to itself; ignore.
                        _ => {}
                    }
                }
            }
        }
        match stream.read(&mut read_buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&read_buf[..n]),
        }
    }
    // Only clear OUR seat: a newer subscriber may already hold it.
    if is_subscriber {
        let mut client = keeper.client.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seated_gen, _)) = client.as_ref() {
            if *seated_gen == gen {
                *client = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip_through_encode_decode() {
        for frame in [
            Frame::Input(b"keystroke".to_vec()),
            Frame::Resize(40, 120),
            Frame::Kill,
            Frame::Identify,
            Frame::IdentifyReply(br#"{"v":1}"#.to_vec()),
            Frame::Output(vec![0u8, 1, 2, 255]),
            Frame::Exited(-1),
        ] {
            let encoded = encode(&frame);
            match decode(&encoded) {
                Decode::Frame(decoded, used) => {
                    assert_eq!(used, encoded.len(), "one frame consumes exactly its bytes");
                    let matches = match (&frame, &decoded) {
                        (Frame::Input(a), Frame::Input(b)) => a == b,
                        (Frame::Resize(ar, ac), Frame::Resize(br, bc)) => ar == br && ac == bc,
                        (Frame::Kill, Frame::Kill) => true,
                        (Frame::Identify, Frame::Identify) => true,
                        (Frame::IdentifyReply(a), Frame::IdentifyReply(b)) => a == b,
                        (Frame::Output(a), Frame::Output(b)) => a == b,
                        (Frame::Exited(a), Frame::Exited(b)) => a == b,
                        _ => false,
                    };
                    assert!(matches, "{frame:?} must round-trip, got {decoded:?}");
                }
                other => panic!("a whole frame must decode, got {other:?}"),
            }
        }
    }

    #[test]
    fn partial_frames_need_more_bytes() {
        let encoded = encode(&Frame::Input(vec![7u8; 100]));
        for cut in [0usize, 1, 3, 5, 5 + 50] {
            assert!(
                matches!(decode(&encoded[..cut]), Decode::NeedMore),
                "a {cut}-byte prefix of a {}-byte frame is not whole",
                encoded.len()
            );
        }
    }

    #[test]
    fn unknown_tags_are_violations_not_silent_drops() {
        let mut garbage = encode(&Frame::Kill);
        garbage[0] = 99;
        match decode(&garbage) {
            Decode::Violation(_) => {}
            other => panic!("tag 99 must read as a violation, got {other:?}"),
        }
    }

    #[test]
    fn ring_keeps_only_the_cap_and_counts_the_rest() {
        let mut ring = Ring::new(10);
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"0123456789");
        assert_eq!(ring.dropped, 0, "nothing fell off yet");
        ring.push(b"abc");
        assert_eq!(ring.snapshot(), b"3456789abc");
        assert_eq!(ring.dropped, 3, "three bytes stated as dropped, not silent");
        // One chunk bigger than the whole ring keeps its tail.
        ring.push(b"WXYZ0123456789");
        assert_eq!(ring.snapshot(), b"0123456789");
        assert_eq!(
            ring.dropped, 17,
            "3 old + 4 overflow + 10 displaced, all counted"
        );
    }

    #[test]
    fn keeper_lane_parse_accepts_both_spellings() {
        let base = [
            "--sock",
            "/tmp/k.sock",
            "--session",
            "t",
            "--pane-key",
            "3",
            "--cwd",
            "/tmp",
            "--",
            "sleep",
            "1",
        ];
        let with = |lane: &str| {
            let mut argv: Vec<String> = vec![lane.to_string()];
            argv.extend(base.iter().map(|s| s.to_string()));
            parse_pane_args(&argv)
        };
        let alias = with("--keeper").expect("--keeper parses");
        let canonical = with("--pane").expect("--pane parses");
        assert_eq!(alias.sock, canonical.sock);
        assert_eq!(alias.session, canonical.session);
        assert_eq!(alias.pane_key, canonical.pane_key);
        assert_eq!(alias.cwd, canonical.cwd);
        assert_eq!(alias.argv, canonical.argv);
    }

    #[test]
    fn keeper_lane_session_id_is_read_off_the_argv_never_guessed() {
        let pi_shape = ["pi", "--session-id", "sid-1", "-m", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            session_id_from_argv(&pi_shape),
            Some("sid-1".to_string()),
            "the lane-B create form rides the argv"
        );
        let equals_shape = ["pi", "--session-id=sid-2"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            session_id_from_argv(&equals_shape),
            Some("sid-2".to_string())
        );
        let no_id = ["sleep", "60"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            session_id_from_argv(&no_id),
            None,
            "an argv with no id answers None, never a guess"
        );
        let empty_value = ["pi", "--session-id", ""]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(session_id_from_argv(&empty_value), None);
    }

    #[test]
    fn keeper_lane_reads_the_cursor_resume_shape_as_a_full_uuid_only() {
        let cursor_shape = ["cursor-agent", "--resume", "0f9e63ed-861d-4f9f-8efa-3e40c5e01266", "--trust"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            session_id_from_argv(&cursor_shape),
            Some("0f9e63ed-861d-4f9f-8efa-3e40c5e01266".to_string()),
            "the callee-minted create form rides --resume"
        );
        // A bare --resume opens an interactive picker: no identity.
        let picker = ["cursor-agent", "--resume"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(session_id_from_argv(&picker), None);
        // A head-8 is an fno session handle, not a chat id: no identity.
        let truncated = ["cursor-agent", "--resume", "74db359a"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(session_id_from_argv(&truncated), None);
    }
}
