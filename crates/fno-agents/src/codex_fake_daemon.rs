//! A fake shared codex app-server daemon, for tests.
//!
//! The codex thread lane is a WebSocket client of one long-lived daemon that
//! owns the threads. Testing it needs a server on a unix socket, not a child
//! on stdin/stdout, and a test that still spoke the stdio protocol would keep
//! passing against a driver that had quietly gone back to forking its own
//! app-server. That is the regression this module exists to make impossible.
//!
//! It replaces six hand-written Python fakes that each re-implemented the same
//! protocol with a different scenario hardcoded. They had already drifted:
//! one of them exited the process on interrupt, which no real app-server does.
//! One implementation, knobs for the scenarios.
//!
//! `#[doc(hidden)]` and test-only by intent. It is compiled into the library
//! rather than a `tests/` helper because the in-crate daemon tests and the
//! integration tests both need it, and only a library item reaches both.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::Message;

/// Makes each fake home unique within a test binary.
static NEXT_HOME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What the fake does with a `turn/steer` arriving mid-turn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Steer {
    /// Ack into the SAME turn, at most once per turn.
    AckOnce,
    /// Ack into the same turn every time.
    AckAlways,
    /// Fail the `expectedTurnId` precondition once, the way the real
    /// app-server does when the turn completed inside the race window.
    FailPreconditionOnce,
}

/// What the fake does with a `turn/interrupt` arriving mid-turn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// Ack after `delay`, then complete the turn as `interrupted`.
    AckThenComplete(Duration),
    /// Ack after `delay` and never complete: the wedge that once let the
    /// settle wait outlive the daemon's outer stop bound.
    AckOnly(Duration),
    /// Refuse, the way the real app-server refuses an unknown turn.
    Refuse,
}

/// One fake daemon's behavior.
#[derive(Clone)]
pub struct Behavior {
    /// The id every `thread/start` and `thread/resume` answers with.
    pub thread_id: String,
    /// How long a turn runs before completing on its own.
    pub turn_duration: Duration,
    pub steer: Steer,
    pub interrupt: Interrupt,
    /// Notification frames streamed at the head of a turn. Exercises the fact
    /// that the whole-turn deadline, not a frame count, is the only bound.
    pub event_frames: usize,
    /// Emit a `turn/completed` for a turn NOBODY drives, this far into a turn.
    /// The stray-completion wedge.
    pub stray_completion_after: Option<Duration>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            thread_id: "thread-t".to_string(),
            turn_duration: Duration::from_millis(1200),
            steer: Steer::AckOnce,
            interrupt: Interrupt::Refuse,
            event_frames: 0,
            stray_completion_after: None,
        }
    }
}

impl Behavior {
    /// Turns complete 1.2s in; one steer per turn acks into the same turn; an
    /// interrupt is refused.
    pub fn quick() -> Self {
        Self::default()
    }

    /// Turns run 30s; an interrupt acks and completes the turn as
    /// `interrupted`.
    pub fn long() -> Self {
        Self {
            turn_duration: Duration::from_secs(30),
            steer: Steer::AckAlways,
            interrupt: Interrupt::AckThenComplete(Duration::ZERO),
            ..Self::default()
        }
    }

    pub fn with_thread_id(mut self, id: &str) -> Self {
        self.thread_id = id.to_string();
        self
    }

    pub fn with_turn_duration(mut self, duration: Duration) -> Self {
        self.turn_duration = duration;
        self
    }

    pub fn with_steer(mut self, steer: Steer) -> Self {
        self.steer = steer;
        self
    }

    pub fn with_interrupt(mut self, interrupt: Interrupt) -> Self {
        self.interrupt = interrupt;
        self
    }

    pub fn with_event_frames(mut self, frames: usize) -> Self {
        self.event_frames = frames;
        self
    }

    pub fn with_stray_completion_after(mut self, after: Duration) -> Self {
        self.stray_completion_after = Some(after);
        self
    }
}

/// A running fake daemon. `CODEX_HOME` points at it for the guard's lifetime,
/// so [`crate::codex_inject::ensure_codex_daemon`] and
/// [`crate::codex_inject::codex_app_server_socket_path`] resolve here.
///
/// Callers hold whatever process-wide env lock their test binary already
/// uses: `CODEX_HOME` is global, so two of these must never overlap.
pub struct FakeDaemon {
    home: PathBuf,
    saved_home: Option<std::ffi::OsString>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    /// Bind a control socket under a temporary `CODEX_HOME` and start serving.
    ///
    /// The daemon state file is stamped with THIS process, so the liveness
    /// check `ensure_codex_daemon` runs passes without a second process to
    /// start and reap. Its pid is therefore the pid the driver records, which
    /// is what the ownership assertions read.
    pub fn start(behavior: Behavior) -> Self {
        // A hand-rolled temp dir, not `tempfile`: that crate is a dev
        // dependency and this module compiles into the library.
        //
        // Rooted at `/tmp` rather than `std::env::temp_dir()`, which on macOS
        // is a ~50-character `/var/folders/...` path. A unix socket path is
        // capped at `SUN_LEN` (104 bytes here), and the control socket adds 42
        // characters of its own, so the platform temp dir overruns the bind.
        let home = PathBuf::from("/tmp").join(format!(
            "fno-fk-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let control = home.join("app-server-control");
        let state_dir = home.join("app-server-daemon");
        std::fs::create_dir_all(&control).expect("control dir");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let pid = std::process::id();
        let start = crate::daemon::process_start_time(pid).expect("own process start time");
        std::fs::write(
            state_dir.join("app-server.pid"),
            json!({"pid": pid, "processStartToken": start}).to_string(),
        )
        .expect("write daemon state");
        let listener =
            UnixListener::bind(control.join("app-server-control.sock")).expect("bind control sock");
        let saved_home = std::env::var_os("CODEX_HOME");
        std::env::set_var("CODEX_HOME", &home);
        let server = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                tokio::spawn(serve(conn, behavior.clone()));
            }
        });
        Self {
            home,
            saved_home,
            server,
        }
    }

    /// The control socket this fake listens on.
    pub fn socket_path(&self) -> PathBuf {
        self.home
            .join("app-server-control")
            .join("app-server-control.sock")
    }

    /// The pid recorded as the serving app-server, which is this process.
    pub fn recorded_pid(&self) -> u32 {
        std::process::id()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.server.abort();
        match self.saved_home.take() {
            Some(path) => std::env::set_var("CODEX_HOME", path),
            None => std::env::remove_var("CODEX_HOME"),
        }
        std::fs::remove_dir_all(&self.home).ok();
    }
}

/// Serve ONE client connection.
///
/// The read loop and the turn timer share one `select!` so a steer or an
/// interrupt is answered WHILE a turn is pending. A sequential fake that
/// sleeps inside its turn branch leaves the interrupt unread, which is not
/// how the real app-server behaves and is why the old Python fakes each
/// carried a reader thread.
async fn serve(conn: UnixStream, behavior: Behavior) {
    let Ok(ws) = tokio_tungstenite::accept_async(conn).await else {
        return;
    };
    let (mut sink, mut stream) = ws.split();
    let mut turn_n = 0u32;
    let mut pending: Option<(String, tokio::time::Instant)> = None;
    let mut stray: Option<tokio::time::Instant> = None;
    let mut steered = false;
    loop {
        let due = match (pending.as_ref(), stray) {
            (Some((_, turn_at)), Some(stray_at)) => Some(stray_at.min(*turn_at)),
            (Some((_, turn_at)), None) => Some(*turn_at),
            (None, _) => None,
        };
        let timer = async move {
            match due {
                Some(when) => tokio::time::sleep_until(when).await,
                None => std::future::pending::<()>().await,
            }
        };
        let frame = tokio::select! {
            _ = timer => {
                // The stray fires first when it is due first, and never
                // consumes the real turn's completion.
                if let Some(stray_at) = stray {
                    if stray_at <= tokio::time::Instant::now() {
                        stray = None;
                        if send(&mut sink, completed("turn-stray", "completed", "STRAY")).await.is_err() {
                            return;
                        }
                        continue;
                    }
                }
                let Some((turn_id, _)) = pending.take() else { continue };
                let text = format!("REPLY-{turn_n}");
                if send(&mut sink, completed(&turn_id, "completed", &text)).await.is_err() {
                    return;
                }
                continue;
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => text.to_string(),
                // Control frames carry no protocol payload.
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return,
            },
        };
        let Ok(msg) = serde_json::from_str::<Value>(&frame) else {
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let reply = match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({"id": id, "result": {}}),
            Some("initialized") => continue,
            Some("thread/start") | Some("thread/resume") => json!({"id": id, "result": {
                "thread": {"id": behavior.thread_id, "path": "/tmp/fake-daemon-rollout.jsonl"}
            }}),
            Some("turn/start") => {
                turn_n += 1;
                steered = false;
                let turn_id = format!("turn-{turn_n}");
                let now = tokio::time::Instant::now();
                pending = Some((turn_id.clone(), now + behavior.turn_duration));
                stray = behavior.stray_completion_after.map(|after| now + after);
                if send(
                    &mut sink,
                    json!({"id": id, "result": {"turn": {"id": turn_id}}}),
                )
                .await
                .is_err()
                {
                    return;
                }
                for seq in 0..behavior.event_frames {
                    if send(
                        &mut sink,
                        json!({"method": "turn/event", "params": {"seq": seq}}),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                continue;
            }
            Some("turn/steer") => {
                let Some((turn_id, _)) = pending.as_ref() else {
                    continue;
                };
                match behavior.steer {
                    Steer::AckAlways => json!({"id": id, "result": {"turn": {"id": turn_id}}}),
                    Steer::AckOnce if !steered => {
                        steered = true;
                        json!({"id": id, "result": {"turn": {"id": turn_id}}})
                    }
                    Steer::FailPreconditionOnce if !steered => {
                        steered = true;
                        json!({"id": id, "error": {
                            "message": format!("{turn_id} is not active")
                        }})
                    }
                    _ => continue,
                }
            }
            Some("turn/interrupt") => match (pending.is_some(), behavior.interrupt) {
                (true, Interrupt::AckThenComplete(delay)) => {
                    tokio::time::sleep(delay).await;
                    let (turn_id, _) = pending.take().expect("just matched");
                    stray = None;
                    if send(&mut sink, json!({"id": id, "result": {}}))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    completed(&turn_id, "interrupted", "")
                }
                (true, Interrupt::AckOnly(delay)) => {
                    tokio::time::sleep(delay).await;
                    json!({"id": id, "result": {}})
                }
                _ => json!({"id": id, "error": {
                    "message": format!("turn-{turn_n} is not active")
                }}),
            },
            _ => continue,
        };
        if send(&mut sink, reply).await.is_err() {
            return;
        }
    }
}

fn completed(turn_id: &str, status: &str, text: &str) -> Value {
    let items = if text.is_empty() {
        json!([])
    } else {
        json!([{"type": "agentMessage", "text": text}])
    };
    json!({"method": "turn/completed", "params": {"turn": {
        "id": turn_id, "status": status, "items": items,
    }}})
}

async fn send(
    sink: &mut crate::codex_inject::AppServerSink,
    frame: Value,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    sink.send(Message::Text(frame.to_string().into())).await
}

/// A live control socket, or `None` with a printed skip line. Live tests that
/// need the operator's real daemon gate on this rather than failing on a
/// machine that has none.
pub fn live_socket_or_skip() -> Option<PathBuf> {
    let path = crate::codex_inject::codex_app_server_socket_path();
    if path.exists() {
        return Some(path);
    }
    eprintln!(
        "skip: no codex app-server daemon at {} (start one with `codex app-server daemon start`)",
        path.display()
    );
    None
}

/// Whether `path` is a socket this process can reach at all.
pub fn socket_exists(path: &Path) -> bool {
    path.exists()
}
